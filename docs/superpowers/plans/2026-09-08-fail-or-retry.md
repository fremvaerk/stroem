# Atomic Fail-or-Retry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decide a step's retry inside the same transaction that records its failure, so a step owed a retry is never observable as `failed`.

**Architecture:** One new repo function `JobStepRepo::fail_or_retry` locks the row, applies either today's `mark_failed` assignment or today's `reset_for_retry` assignment (built from the incoming error), and commits. One server helper `job_recovery::fail_step` wraps it with the existing log lines. The seven sites that today call `mark_failed`/`reject_step` and then `orchestrate_after_step` call the helper and orchestrate only on `Failed`. The retry block at the top of `orchestrate_after_step` is deleted.

**Tech Stack:** Rust, sqlx runtime queries against Postgres, testcontainers for DB tests, axum router driven with `tower::ServiceExt::oneshot` in server integration tests.

**Spec:** `docs/superpowers/specs/2026-09-08-fail-or-retry-design.md` (rev 4). Read §1 for the sites, §2 for the exact SQL, §3 for what must stay identical.

## Global Constraints

- `anyhow::Result` with `.context("...")` on every fallible call (CLAUDE.md § Conventions).
- sqlx runtime queries only (`sqlx::query`, `sqlx::query_as`), never compile-time macros.
- Transaction-taking repo functions are generic over `E: sqlx::Executor<'e, Database = sqlx::Postgres>` and callers pass `&mut *tx` (pattern: `JobStepRepo::fail_non_terminal_steps_tx`, `crates/stroem-db/src/repos/job_step.rs:948-955`).
- Every error-message and log-line text is preserved byte for byte (spec §3).
- No AI co-author trailer in commit messages (user's global CLAUDE.md).
- Run `cargo fmt --all` before every commit; `cargo clippy --workspace -- -D warnings` must stay clean.
- Container tests need Docker. Run them with `cargo test -p <crate> --test <file> <filter>`.

---

### Task 1: `JobStepRepo::fail_or_retry` and `FailOutcome`

**Files:**
- Modify: `crates/stroem-db/src/repos/job_step.rs` (add after `reset_for_retry`, which ends at line 556)
- Modify: `crates/stroem-db/src/lib.rs` (re-export `FailOutcome` next to `JobStepRepo`)
- Test: `crates/stroem-db/tests/job_step_status_tests.rs` (append; reuse its `setup_db`, `make_job`, `make_step`)

**Interfaces:**
- Consumes: `JobStepRow` (all `STEP_COLUMNS`, `job_step.rs:10`), `StepStatus` (`stroem_common::models::job::StepStatus`, `as_ref()` gives the lowercase status string), `JobStepRepo::create_steps(pool, &[NewJobStep])`.
- Produces:
  ```rust
  pub enum FailOutcome {
      NotApplied,
      Failed { attempt: i32, max: Option<i32> },
      RetryScheduled { attempt: i32, max: i32, delay_secs: u64 },
  }
  impl JobStepRepo {
      pub async fn fail_or_retry(
          pool: &PgPool, job_id: Uuid, step_name: &str, error: &str,
          expected: &[StepStatus],
          delay_for: impl FnOnce(&JobStepRow) -> u64,
      ) -> Result<FailOutcome>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-db/tests/job_step_status_tests.rs`:

```rust
// ─── fail_or_retry ───────────────────────────────────────────────────

use stroem_common::models::job::StepStatus;
use stroem_db::FailOutcome;

/// Insert one running step with the given retry budget and return nothing.
async fn make_running_step_with_retry(
    pool: &PgPool,
    job_id: Uuid,
    name: &str,
    max_retries: Option<i32>,
) -> Result<()> {
    let step = make_step(job_id, name, "running");
    JobStepRepo::create_steps(pool, &[step]).await?;
    sqlx::query(
        "UPDATE job_step SET max_retries = $3, retry_backoff_secs = 7, retry_strategy = 'fixed', \
         started_at = NOW() - INTERVAL '5 seconds', worker_id = NULL, \
         output = '{\"partial\": true}'::jsonb, agent_state = '{\"turn\": 1}'::jsonb \
         WHERE job_id = $1 AND step_name = $2",
    )
    .bind(job_id)
    .bind(name)
    .bind(max_retries)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_fail_or_retry_schedules_retry_when_budget_remains() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = make_job(&pool, "t").await?;
    make_running_step_with_retry(&pool, job_id, "s", Some(2)).await?;

    let outcome =
        JobStepRepo::fail_or_retry(&pool, job_id, "s", "boom", &[], |_| 7).await?;
    assert_eq!(
        outcome,
        FailOutcome::RetryScheduled { attempt: 1, max: 2, delay_secs: 7 }
    );

    let row = JobStepRepo::get_step(&pool, job_id, "s").await?.unwrap();
    assert_eq!(row.status, "ready");
    assert_eq!(row.retry_attempt, 1);
    assert!(row.retry_at.is_some(), "retry_at must be set");
    let retry_in = (row.retry_at.unwrap() - chrono::Utc::now()).num_seconds();
    assert!((5..=7).contains(&retry_in), "retry_at ≈ now + 7s, got {retry_in}s");
    assert!(row.worker_id.is_none());
    assert!(row.started_at.is_none());
    assert!(row.completed_at.is_none());
    assert!(row.error_message.is_none());
    assert!(row.output.is_none());
    assert!(row.agent_state.is_none());
    assert!(row.suspended_at.is_none());

    let history = row.retry_history.as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["attempt"], 0);
    assert_eq!(history[0]["error"], "boom");
    assert!(history[0]["started_at"].is_string(), "started_at copied from the row");
    assert!(history[0]["failed_at"].is_string(), "failed_at is this failure's time");
    Ok(())
}

#[tokio::test]
async fn test_fail_or_retry_fails_when_budget_exhausted() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = make_job(&pool, "t").await?;
    make_running_step_with_retry(&pool, job_id, "s", Some(1)).await?;
    sqlx::query("UPDATE job_step SET retry_attempt = 1 WHERE job_id = $1 AND step_name = 's'")
        .bind(job_id)
        .execute(&pool)
        .await?;

    let outcome =
        JobStepRepo::fail_or_retry(&pool, job_id, "s", "boom", &[], |_| 7).await?;
    assert_eq!(outcome, FailOutcome::Failed { attempt: 1, max: Some(1) });

    let row = JobStepRepo::get_step(&pool, job_id, "s").await?.unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_message.as_deref(), Some("boom"));
    assert!(row.completed_at.is_some());
    assert_eq!(row.retry_attempt, 1, "unchanged");
    assert_eq!(row.retry_history.as_array().unwrap().len(), 0, "unchanged");
    assert!(row.output.is_some(), "mark_failed never touches output");
    Ok(())
}

#[tokio::test]
async fn test_fail_or_retry_fails_when_no_retry_configured() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = make_job(&pool, "t").await?;
    make_running_step_with_retry(&pool, job_id, "s", None).await?;

    let outcome =
        JobStepRepo::fail_or_retry(&pool, job_id, "s", "boom", &[], |_| 7).await?;
    assert_eq!(outcome, FailOutcome::Failed { attempt: 0, max: None });
    let row = JobStepRepo::get_step(&pool, job_id, "s").await?.unwrap();
    assert_eq!(row.status, "failed");
    Ok(())
}

#[tokio::test]
async fn test_fail_or_retry_precondition() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = make_job(&pool, "t").await?;
    make_running_step_with_retry(&pool, job_id, "s", Some(2)).await?;

    // Expected suspended, row is running → nothing written.
    let outcome = JobStepRepo::fail_or_retry(
        &pool, job_id, "s", "rejected", &[StepStatus::Suspended], |_| 7,
    )
    .await?;
    assert_eq!(outcome, FailOutcome::NotApplied);
    let row = JobStepRepo::get_step(&pool, job_id, "s").await?.unwrap();
    assert_eq!(row.status, "running");
    assert_eq!(row.retry_attempt, 0);
    assert!(row.error_message.is_none());

    // Now suspend it and the precondition holds.
    sqlx::query("UPDATE job_step SET status = 'suspended' WHERE job_id = $1 AND step_name = 's'")
        .bind(job_id)
        .execute(&pool)
        .await?;
    let outcome = JobStepRepo::fail_or_retry(
        &pool, job_id, "s", "rejected", &[StepStatus::Suspended], |_| 7,
    )
    .await?;
    assert!(matches!(outcome, FailOutcome::RetryScheduled { .. }));
    Ok(())
}

#[tokio::test]
async fn test_fail_or_retry_missing_row_is_not_applied() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = make_job(&pool, "t").await?;
    let outcome =
        JobStepRepo::fail_or_retry(&pool, job_id, "nope", "boom", &[], |_| 7).await?;
    assert_eq!(outcome, FailOutcome::NotApplied);
    Ok(())
}

/// The retry branch is ONE UPDATE: a concurrent reader can only ever observe
/// `running` (before) or `ready` (after), never `failed`. Probabilistic check;
/// the structural guarantee is the single statement in the implementation.
#[tokio::test]
async fn test_fail_or_retry_never_exposes_failed_on_retry_path() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = make_job(&pool, "t").await?;
    for i in 0..50 {
        let name = format!("s{i}");
        make_running_step_with_retry(&pool, job_id, &name, Some(5)).await?;
    }
    let reader_pool = pool.clone();
    let reader = tokio::spawn(async move {
        let mut seen_failed = 0u32;
        for _ in 0..2000 {
            let statuses: Vec<String> = sqlx::query_scalar(
                "SELECT status FROM job_step WHERE job_id = $1",
            )
            .bind(job_id)
            .fetch_all(&reader_pool)
            .await
            .unwrap();
            seen_failed += statuses.iter().filter(|s| s.as_str() == "failed").count() as u32;
            tokio::task::yield_now().await;
        }
        seen_failed
    });
    for i in 0..50 {
        let name = format!("s{i}");
        JobStepRepo::fail_or_retry(&pool, job_id, &name, "boom", &[], |_| 1).await?;
    }
    assert_eq!(reader.await?, 0, "a retried step must never be observable as failed");
    Ok(())
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p stroem-db --test job_step_status_tests fail_or_retry`
Expected: compile error, `no function or associated item named fail_or_retry` and `unresolved import stroem_db::FailOutcome`.

- [ ] **Step 3: Implement `FailOutcome` and `fail_or_retry`**

In `crates/stroem-db/src/repos/job_step.rs`, add after the closing brace of `reset_for_retry` (line 556), inside `impl JobStepRepo`:

```rust
    /// Record a step failure and decide its retry in ONE transaction, so a row
    /// that will be retried is never observable as `failed`.
    ///
    /// * `expected` — when non-empty, the row must currently be in one of these
    ///   statuses or nothing is written (`NotApplied`). Approval rejection passes
    ///   `[Suspended]`; every other caller passes `[]`.
    /// * `delay_for` — computes the retry delay in seconds from the locked row
    ///   (the server passes `compute_retry_delay`).
    pub async fn fail_or_retry(
        pool: &PgPool,
        job_id: Uuid,
        step_name: &str,
        error: &str,
        expected: &[StepStatus],
        delay_for: impl FnOnce(&JobStepRow) -> u64,
    ) -> Result<FailOutcome> {
        let mut tx = pool.begin().await.context("begin fail_or_retry")?;

        let row = sqlx::query_as::<_, JobStepRow>(&format!(
            "SELECT {} FROM job_step WHERE job_id = $1 AND step_name = $2 FOR UPDATE",
            STEP_COLUMNS
        ))
        .bind(job_id)
        .bind(step_name)
        .fetch_optional(&mut *tx)
        .await
        .context("lock step row for fail_or_retry")?;

        let Some(row) = row else {
            tx.rollback().await.ok();
            return Ok(FailOutcome::NotApplied);
        };

        if !expected.is_empty() && !expected.iter().any(|s| s.as_ref() == row.status) {
            tx.rollback().await.ok();
            return Ok(FailOutcome::NotApplied);
        }

        let outcome = match row.max_retries {
            Some(max) if row.retry_attempt < max => {
                let delay_secs = delay_for(&row);
                let retry_at = Utc::now() + chrono::Duration::seconds(delay_secs as i64);
                sqlx::query(
                    r#"
                    UPDATE job_step
                    SET
                        retry_history = retry_history || jsonb_build_array(jsonb_build_object(
                            'attempt', retry_attempt,
                            'error', $3::text,
                            'started_at', started_at,
                            'failed_at', NOW()
                        )),
                        retry_attempt = retry_attempt + 1,
                        status = 'ready',
                        ready_at = NOW(),
                        retry_at = $4,
                        worker_id = NULL,
                        started_at = NULL,
                        completed_at = NULL,
                        error_message = NULL,
                        output = NULL,
                        agent_state = NULL,
                        suspended_at = NULL
                    WHERE job_id = $1 AND step_name = $2
                    "#,
                )
                .bind(job_id)
                .bind(step_name)
                .bind(error)
                .bind(retry_at)
                .execute(&mut *tx)
                .await
                .context("schedule step retry")?;
                FailOutcome::RetryScheduled {
                    attempt: row.retry_attempt + 1,
                    max,
                    delay_secs,
                }
            }
            _ => {
                sqlx::query(
                    r#"
                    UPDATE job_step
                    SET status = 'failed', error_message = $3, completed_at = NOW()
                    WHERE job_id = $1 AND step_name = $2
                    "#,
                )
                .bind(job_id)
                .bind(step_name)
                .bind(error)
                .execute(&mut *tx)
                .await
                .context("mark step failed")?;
                FailOutcome::Failed {
                    attempt: row.retry_attempt,
                    max: row.max_retries,
                }
            }
        };

        tx.commit().await.context("commit fail_or_retry")?;
        Ok(outcome)
    }
```

Add the enum at module level in the same file, above `impl JobStepRepo`:

```rust
/// Result of [`JobStepRepo::fail_or_retry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailOutcome {
    /// Precondition not met (row missing or not in one of `expected`). Nothing written.
    NotApplied,
    /// Retries exhausted or none configured. Row is `failed`.
    /// `attempt` is the row's `retry_attempt` (unchanged), `max` its `max_retries`.
    Failed { attempt: i32, max: Option<i32> },
    /// Row went straight to `ready` with `retry_at`; never observable as `failed`.
    /// `attempt` is the NEW `retry_attempt` (post-increment).
    RetryScheduled { attempt: i32, max: i32, delay_secs: u64 },
}
```

In `crates/stroem-db/src/lib.rs`, find the line that re-exports `JobStepRepo` (grep `pub use repos::job_step::`) and add `FailOutcome` to that `pub use` list.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p stroem-db --test job_step_status_tests fail_or_retry`
Expected: 6 passed.

- [ ] **Step 5: Run the rest of the stroem-db suite to be sure nothing else moved**

Run: `cargo test -p stroem-db`
Expected: all green (the only change is additive).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/stroem-db/src/repos/job_step.rs crates/stroem-db/src/lib.rs crates/stroem-db/tests/job_step_status_tests.rs
git commit -m "feat(db): JobStepRepo::fail_or_retry — decide step retry in the failure transaction"
```

---

### Task 2: Server helper `job_recovery::fail_step` and helper visibility

**Files:**
- Modify: `crates/stroem-server/src/job_recovery.rs` (make three helpers `pub(crate)`, add `fail_step` above `orchestrate_after_step` at line 145)

**Interfaces:**
- Consumes: `JobStepRepo::fail_or_retry`, `FailOutcome` (Task 1); `compute_retry_delay(&JobStepRow) -> u64` (line 1092), `step_retry_message(step, retry_attempt_pre_increment, max, delay_secs)` (line 1050), `step_retries_exhausted_message(step, retry_attempt, max)` (line 1066); `AppState::append_server_log(job_id, &str)`.
- Produces:
  ```rust
  pub(crate) async fn fail_step(
      state: &AppState, job_id: Uuid, step_name: &str, error: &str,
      expected: &[StepStatus],
  ) -> Result<FailOutcome>;
  ```
  Writes the failure, appends the retry or exhausted log line, and returns the outcome. It does **not** orchestrate; the caller calls `orchestrate_after_step` on `Failed` and does nothing on `RetryScheduled` (today's early return). This keeps every site's existing error handling around orchestration untouched.

- [ ] **Step 1: Write the failing unit test**

In `crates/stroem-server/src/job_recovery.rs`, the existing `#[cfg(test)] mod tests` (grep `retry_messages_count_executions_consistently`) gains:

```rust
    /// `fail_step` passes the PRE-increment attempt to `step_retry_message`
    /// (the outcome carries the post-increment one), so the log line keeps
    /// today's "attempt N/M" arithmetic.
    #[test]
    fn fail_step_log_line_uses_pre_increment_attempt() {
        let outcome = stroem_db::FailOutcome::RetryScheduled { attempt: 1, max: 2, delay_secs: 7 };
        let line = retry_log_line("s", &outcome).unwrap();
        assert_eq!(line, "[retry] Step 's' attempt 1/3 failed, retrying in 7s");

        let outcome = stroem_db::FailOutcome::Failed { attempt: 2, max: Some(2) };
        let line = retry_log_line("s", &outcome).unwrap();
        assert_eq!(line, "[retry] Step 's' retries exhausted (3/3)");

        let outcome = stroem_db::FailOutcome::Failed { attempt: 0, max: None };
        assert!(retry_log_line("s", &outcome).is_none());

        assert!(retry_log_line("s", &stroem_db::FailOutcome::NotApplied).is_none());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p stroem-server --lib fail_step_log_line_uses_pre_increment_attempt`
Expected: compile error, `cannot find function retry_log_line`.

- [ ] **Step 3: Implement**

In `crates/stroem-server/src/job_recovery.rs`:

1. Change `fn step_retry_message(` (line 1050), `fn step_retries_exhausted_message(` (line 1066) and `fn compute_retry_delay(` (line 1092) to `pub(crate) fn`.
2. Add, immediately above `#[tracing::instrument(skip(state))] pub async fn orchestrate_after_step` (line 145):

```rust
/// The server-log line for a `fail_or_retry` outcome, or `None` when there is
/// nothing to log (no retry configured, or nothing was applied).
pub(crate) fn retry_log_line(step_name: &str, outcome: &stroem_db::FailOutcome) -> Option<String> {
    use stroem_db::FailOutcome::*;
    match outcome {
        RetryScheduled { attempt, max, delay_secs } => Some(step_retry_message(
            step_name,
            attempt - 1,
            *max,
            *delay_secs,
        )),
        Failed { attempt, max: Some(max) } => {
            Some(step_retries_exhausted_message(step_name, *attempt, *max))
        }
        Failed { max: None, .. } | NotApplied => None,
    }
}

/// Record a step failure, deciding its retry atomically (see
/// `JobStepRepo::fail_or_retry`), and append the matching server-log line.
///
/// Does NOT orchestrate. Callers run `orchestrate_after_step` when the outcome is
/// `Failed` and do nothing when it is `RetryScheduled` — the step is `ready` again.
pub(crate) async fn fail_step(
    state: &AppState,
    job_id: Uuid,
    step_name: &str,
    error: &str,
    expected: &[StepStatus],
) -> Result<stroem_db::FailOutcome> {
    let outcome = JobStepRepo::fail_or_retry(
        &state.pool,
        job_id,
        step_name,
        error,
        expected,
        compute_retry_delay,
    )
    .await
    .with_context(|| format!("fail_or_retry for step '{}' of job {}", step_name, job_id))?;
    if let Some(line) = retry_log_line(step_name, &outcome) {
        state.append_server_log(job_id, &line).await;
    }
    Ok(outcome)
}
```

`StepStatus` and `JobStepRepo` are already imported in this file (they are used at lines 186-194).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p stroem-server --lib retry`
Expected: `fail_step_log_line_uses_pre_increment_attempt` and `retry_messages_count_executions_consistently` pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/job_recovery.rs
git commit -m "feat(server): job_recovery::fail_step wraps fail_or_retry with the retry log lines"
```

---

### Task 3: Switch the seven sites, delete the old retry block and the two dead repo functions

**Files:**
- Modify: `crates/stroem-server/src/web/worker_api/jobs.rs:370-395` (`fail_claimed_step`) and `:905-935` (`complete_step`)
- Modify: `crates/stroem-server/src/recovery.rs:91-105`, `:143-155`, `:193-205`, `:257-270`
- Modify: `crates/stroem-server/src/web/api/jobs.rs:1073-1100` (reject branch)
- Modify: `crates/stroem-server/src/job_recovery.rs:183-222` (delete the retry block)
- Modify: `crates/stroem-db/src/repos/job_step.rs` (delete `reset_for_retry` `:520-556` and `reject_step` `:1138-1160`; update the doc comment of `test_approve_reject_return_type_semantics` at `:1317`)

**Interfaces:**
- Consumes: `job_recovery::fail_step` (Task 2), `FailOutcome`.
- Produces: nothing new. After this task no production code calls `JobStepRepo::mark_failed` followed by `orchestrate_after_step`.

- [ ] **Step 1: Run the retry oracle before touching anything, to know the baseline**

Run: `cargo test -p stroem-server --test integration_test step_retry`
Expected: `test_step_retry_resets_failed_step`, `test_step_retry_max_retries_column_is_max_attempts_minus_one`, `test_step_retry_exhausted_fails_job`, `test_step_retry_claim_respects_retry_at`, `test_step_retry_with_continue_on_failure`, `test_step_retry_success_on_second_attempt` all pass.

- [ ] **Step 2: `complete_step` (`web/worker_api/jobs.rs:909-935`)**

Replace the body from `let step_failed = ...` through the `orchestrate_after_step` call with:

```rust
    // Determine if this step failed based on exit_code or error
    let step_failed = req.exit_code.unwrap_or(0) != 0 || req.error.is_some();

    if step_failed {
        let error_msg = req
            .error
            .unwrap_or_else(|| format!("Process exited with code {}", req.exit_code.unwrap_or(1)));
        // Failure and retry decision in one transaction: a retried step is
        // `ready` again and needs no orchestration.
        let outcome = crate::job_recovery::fail_step(&state, job_id, &step_name, &error_msg, &[])
            .await
            .context("mark step failed")?;
        if !matches!(outcome, stroem_db::FailOutcome::Failed { .. }) {
            return Ok(Json(json!({"status": "ok"})));
        }
    } else {
        JobStepRepo::mark_completed(
            &state.pool,
            job_id,
            &step_name,
            req.output.map(into_exposed),
        )
        .await
        .context("mark step completed")?;
    }

    // Orchestrate: promote steps, skip unreachable, propagate to parent, fire hooks
    crate::job_recovery::orchestrate_after_step(&state, job_id, &step_name)
        .await
        .context("orchestration after step completion")?;

    Ok(Json(json!({"status": "ok"})))
```

- [ ] **Step 3: `fail_claimed_step` (`web/worker_api/jobs.rs:379-390`)**

Replace the `if let Err(e) = JobStepRepo::mark_failed(...)` block with:

```rust
    match crate::job_recovery::fail_step(state, job_id, step_name, error_msg, &[]).await {
        Err(e) => {
            tracing::error!("Failed to mark step as failed after render error: {:#}", e);
        }
        Ok(stroem_db::FailOutcome::Failed { .. }) => {
            // Trigger orchestration so the job can progress (fail/skip downstream steps)
            if let Err(e) =
                crate::job_recovery::orchestrate_after_step(state, job_id, step_name).await
            {
                let orch_msg = format!("Failed to orchestrate after render failure: {:#}", e);
                tracing::error!("{}", orch_msg);
                state.append_server_log(job_id, &orch_msg).await;
            }
        }
        Ok(_) => {} // retry scheduled: step is ready again, nothing to orchestrate
    }
```

- [ ] **Step 4: The four recovery phases (`recovery.rs`)**

At each of the four places (lines 91-105 stale worker, 143-155 step timeout, 193-205 approval timeout, 257-270 unmatched step) the shape is

```rust
        JobStepRepo::mark_failed(&state.pool, step_info.job_id, &step_info.step_name, <err>).await?;

        if let Err(e) = orchestrate_after_step(state, step_info.job_id, &step_info.step_name).await {
            tracing::error!(<existing message>, ...);
        }
```

Replace each with (keep each site's existing `tracing::error!` text):

```rust
        let outcome = crate::job_recovery::fail_step(
            state,
            step_info.job_id,
            &step_info.step_name,
            <err>,
            &[],
        )
        .await?;

        if matches!(outcome, stroem_db::FailOutcome::Failed { .. }) {
            if let Err(e) =
                orchestrate_after_step(state, step_info.job_id, &step_info.step_name).await
            {
                tracing::error!(<existing message>, ...);
            }
        }
```

Where `<err>` is whatever expression the site passes today (`&error_msg` at the first two, `error_msg` at the last two). The `?` on the failure write and the log-and-continue on orchestration are exactly today's error semantics.

- [ ] **Step 5: Reject (`web/api/jobs.rs:1073-1100`)**

Replace from `let applied = JobStepRepo::reject_step(...)` through the `if !applied { ... }` block with:

```rust
        // Atomic reject — only succeeds if step is still suspended; decides
        // retry in the same transaction (a rejected approval with retry budget
        // becomes `ready` again, as today).
        let outcome = crate::job_recovery::fail_step(
            &state,
            job_id,
            &step_name,
            &reason,
            &[StepStatus::Suspended],
        )
        .await
        .context("reject suspended step")?;

        if matches!(outcome, stroem_db::FailOutcome::NotApplied) {
            return Err(AppError::Conflict(
                "Step is no longer in suspended state".to_string(),
            ));
        }
```

Keep the `[approval] Step '{}' rejected by {}: {}` log append that follows. Then wrap the existing `orchestrate_after_step` call (and its error handling) in
`if matches!(outcome, stroem_db::FailOutcome::Failed { .. }) { ... }`.
Add `use stroem_common::models::job::StepStatus;` to the file's imports if it is not already there (grep `StepStatus` in `web/api/jobs.rs`).

- [ ] **Step 6: Delete the retry block in `orchestrate_after_step` (`job_recovery.rs:183-222`)**

Delete from the comment `// Check if the just-completed step failed and should be retried.` through the closing brace of `if let Some(step_row) = JobStepRepo::get_step(...)` (the line before `// Check if this is a loop instance completing`). The function now goes from the task lookup straight to `check_loop_completion`.

- [ ] **Step 7: Delete `reset_for_retry` and `reject_step` in stroem-db**

In `crates/stroem-db/src/repos/job_step.rs` delete `pub async fn reset_for_retry` (with its doc comment, lines 517-556) and `pub async fn reject_step` (lines 1135-1160). Update the doc comment of `test_approve_reject_return_type_semantics` (line ~1317) to read:

```rust
    /// Verify the `Result<bool>` contract of `approve_step`, and document that
    /// rejection now goes through `fail_or_retry` with `expected = [Suspended]`,
    /// returning `FailOutcome::NotApplied` where `reject_step` returned `false`.
```

- [ ] **Step 8: Build and lint**

Run: `cargo build --workspace && cargo clippy --workspace -- -D warnings`
Expected: clean. If `reject_step` or `reset_for_retry` is still referenced anywhere, the build names the site; there must be none.

- [ ] **Step 9: Run the oracle**

Run: `cargo test -p stroem-server --test integration_test retry` and `cargo test -p stroem-server --test restart_integration_test retry`
Expected: every test that passed in Step 1 still passes, plus `test_task_retry_*` and `test_retry_*`.

Also run: `cargo test -p stroem-server --test integration_test approval`
Expected: green (the reject 409 path is covered there).

- [ ] **Step 10: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/web/worker_api/jobs.rs crates/stroem-server/src/recovery.rs crates/stroem-server/src/web/api/jobs.rs crates/stroem-server/src/job_recovery.rs crates/stroem-db/src/repos/job_step.rs
git commit -m "refactor(retry): decide step retry at the failure write at all seven sites; delete reset_for_retry, reject_step and the post-hoc retry block"
```

---

### Task 4: Regression tests for the closed window and the workspace-outage change

**Files:**
- Test: `crates/stroem-server/tests/integration_test.rs` (append next to `test_step_retry_resets_failed_step`, line 21618; reuse `retry_workspace`, `setup_with_workspace`, `create_job_for_task`, `register_test_worker`, `worker_request`, `body_json` from that file)

**Interfaces:**
- Consumes: the public worker API (`/worker/jobs/claim`, `/worker/jobs/{id}/steps/{step}/complete`), `JobStepRepo::get_steps_for_job`, `stroem_server::orchestrator::on_step_completed`.

- [ ] **Step 1: Write the window regression**

```rust
/// A step that fails with retry budget is `ready` the instant the failure is
/// recorded, so an orchestration for ANY other step of the job cannot see it as
/// failed and skip its dependents.
#[tokio::test]
async fn test_step_retry_window_never_skips_dependents() -> Result<()> {
    use stroem_common::duration::HumanDuration;
    use stroem_common::models::workflow::{BackoffStrategy, RetryConfig};

    let retry_cfg = RetryConfig {
        max_attempts: 2,
        delay: HumanDuration(0),
        backoff: BackoffStrategy::Fixed,
        jitter: false,
    };
    let workspace = retry_workspace(retry_cfg);
    let (router, pool, _tmp, _container) = setup_with_workspace(workspace.clone()).await?;
    let task = workspace.tasks.get("retry-task").unwrap().clone();

    let job_id = create_job_for_task(
        &pool, &workspace, "default", "retry-task", json!({}), "api",
        None, None, None, None, JobDefaults::default(),
    )
    .await?;

    // A dependent of step1 that today would be cascade-skipped in the window.
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, action_spec, status) \
         VALUES ($1, 'dependent', 'noop', 'script', '{}'::jsonb, 'pending')",
    )
    .bind(job_id)
    .execute(&pool)
    .await?;
    // and an unrelated sibling whose completion triggers a cascade
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, action_spec, status) \
         VALUES ($1, 'sibling', 'noop', 'script', '{}'::jsonb, 'completed')",
    )
    .bind(job_id)
    .execute(&pool)
    .await?;
    let mut task_with_dep = task.clone();
    task_with_dep.flow.insert(
        "dependent".to_string(),
        stroem_common::models::workflow::FlowStep {
            action: "noop".to_string(),
            depends_on: vec!["step1".to_string()],
            ..Default::default()
        },
    );
    task_with_dep.flow.insert(
        "sibling".to_string(),
        stroem_common::models::workflow::FlowStep {
            action: "noop".to_string(),
            ..Default::default()
        },
    );

    let worker_id = register_test_worker(&pool).await;
    let claim = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["script"]}),
        ))
        .await?;
    assert_eq!(claim.status(), StatusCode::OK);

    // Fail step1 (retry budget remains) and, without waiting, cascade on the sibling.
    let complete = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step1/complete", job_id),
            json!({"exit_code": 1, "error": "flaky"}),
        ))
        .await?;
    assert_eq!(complete.status(), StatusCode::OK);
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "sibling", &task_with_dep, None)
        .await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let by = |n: &str| steps.iter().find(|s| s.step_name == n).unwrap().status.clone();
    assert_eq!(by("step1"), "ready", "retried step is ready, never failed");
    assert_eq!(by("dependent"), "pending", "dependent must not be cascade-skipped");
    Ok(())
}
```

If `FlowStep` does not implement `Default` in this crate, build it with the same fields the file's other tests use (grep `FlowStep {` in `integration_test.rs` and copy that literal, setting `depends_on`).

- [ ] **Step 2: Write the workspace-outage regression**

```rust
/// Retry no longer depends on the workspace being loaded (spec §3 change 2):
/// the step is `ready` with `retry_at` even when orchestration could not find
/// the workspace, and a later claim proceeds on the stored input/spec.
#[tokio::test]
async fn test_step_retry_scheduled_when_workspace_unavailable() -> Result<()> {
    use stroem_common::duration::HumanDuration;
    use stroem_common::models::workflow::{BackoffStrategy, RetryConfig};

    let retry_cfg = RetryConfig {
        max_attempts: 2,
        delay: HumanDuration(0),
        backoff: BackoffStrategy::Fixed,
        jitter: false,
    };
    let workspace = retry_workspace(retry_cfg);
    let (router, pool, _tmp, _container) = setup_with_workspace(workspace.clone()).await?;
    let job_id = create_job_for_task(
        &pool, &workspace, "default", "retry-task", json!({}), "api",
        None, None, None, None, JobDefaults::default(),
    )
    .await?;
    let worker_id = register_test_worker(&pool).await;
    let claim = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["script"]}),
        ))
        .await?;
    assert_eq!(claim.status(), StatusCode::OK);

    // Make the job's workspace unresolvable before the failure is reported.
    sqlx::query("UPDATE job SET workspace = 'gone' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;

    let complete = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step1/complete", job_id),
            json!({"exit_code": 1, "error": "flaky"}),
        ))
        .await?;
    assert_eq!(complete.status(), StatusCode::OK);

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "step1").unwrap();
    assert_eq!(step.status, "ready", "retry is scheduled regardless of workspace availability");
    assert_eq!(step.retry_attempt, 1);
    assert!(step.retry_at.is_some());

    // The retry is claimable on the stored input/spec once retry_at has passed.
    sqlx::query("UPDATE job_step SET retry_at = NOW() - INTERVAL '1 second' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;
    let claim2 = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["script"]}),
        ))
        .await?;
    assert_eq!(claim2.status(), StatusCode::OK);
    let body = body_json(claim2).await;
    assert_eq!(body["job_id"].as_str().unwrap(), job_id.to_string());
    assert_eq!(body["step_name"].as_str().unwrap(), "step1");
    Ok(())
}
```

- [ ] **Step 3: Run both tests**

Run: `cargo test -p stroem-server --test integration_test test_step_retry_window_never_skips_dependents test_step_retry_scheduled_when_workspace_unavailable`
Expected: both pass. (Sanity check that the first one is a real regression test: `git stash`-free alternative is to temporarily revert Task 3's `complete_step` change to the old `mark_failed` + `orchestrate_after_step` and re-run; the window test then fails with `dependent == "skipped"`. Restore before committing.)

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/tests/integration_test.rs
git commit -m "test(retry): window regression (dependents never skipped) and retry scheduled while workspace unavailable"
```

