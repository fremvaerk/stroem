# Atomic Fail-or-Retry — Design

**Status:** Draft, revision 2 (2026-09-08, after Codex review)
**Date:** 2026-09-08
**Origin:** prerequisite of `2026-09-08-step-cascade-design.md` (Q28); first slice of the
architecture-review candidate "One interface for every step status transition".
**Ships:** on its own branch, before the step cascade. The cascade's activation commit
must not be deployed until every server replica runs this change (a mixed fleet would
still produce failed-then-reset rows).

## 1. Problem

The step-retry decision is made *after* the failure has been written. Every path that
fails a worker-executed, recovered, or rejected step first writes `status = 'failed'`
and then calls `job_recovery::orchestrate_after_step`, whose first act after loading the
job, workspace and task (`job_recovery.rs:148-185`) is to read the row back
(`:186-209`), decide whether a retry is owed, and if so `reset_for_retry` it to `ready`
with a `retry_at`. Between the two statements the row is `failed` and visible to every
concurrent reader:

- `skip_unreachable_steps` on another step's cascade skips its dependents;
- `check_loop_completion` on a sibling instance's completion fails the placeholder;
- after the step cascade lands, any cascade on the job would do both.

The window is milliseconds long but real, and no reader can tell "failed, about to be
reset" from "failed, final" by looking at the row.

Sites (every `orchestrate_after_step` call preceded by a failure write):

| Site | Writes failure via | When |
|---|---|---|
| `web/worker_api/jobs.rs:932` `complete_step` | `mark_failed` | worker reports failure |
| `web/worker_api/jobs.rs:383` | `mark_failed` | claim-time render/prepare failure of the action |
| `recovery.rs:100` phase 1 | `mark_failed` | stale worker |
| `recovery.rs:151` phase 2 | `mark_failed` | step timeout |
| `recovery.rs:200` phase 2.5 | `mark_failed` | suspended-step timeout |
| `recovery.rs:264` phase 4 | `mark_failed` | unmatched ready step |
| `web/api/jobs.rs:1073` `reject_step` | `JobStepRepo::reject_step` (guarded `status = 'suspended'`, returns `false` → 409) | approval rejected |

(`web/api/jobs.rs:1041` `approve_step` marks completed; unchanged.)

Failure paths that today do **not** retry and are **not** changed by this design:
`propagate_to_parent` (child job failed, `job_recovery.rs:575`), `fail_task_step`
(dispatch failure, `job_creator.rs:743`), approval dispatch/render failure
(`job_creator.rs:1435`, `:1473`), and cascade-generated failures (`when`/`for_each`
errors). They keep calling plain `mark_failed`.

## 2. Change

One repo function replaces the write-then-decide pair at the seven sites:

```rust
pub enum FailOutcome {
    /// Precondition not met (row not in one of `expected`). Nothing written.
    NotApplied,
    /// Retries exhausted or none configured. Row is `failed`.
    Failed { attempt: i32, max: Option<i32> },
    /// Row went straight to `ready` with `retry_at`; never observable as `failed`.
    RetryScheduled { attempt: i32, max: i32, delay_secs: u64 },
}

/// One transaction on one connection:
///   1. SELECT <STEP_COLUMNS> FROM job_step WHERE job_id=$1 AND step_name=$2 FOR UPDATE
///   2. if `expected` is non-empty and row.status ∉ expected → ROLLBACK, NotApplied
///   3. if max_retries = Some(m) and retry_attempt < m → RETRY assignment (below),
///      RetryScheduled { attempt: retry_attempt + 1, max: m, delay_secs }
///      else → FAIL assignment (below), Failed { attempt: retry_attempt, max: max_retries }
///   4. COMMIT
pub async fn fail_or_retry(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    error: &str,
    expected: &[StepStatus],                       // [] = no precondition (today's mark_failed)
    delay_for: impl FnOnce(&JobStepRow) -> u64,    // today's compute_retry_delay
) -> Result<FailOutcome>;
```

