//! Retry: the server-log lines for a step's `fail_or_retry` outcome, the step
//! backoff computation, and task-level retry-job creation.

use super::{CreatedJob, Settlement};
use anyhow::{Context, Result};
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{JobRow, JobStepRow};
use uuid::Uuid;

/// The server-log line for a `fail_or_retry` outcome, or `None` when there is
/// nothing to log (no retry configured, or nothing was applied).
pub(crate) fn retry_log_line(step_name: &str, outcome: &stroem_db::FailOutcome) -> Option<String> {
    use stroem_db::FailOutcome::*;
    match outcome {
        RetryScheduled {
            attempt,
            max,
            delay_secs,
        } => Some(step_retry_message(
            step_name,
            attempt - 1,
            *max,
            *delay_secs,
        )),
        Failed {
            attempt,
            max: Some(max),
        } => Some(step_retries_exhausted_message(step_name, *attempt, *max)),
        Failed { max: None, .. } | NotApplied => None,
    }
}

/// Server-log line for a failed step execution that will be retried.
///
/// `retry_attempt` is 0 for the initial execution and `max_retries` counts
/// retries only (`RetryConfig.max_attempts`), so both sides of the slash are
/// converted to execution counts: `retry_attempt + 1` of `max_retries + 1`.
/// This matches the UI step timeline (`attempt N/M`).
pub(crate) fn step_retry_message(
    step_name: &str,
    retry_attempt: i32,
    max_retries: i32,
    delay_secs: u64,
) -> String {
    format!(
        "[retry] Step '{}' attempt {}/{} failed, retrying in {}s",
        step_name,
        retry_attempt + 1,
        max_retries + 1,
        delay_secs,
    )
}

/// Server-log line for the final failed execution of a step (no retries left).
pub(crate) fn step_retries_exhausted_message(
    step_name: &str,
    retry_attempt: i32,
    max_retries: i32,
) -> String {
    format!(
        "[retry] Step '{}' retries exhausted ({}/{})",
        step_name,
        retry_attempt + 1,
        max_retries + 1,
    )
}

/// Server-log line for a task-level retry that creates a new job.
fn task_retry_message(
    retry_attempt: i32,
    max_retries: i32,
    retry_job_id: Uuid,
    delay_secs: u64,
) -> String {
    format!(
        "[retry] Task attempt {}/{} failed, retrying as job {} (in {}s)",
        retry_attempt + 1,
        max_retries + 1,
        retry_job_id,
        delay_secs,
    )
}

/// Compute the retry delay in seconds for a step based on its retry config.
pub(crate) fn compute_retry_delay(step: &JobStepRow) -> u64 {
    let base_secs = step.retry_backoff_secs.unwrap_or(30) as u64;
    let strategy = step.retry_strategy.as_deref().unwrap_or("fixed");
    let attempt = step.retry_attempt.max(0) as u32;

    let delay = match strategy {
        "exponential" => base_secs.saturating_mul(1u64 << attempt.min(6)),
        _ => base_secs, // fixed
    };

    if step.retry_jitter {
        // Add 0-25% random jitter to spread out concurrent retries
        let jitter_max = delay / 4 + 1;
        let jitter = rand::random::<u64>() % jitter_max;
        delay.saturating_add(jitter)
    } else {
        delay
    }
}

