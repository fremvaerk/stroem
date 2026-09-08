# Atomic Fail-or-Retry — Design

**Status:** Draft (2026-09-08)
**Date:** 2026-09-08
**Origin:** prerequisite of `2026-09-08-step-cascade-design.md` (Q28); first slice of the
architecture-review candidate "One interface for every step status transition".
**Ships:** on its own branch, before the step cascade.

## 1. Problem

The step-retry decision is made *after* the failure has been written. Every path that
fails a worker-executed, recovered, or rejected step first calls
`JobStepRepo::mark_failed` and then `job_recovery::orchestrate_after_step`, whose first
act (`job_recovery.rs:186-209`) is to read the row back, decide whether a retry is owed,
and if so `reset_for_retry` it to `ready` with a `retry_at`. Between the two statements
the row is `failed` and visible to every concurrent reader:

- `skip_unreachable_steps` on another step's cascade skips its dependents;
- `check_loop_completion` on a sibling instance's completion fails the placeholder;
- after the step cascade lands, any cascade on the job would do both.

The window is a few milliseconds but it is real, and no reader can tell "failed, about to
be reset" from "failed, final" by looking at the row.

Sites (every `orchestrate_after_step` call preceded by a failure mark):

| Site | Marks failed when |
|---|---|
| `web/worker_api/jobs.rs:932` `complete_step` | worker reports failure |
| `web/worker_api/jobs.rs:383` | claim-time render/prepare failure of the action |
| `recovery.rs:100` phase 1 | stale worker |
| `recovery.rs:151` phase 2 | step timeout |
| `recovery.rs:200` phase 2.5 | suspended-step timeout |
| `recovery.rs:264` phase 4 | unmatched ready step |
| `web/api/jobs.rs:1095` `reject_step` | approval rejected |

(`web/api/jobs.rs:1041` `approve_step` marks completed; unchanged.)

Failure paths that today do **not** retry and are **not** changed by this design:
`propagate_to_parent` (child job failed, `job_recovery.rs:575`), `fail_task_step`
(dispatch failure, `job_creator.rs:743`), approval dispatch/render failure
(`job_creator.rs:1435`, `:1473`), and cascade-generated failures (`when`/`for_each`
errors). They keep calling plain `mark_failed`.

## 2. Change

One repo function replaces the mark-then-decide pair at the seven sites above:

```rust
pub enum FailOutcome {
    /// Retries exhausted or none configured. Row is `failed`.
    Failed,
    /// Row went straight to `ready` with `retry_at`; never observable as `failed`.
    RetryScheduled { attempt: i32, max: i32, delay_secs: u64 },
}

/// In ONE transaction: `SELECT … FOR UPDATE` the row; if `max_retries` is set and
/// `retry_attempt < max_retries`, apply today's `reset_for_retry` body (append to
/// `retry_history`, bump `retry_attempt`, clear `worker_id`/`agent_state`, set
/// `status='ready'`, `retry_at`), else apply today's `mark_failed` body. Commit.
pub async fn fail_or_retry(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    error: &str,
    delay_for: impl FnOnce(&JobStepRow) -> u64,   // today's compute_retry_delay
) -> Result<FailOutcome>;
```

Callers:

```rust
match JobStepRepo::fail_or_retry(pool, job_id, step, &err, compute_retry_delay).await? {
    FailOutcome::RetryScheduled { attempt, max, delay_secs } => {
        state.append_server_log(job_id, &step_retry_message(step, attempt - 1, max, delay_secs)).await;
        // no orchestration: the step is ready again
    }
    FailOutcome::Failed => {
        if exhausted { state.append_server_log(job_id, &step_retries_exhausted_message(..)).await; }
        orchestrate_after_step(state, job_id, step).await?;
    }
}
```

`orchestrate_after_step`'s own retry block (`:186-222`) is deleted; the function starts
at the loop-completion check. `reset_for_retry` becomes private to the new function.
`compute_retry_delay`, `step_retry_message`, `step_retries_exhausted_message` are
unchanged (their unit test `retry_messages_count_executions_consistently` stays).

## 3. Semantics preserved

- Which steps retry, how many times, with what backoff and jitter, what the log lines
  say, what `retry_history` records, `retry_at` respected by claim, `agent_state`
  cleared on retry: all unchanged. The only observable difference is that no reader can
  ever see a `failed` row that will be retried.
- The early return "step is back to ready — do NOT cascade" is preserved: on
  `RetryScheduled` the caller does not orchestrate.
- Hooks still fire only after retries are exhausted (they fire from terminal handling,
  which only runs on `Failed`).

## 4. Tests

- Unit (`stroem-db`, container): `fail_or_retry` with retries remaining → row `ready`,
  `retry_attempt` +1, `retry_at` set, `retry_history` appended, `worker_id`/`agent_state`
  cleared, `error_message` unchanged from before; with retries exhausted or none
  configured → row `failed`, `error_message` set, `completed_at` set. Both under a
  concurrent reader holding a `SELECT … FOR SHARE` to prove the reader never observes
  `failed` for the retry case.
- The existing retry integration tests (`integration_test.rs:21672`, `:22138`, and the
  step-retry cases in `restart_integration_test.rs`) are the oracle and run unchanged.
- Regression for the window: a step fails with retries remaining while another step's
  cascade runs concurrently; the dependent is never skipped, the loop is never failed.

## 5. Rollout

One branch, two commits: (1) `fail_or_retry` + its tests; (2) switch the seven sites and
delete the block in `orchestrate_after_step`. Patch release. CLAUDE.md Retry Mechanism
section: replace "retry check in `orchestrate_after_step()` before failure cascade" with
"retry decided atomically at the failure mark by `fail_or_retry`; a retried step is never
observable as `failed`".