**FAIL assignment** (identical to today's `mark_failed`, `job_step.rs:494-512`):

```sql
UPDATE job_step
SET status = 'failed', error_message = $error, completed_at = NOW()
WHERE job_id = $1 AND step_name = $2
```

**RETRY assignment** — today's `reset_for_retry` (`job_step.rs:522-556`) rewritten to take
the incoming failure as parameters instead of reading it back from the row, since the
row was never marked failed:

```sql
UPDATE job_step
SET retry_history = retry_history || jsonb_build_array(jsonb_build_object(
        'attempt',    retry_attempt,        -- pre-increment, as today
        'error',      $error,               -- the incoming failure (today: error_message)
        'started_at', started_at,           -- this attempt's start, as today
        'failed_at',  NOW()                 -- this failure's time (today: completed_at)
    )),
    retry_attempt = retry_attempt + 1,
    status        = 'ready',
    ready_at      = NOW(),
    retry_at      = $retry_at,
    worker_id     = NULL,
    started_at    = NULL,
    completed_at  = NULL,
    error_message = NULL,
    output        = NULL,
    agent_state   = NULL,
    suspended_at  = NULL
WHERE job_id = $1 AND step_name = $2
```

Every column today's reset clears is cleared; the history entry carries the same four
keys with the same meanings. `retry_at = NOW() + delay` is computed in Rust from the
locked row via `delay_for`, as today via `compute_retry_delay`.

Callers, by site:

```rust
// complete_step / claim-time failure / recovery phases: no precondition
match JobStepRepo::fail_or_retry(pool, job_id, step, &err, &[], compute_retry_delay).await? {
    FailOutcome::RetryScheduled { attempt, max, delay_secs } => {
        // step_retry_message takes the PRE-increment attempt, as today (:199-205)
        state.append_server_log(job_id, &step_retry_message(step, attempt - 1, max, delay_secs)).await;
        // no orchestration: the step is ready again (today's early return, :208)
    }
    FailOutcome::Failed { attempt, max: Some(max) } => {
        state.append_server_log(job_id, &step_retries_exhausted_message(step, attempt, max)).await;
        orchestrate_after_step(state, job_id, step).await?;
    }
    FailOutcome::Failed { max: None, .. } => orchestrate_after_step(state, job_id, step).await?,
    FailOutcome::NotApplied => unreachable!("no precondition given"),
}

// reject_step: precondition preserved
match JobStepRepo::fail_or_retry(pool, job_id, step, &reason, &[StepStatus::Suspended], compute_retry_delay).await? {
    FailOutcome::NotApplied => return Err(AppError::Conflict("Step is no longer awaiting approval")),  // today's 409
    outcome => /* as above */,
}
```

`orchestrate_after_step`'s retry block (`:186-222`) is deleted; the function starts at
the job/workspace/task lookup and proceeds to loop completion. `reset_for_retry` and
`JobStepRepo::reject_step` are deleted (their only callers are replaced).
`compute_retry_delay`, `step_retry_message`, `step_retries_exhausted_message` are
unchanged (their unit test `retry_messages_count_executions_consistently` stays).

## 3. Semantics preserved, and the two that change

Preserved: which steps retry, how many times, backoff and jitter, the log lines and
their attempt arithmetic, what `retry_history` records, `retry_at` respected by claim,
`agent_state`/`output`/`worker_id` cleared on retry, hooks firing only after retries are
exhausted (terminal handling runs only after `Failed`), task-level retry (untouched),
restart/rerun lineage (untouched), the reject path's 409 when the step is no longer
suspended.

Changed, both by construction:

1. **No reader can ever see a `failed` row that will be retried.** Today's window
   (§1) is gone.
2. **Retry no longer depends on the workspace being loaded.** Today the retry check sits
   *after* `orchestrate_after_step`'s job/workspace/task lookup (`:148-185`), so if the
   workspace is temporarily unavailable the function returns early and the step stays
   `failed` with its retry budget unused. Now the decision is made at the failure write,
   before any lookup. A consequence: `retry_at` is anchored a few milliseconds earlier
   (at the write, not after the lookup). Neither is a regression; both are recorded in
   CLAUDE.md.

## 4. Tests

- DB (container): `fail_or_retry` with retries remaining → row `ready`, `retry_attempt`
  +1, `retry_at ≈ now + delay`, `retry_history` last entry `{attempt: old,
  error: <incoming>, started_at: <old started_at>, failed_at: ≈ now}`, and `worker_id`,
  `started_at`, `completed_at`, `error_message`, `output`, `agent_state`, `suspended_at`
  all `NULL`; with retries exhausted or none configured → row `failed`, `error_message`
  set, `completed_at` set, `retry_history` unchanged; with `expected = [suspended]` on a
  `running` row → `NotApplied`, row unchanged; on a `suspended` row → applied.
- **Single-statement guarantee**: the retry branch is one `UPDATE`; a test runs
  `fail_or_retry` 200 times on fresh rows while a concurrent reader loops on
  `SELECT status`, asserting every observed status ∈ {`running`, `ready`}. This is a
  probabilistic check of the property; the structural guarantee is that no statement
  ever writes `'failed'` on the retry branch (asserted by code review and by grepping
  the SQL in a unit test).
- The existing retry integration tests (`integration_test.rs:21672`, `:22138`, and the
  step-retry cases in `restart_integration_test.rs`) are the oracle and run unchanged.
- Reject: the existing 409 test for rejecting a non-suspended step stays green.
- Regression for the window: a step fails with retries remaining while another step's
  cascade runs concurrently; the dependent is never skipped, the loop is never failed.
- Regression for change 2: a step fails with retries remaining while its workspace is
  unloaded; the step is `ready` with `retry_at` set.

## 5. Rollout

One branch, two commits: (1) `fail_or_retry` + its DB tests; (2) switch the seven
sites, delete the block in `orchestrate_after_step`, delete `reset_for_retry` and
`reject_step`, add the regression tests. Patch release, deployed to every replica before
the cascade's activation release. CLAUDE.md Retry Mechanism section: replace "retry
check in `orchestrate_after_step()` before failure cascade" with "retry decided
atomically at the failure write by `JobStepRepo::fail_or_retry`; a retried step is never
observable as `failed`; retry does not depend on the workspace being loaded".

## 6. Review Log

- 2026-09-08 — rev 1 written as the cascade's prerequisite.
- 2026-09-08 — Codex review (fourth cascade pass): reject guard lost → `expected`
  precondition + `NotApplied`; history must be built from the incoming failure, all
  reset columns listed; `Failed` needs attempt/max for the exhausted log; "only
  observable difference" too strong → §3 lists the two changes; `FOR SHARE` test does
  not prove the window → replaced (§4); replica-wide rollout before the cascade (§5).