/// Create the retry job for a failed top-level job and link the two rows.
/// Returns the created job for the caller to finalize through
/// `Settlement::job_created`. Spec §8.1.
pub(super) async fn create_retry_job(
    s: &Settlement,
    failed_job: &JobRow,
    workspace: &WorkspaceConfig,
    task: &TaskDef,
) -> Result<Option<CreatedJob>> {
    let max = match failed_job.max_retries {
        Some(m) => m,
        None => return Ok(None),
    };
    if failed_job.retry_attempt >= max {
        return Ok(None);
    }

    // Determine the root job ID (first in the retry chain)
    let root_job_id = failed_job.retry_of_job_id.unwrap_or(failed_job.job_id);

    // Compute delay for the task retry
    let delay_secs = if let Some(ref retry) = task.retry {
        let base = retry.delay.as_secs();
        let attempt = failed_job.retry_attempt.max(0) as u32;
        match retry.backoff {
            stroem_common::models::workflow::BackoffStrategy::Exponential => {
                base.saturating_mul(1u64 << attempt.min(6))
            }
            stroem_common::models::workflow::BackoffStrategy::Fixed => base,
        }
    } else {
        30
    };

    let input = failed_job.input.clone().unwrap_or_default();
    let created = crate::job_creator::create_job_for_task_detailed(
        &s.workspaces,
        &s.pool,
        workspace,
        &failed_job.workspace,
        &failed_job.task_name,
        input,
        "retry",
        Some(&failed_job.job_id.to_string()),
        failed_job.revision.as_deref(),
        None, // source_job_id: automatic retries don't prefill from source
        None,
        s.defaults,
    )
    .await
    .context("Failed to create retry job")?;
    let retry_job_id = created.job_id;

    // Set retry tracking fields, link original → retry, and optionally set retry_at
    // in a single transaction so the retry job is never visible in a partial state.
    let mut tx = s
        .pool
        .begin()
        .await
        .context("Failed to begin retry transaction")?;

    sqlx::query("UPDATE job SET retry_of_job_id = $1, retry_attempt = $2 WHERE job_id = $3")
        .bind(root_job_id)
        .bind(failed_job.retry_attempt + 1)
        .bind(retry_job_id)
        .execute(&mut *tx)
        .await
        .context("Failed to set retry fields on new job")?;

    sqlx::query("UPDATE job SET retry_job_id = $1 WHERE job_id = $2")
        .bind(retry_job_id)
        .bind(failed_job.job_id)
        .execute(&mut *tx)
        .await
        .context("Failed to link original to retry job")?;

    if delay_secs > 0 {
        let retry_at = chrono::Utc::now() + chrono::Duration::seconds(delay_secs as i64);
        sqlx::query("UPDATE job_step SET retry_at = $1 WHERE job_id = $2 AND status = 'ready'")
            .bind(retry_at)
            .bind(retry_job_id)
            .execute(&mut *tx)
            .await
            .context("Failed to set retry_at on retry job steps")?;
    }

    tx.commit()
        .await
        .context("Failed to commit retry transaction")?;

    s.server_log(
        failed_job.job_id,
        &task_retry_message(failed_job.retry_attempt, max, retry_job_id, delay_secs),
    )
    .await;

    Ok(Some(created))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn make_step(status: &str, error_message: Option<&str>) -> JobStepRow {
        JobStepRow {
            job_id: Uuid::new_v4(),
            step_name: "hook".to_string(),
            action_name: "notify".to_string(),
            action_type: "script".to_string(),
            status: status.to_string(), // DB model stays as String
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            error_message: error_message.map(String::from),
            required_ability: "script".to_string(),
            required_tags: json!([]),
            runner: "local".to_string(),
            retry_history: json!([]),
            ..Default::default()
        }
    }

    /// `step_failed` passes the PRE-increment attempt to `step_retry_message`
    /// (the outcome carries the post-increment one), so the log line keeps
    /// today's "attempt N/M" arithmetic.
    #[test]
    fn fail_step_log_line_uses_pre_increment_attempt() {
        let outcome = stroem_db::FailOutcome::RetryScheduled {
            attempt: 1,
            max: 2,
            delay_secs: 7,
        };
        let line = retry_log_line("s", &outcome).unwrap();
        assert_eq!(line, "[retry] Step 's' attempt 1/3 failed, retrying in 7s");

        let outcome = stroem_db::FailOutcome::Failed {
            attempt: 2,
            max: Some(2),
        };
        let line = retry_log_line("s", &outcome).unwrap();
        assert_eq!(line, "[retry] Step 's' retries exhausted (3/3)");

        let outcome = stroem_db::FailOutcome::Failed {
            attempt: 0,
            max: None,
        };
        assert!(retry_log_line("s", &outcome).is_none());

        assert!(retry_log_line("s", &stroem_db::FailOutcome::NotApplied).is_none());
    }

    // `max_retries` stores RetryConfig.max_attempts, which counts retries and
    // excludes the initial execution: max 2 ⇒ 3 executions. Every message
    // counts EXECUTIONS on both sides of the slash, matching the UI timeline's
    // `attempt {retry_attempt + 1}/{max_retries + 1}`.
    #[test]
    fn retry_messages_count_executions_consistently() {
        // First execution (retry_attempt 0) failed, 2 retries allowed ⇒ 1 of 3.
        assert_eq!(
            step_retry_message("run", 0, 2, 900),
            "[retry] Step 'run' attempt 1/3 failed, retrying in 900s"
        );
        // Second execution (retry_attempt 1) failed ⇒ 2 of 3.
        assert_eq!(
            step_retry_message("run", 1, 2, 1800),
            "[retry] Step 'run' attempt 2/3 failed, retrying in 1800s"
        );
        // Third execution (retry_attempt 2) failed and no retries remain ⇒ 3 of 3.
        assert_eq!(
            step_retries_exhausted_message("run", 2, 2),
            "[retry] Step 'run' retries exhausted (3/3)"
        );
        // Task-level retry follows the same convention.
        let job_id = Uuid::nil();
        assert_eq!(
            task_retry_message(0, 2, job_id, 60),
            format!(
                "[retry] Task attempt 1/3 failed, retrying as job {} (in 60s)",
                job_id
            )
        );
    }

    #[test]
    fn test_compute_retry_delay_fixed() {
        let mut step = make_step("failed", None);
        step.retry_backoff_secs = Some(10);
        step.retry_strategy = Some("fixed".to_string());
        step.retry_jitter = false;
        step.retry_attempt = 0;
        assert_eq!(compute_retry_delay(&step), 10);
        step.retry_attempt = 3;
        assert_eq!(compute_retry_delay(&step), 10); // fixed stays constant
    }

    #[test]
    fn test_compute_retry_delay_exponential() {
        let mut step = make_step("failed", None);
        step.retry_backoff_secs = Some(5);
        step.retry_strategy = Some("exponential".to_string());
        step.retry_jitter = false;
        step.retry_attempt = 0;
        assert_eq!(compute_retry_delay(&step), 5); // 5 * 2^0 = 5
        step.retry_attempt = 1;
        assert_eq!(compute_retry_delay(&step), 10); // 5 * 2^1 = 10
        step.retry_attempt = 2;
        assert_eq!(compute_retry_delay(&step), 20); // 5 * 2^2 = 20
        step.retry_attempt = 6;
        assert_eq!(compute_retry_delay(&step), 320); // 5 * 2^6 = 320
    }

    #[test]
    fn test_compute_retry_delay_exponential_capped() {
        let mut step = make_step("failed", None);
        step.retry_backoff_secs = Some(5);
        step.retry_strategy = Some("exponential".to_string());
        step.retry_jitter = false;
        step.retry_attempt = 6;
        assert_eq!(compute_retry_delay(&step), 320); // 5 * 2^6 = 320
        step.retry_attempt = 7;
        assert_eq!(compute_retry_delay(&step), 320); // capped at exponent 6
        step.retry_attempt = 10;
        assert_eq!(compute_retry_delay(&step), 320); // still capped
    }

    #[test]
    fn test_compute_retry_delay_default_strategy() {
        let mut step = make_step("failed", None);
        step.retry_backoff_secs = Some(15);
        step.retry_strategy = None; // defaults to fixed
        step.retry_jitter = false;
        step.retry_attempt = 3;
        assert_eq!(compute_retry_delay(&step), 15);
    }

    #[test]
    fn test_compute_retry_delay_jitter_adds_bounded_noise() {
        let mut step = make_step("failed", None);
        step.retry_backoff_secs = Some(100);
        step.retry_strategy = Some("fixed".to_string());
        step.retry_jitter = true;
        step.retry_attempt = 0;
        // Run several times to account for randomness
        for _ in 0..20 {
            let delay = compute_retry_delay(&step);
            assert!(delay >= 100, "delay should be at least base ({delay})");
            assert!(delay <= 125, "jitter should add at most 25% ({delay})");
        }
    }
}