---

### Task 5: Documentation

**Files:**
- Modify: `CLAUDE.md` § Retry Mechanism (the bullet starting `**Server-side**: retry check in \`orchestrate_after_step()\``)
- Modify: `docs/internal/TODO.md` (add follow-up)

- [ ] **Step 1: CLAUDE.md**

Replace the bullet
`- **Server-side**: retry check in `orchestrate_after_step()` before failure cascade. `claim_ready_step` respects `retry_at`.`
with:

```markdown
- **Server-side**: the retry decision is made atomically at the failure write by `JobStepRepo::fail_or_retry` (called through `job_recovery::fail_step` at the seven sites that fail a step and then orchestrate: worker `complete_step`, claim-time render failure, the four recovery phases, approval reject). A retried step goes straight from `running`/`suspended` to `ready` with `retry_at`; it is **never observable as `failed`**, so no concurrent cascade or loop rollup can act on it. Retry no longer depends on the workspace being loaded at orchestration time. Callers orchestrate only on `FailOutcome::Failed`. `claim_ready_step` respects `retry_at`. Failure paths that never retry (child-job propagation, task dispatch failure, approval dispatch failure, `when`/`for_each` errors) still call plain `mark_failed`.
```

- [ ] **Step 2: TODO.md**

Under the Architecture section add:

```markdown
- [ ] Step retry is honoured only on the seven `fail_step` paths; `propagate_to_parent` (child failed), `fail_task_step` (dispatch failure) and approval dispatch/render failures bypass it. Decide whether `type: task` / approval steps should retry on those paths (transition candidate). Pre-existing; surfaced during the step-cascade design review 2026-09-08.
```

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md docs/internal/TODO.md
git commit -m "docs: retry decided atomically at the failure write; log the retry-ownership gap"
```

---

## Self-review

- **Spec coverage.** §1 sites: Task 3 steps 2–5 (seven sites: complete_step, fail_claimed_step, four recovery phases, reject). §2 SQL: Task 1 step 3 reproduces the RETRY and FAIL assignments column for column, `expected`/`NotApplied`, `Failed { attempt, max }`, `RetryScheduled { attempt: post-increment }`; helper visibility: Task 2 step 3; `retry_at` sampled in Rust after the `FOR UPDATE` (Task 1: `Utc::now()` after `fetch_optional`). §3 preserved semantics: Task 3 step 1/9 run the oracle; change 1 tested in Task 4 step 1; change 2 tested in Task 4 step 2. §4 tests: Task 1 (five DB tests + the probabilistic reader), Task 3 step 9 (oracle, reject 409), Task 4. §5 rollout: Task 1 = commit 1, Tasks 2–4 = commit 2 split into reviewable pieces, Task 5 = docs. Replica-wide rollout is a deployment note, not code.
- **Placeholders.** None; every step carries code or an exact command. Task 4 step 1 names a fallback for `FlowStep::default()` with a concrete grep target.
- **Type consistency.** `FailOutcome` variants and field names identical in Tasks 1–4; `fail_step(state, job_id, step_name, error, expected) -> Result<FailOutcome>` used identically at every site; `retry_log_line` only in Task 2.
