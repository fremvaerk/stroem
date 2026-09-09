//! Terminal handling: the drain gate, the exactly-once claim, the pure plan of
//! which side effects a settled job gets, and the runner for those effects.

use super::{hooks, Settlement};
use crate::job_completion::JobCompletionEvent;
use crate::log_storage::JobLogMeta;
use anyhow::Result;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::job::{JobStatus, SourceType, StepStatus};
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_db::{JobRow, JobStepRepo, JobStepRow};
use uuid::Uuid;

/// Exactly-once claim that a job's terminal side effects — hooks, sync-waiter
/// notification, log archive upload, parent-step propagation, task-level
/// retry-job creation, and the completion metric — have started running.
///
/// Terminal state is observed concurrently from multiple independent code
/// paths (`Settlement::advance` entered from a step completion, a propagation,
/// a cancellation, or a creation-time settle), potentially on different server
/// replicas under HA with no shared lock. Every one of those paths MUST call
/// this function immediately after detecting a job is terminal and run
/// propagation, retry, hook, notify, and archive logic ONLY when it returns
/// `true` — otherwise the same job's terminal side effects (most importantly
/// hook jobs) can fire more than once.
///
/// Implemented as a DB-level CAS on `metrics_recorded_at`: whichever caller
/// wins the `UPDATE ... WHERE metrics_recorded_at IS NULL RETURNING job_id`
/// race increments the `stroem_jobs_completed_total` counter and returns
/// `true` — the metric emission piggybacks on the same claim rather than
/// being a separate concern. Every other caller observes the column already
/// set, returns `false`, and must skip all terminal side effects for this job.
///
/// The UPDATE is fail-closed: if it errors (e.g. pool exhausted), we return
/// `false` rather than risk double-processing — which means this job's hooks,
/// log archive upload and parent propagation are dropped entirely and no later
/// path re-observes it. That is deliberate but must never be silent, so the
/// error is logged at `error!` level AND written to the job's own log view via
/// `Settlement::server_log`.
pub(super) async fn claim(s: &Settlement, job: &JobRow) -> bool {
    match sqlx::query_scalar::<_, uuid::Uuid>(
        "UPDATE job SET metrics_recorded_at = NOW() \
         WHERE job_id = $1 AND metrics_recorded_at IS NULL \
         RETURNING job_id",
    )
    .bind(job.job_id)
    .fetch_optional(&s.pool)
    .await
    {
        Ok(Some(_)) => {
            // We won the CAS race — increment the counter and claim terminal handling.
            metrics::counter!(
                crate::metrics::STROEM_JOBS_COMPLETED_TOTAL,
                "status" => job.status.clone(),
            )
            .increment(1);
            true
        }
        Ok(None) => {
            // Another code path already claimed terminal handling for this job —
            // skip to avoid double-firing hooks, double-counting, etc.
            tracing::debug!(
                job_id = %job.job_id,
                "claim_terminal_handling: metrics_recorded_at already set, terminal handling already claimed elsewhere"
            );
            false
        }
        Err(e) => {
            tracing::error!(
                job_id = %job.job_id,
                error = %e,
                "claim_terminal_handling: CAS update failed, treating as not claimed"
            );
            s.server_log(
                job.job_id,
                &format!(
                    "[orchestration] terminal handling claim failed: {e} — \
                     hooks/archive/propagation NOT run for this job"
                ),
            )
            .await;
            false
        }
    }
}

/// Drain gate: a terminal job's side effects must not be claimed while any of
/// its own steps is still `running` or `claimed` on a worker.
///
/// A job row reaches a terminal status before its execution finishes whenever
/// the status is written from outside step completion — `JobRepo::cancel`
/// stamps `cancelled` the instant the user asks, while workers keep executing.
/// Without this gate the FIRST worker to report a step would find the job
/// terminal, win `claim`, and `run_terminal_actions` would `close_log` and
/// upload the archive while a sibling worker is still emitting lines. The
/// later completion loses the one-shot claim, so the archive is never
/// refreshed — before the exactly-once claim the upload simply ran again,
/// which is why this is a regression rather than a pre-existing wart. The
/// cancellation signal must stay visible to those workers for the same reason,
/// so `clear_cancelled` sits behind this gate too.
///
/// Returns `true` when the job is drained and the caller may proceed.
///
/// A query error fails **open** — deliberately the opposite of [`claim`].
/// Being wrong here costs at worst an early archive upload, whose content
/// still has a complete local-JSONL fallback; deferring on the LAST worker's
/// completion would instead drop the job's hooks entirely, with nothing left
/// to re-observe it.
pub(super) async fn drained(s: &Settlement, job_id: Uuid) -> bool {
    match JobStepRepo::has_live_steps(&s.pool, job_id).await {
        Ok(false) => true,
        Ok(true) => {
            tracing::debug!(
                job_id = %job_id,
                "terminal side effects deferred: live steps remain"
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                job_id = %job_id,
                error = %e,
                "drain check failed, proceeding with terminal handling"
            );
            true
        }
    }
}

/// Build a `JobLogMeta` from a `JobRow`.
fn meta_from_job(job: &JobRow) -> JobLogMeta {
    JobLogMeta {
        workspace: job.workspace.clone(),
        task_name: job.task_name.clone(),
        created_at: job.created_at,
    }
}

/// Which terminal side effects a settled job gets. Pure; see spec §6.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Success,
    Error,
    Cancel,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalPlan {
    /// The job has a parent step to mark.
    pub propagate: bool,
    /// Failed, top-level, and `retry_attempt < max_retries`.
    pub retry: bool,
    /// By status; `None` when `retry` is true (hooks fire only once retries
    /// are exhausted) or when the status has no hook kind (`skipped`).
    pub hooks: HookKind,
}

/// The hook kind a job status alone implies, ignoring the retry budget.
///
/// This is the status-only rule the pre-`Settlement` terminal path used. It is
/// what a caller needs when it has fallen through a retry plan whose retry job
/// could not be created: the plan's `hooks` is `HookKind::None` there, but the
/// failure is now final and must fire `on_error`.
pub fn hook_kind(status: &str) -> HookKind {
    match status.parse::<JobStatus>().ok() {
        Some(JobStatus::Completed) => HookKind::Success,
        Some(JobStatus::Failed) => HookKind::Error,
        Some(JobStatus::Cancelled) => HookKind::Cancel,
        _ => HookKind::None,
    }
}

/// Which terminal side effects `job` gets. Pure.
///
/// `hooks` is `None` on a retry plan: hooks fire only once retries are
/// exhausted, which makes that rule a property of the plan rather than of
/// control flow.
pub fn plan(job: &JobRow) -> TerminalPlan {
    let status = job.status.parse::<JobStatus>().ok();
    let propagate = job.parent_job_id.is_some() && job.parent_step_name.is_some();
    // Child jobs (type: task sub-jobs) never retry — their parent owns the
    // retry decision at the job level.
    let retry = status == Some(JobStatus::Failed)
        && job.parent_job_id.is_none()
        && job.max_retries.is_some_and(|max| job.retry_attempt < max);
    let hooks = if retry {
        HookKind::None
    } else {
        hook_kind(&job.status)
    };
    TerminalPlan {
        propagate,
        retry,
        hooks,
    }
}

/// Perform all side effects for a job that has just reached terminal state:
/// fire hooks, log hook failure to the originating job, notify sync waiters,
/// and upload logs to S3.
pub(super) async fn run_terminal_actions(
    s: &Settlement,
    job: &JobRow,
    workspace: &WorkspaceConfig,
    task: &TaskDef,
    kind: HookKind,
) {
    let job_id = job.job_id;

    // Fire hooks (best-effort). The kind comes from the plan, not from a
    // second status match here; `HookKind::None` fires nothing, which is the
    // `skipped` and retry-pending cases.
    hooks::fire_hooks_of_kind(s, workspace, job, task, kind).await;

    // If a hook job failed, log it to the original job's server events
    if job.source_type == SourceType::Hook.as_ref() && job.status == JobStatus::Failed.as_ref() {
        if let Some(ref source_id) = job.source_id {
            if let Some(original_job_id) = source_id
                .split('/')
                .next()
                .and_then(|id| Uuid::parse_str(id).ok())
            {
                let error_msg = get_hook_error_summary(&s.pool, job).await;
                s.server_log(
                    original_job_id,
                    &format!("[hooks] Hook '{}' failed: {}", job.task_name, error_msg),
                )
                .await;
            }
        }
    }

    // Notify sync webhook waiters
    s.job_completion
        .notify(JobCompletionEvent {
            job_id,
            status: job.status.clone(),
            output: job.output.clone(),
        })
        .await;

    // Flush and close the cached log file handle before S3 upload
    s.log_storage.close_log(job_id).await;

    // S3 upload after hooks so server events are included
    let log_storage = s.log_storage.clone();
    let meta = meta_from_job(job);
    tokio::spawn(async move {
        if let Err(e) = log_storage.upload_to_archive(job_id, &meta).await {
            tracing::warn!(
                "Failed to upload logs to archive for job {}: {:#}",
                job_id,
                e
            );
        }
    });
}

/// Build a minimal TaskDef for hook jobs (which use synthetic task names).
pub(super) async fn build_minimal_task_def(s: &Settlement, job_id: Uuid) -> Result<TaskDef> {
    let steps = JobStepRepo::get_steps_for_job(&s.pool, job_id).await?;
    let mut flow = HashMap::new();
    for step in &steps {
        flow.insert(
            step.step_name.clone(),
            FlowStep {
                action: step.action_name.clone(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                for_each: None,
                sequential: false,
                retry: None,
                inline_action: None,
            },
        );
    }
    Ok(TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,
        retry: None,
        on_success: vec![],
        on_error: vec![],
        on_suspended: vec![],
        on_cancel: vec![],
    })
}

/// Build a short error summary from a hook job's failed steps.
async fn get_hook_error_summary(pool: &PgPool, job: &JobRow) -> String {
    match JobStepRepo::get_steps_for_job(pool, job.job_id).await {
        Ok(steps) => extract_first_failure(&steps),
        Err(_) => "unknown error".to_string(),
    }
}

/// Extract the error message from the first failed step, or "unknown error".
fn extract_first_failure(steps: &[JobStepRow]) -> String {
    for step in steps {
        if step.status == StepStatus::Failed.as_ref() {
            if let Some(ref msg) = step.error_message {
                return msg.clone();
            }
        }
    }
    "unknown error".to_string()
}

/// Upload logs for a job without running hooks or notifying waiters.
/// Used when a job is retried — we want to preserve its logs but skip hooks.
pub(super) async fn upload_logs_for_job(s: &Settlement, job: &JobRow) {
    s.log_storage.close_log(job.job_id).await;
    let log_storage = s.log_storage.clone();
    let meta = meta_from_job(job);
    let job_id = job.job_id;
    tokio::spawn(async move {
        if let Err(e) = log_storage.upload_to_archive(job_id, &meta).await {
            tracing::warn!(
                "Failed to upload logs to archive for retry job {}: {:#}",
                job_id,
                e
            );
        }
    });
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

    #[test]
    fn test_extract_first_failure_with_error() {
        let steps = vec![
            make_step("completed", None),
            make_step("failed", Some("exit code 127: command not found")),
        ];
        assert_eq!(
            extract_first_failure(&steps),
            "exit code 127: command not found"
        );
    }

    #[test]
    fn test_extract_first_failure_no_steps() {
        let steps: Vec<JobStepRow> = vec![];
        assert_eq!(extract_first_failure(&steps), "unknown error");
    }

    #[test]
    fn test_extract_first_failure_failed_without_message() {
        let steps = vec![make_step("failed", None)];
        assert_eq!(extract_first_failure(&steps), "unknown error");
    }

    #[test]
    fn test_extract_first_failure_no_failed_steps() {
        let steps = vec![make_step("completed", None), make_step("completed", None)];
        assert_eq!(extract_first_failure(&steps), "unknown error");
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn job(status: &str, parent: bool, attempt: i32, max: Option<i32>) -> JobRow {
        let mut j = JobRow::test_default();
        j.status = status.to_string();
        if parent {
            j.parent_job_id = Some(Uuid::new_v4());
            j.parent_step_name = Some("child".to_string());
        }
        j.retry_attempt = attempt;
        j.max_retries = max;
        j
    }

    #[test]
    fn failed_top_level_with_budget_retries_without_hooks() {
        let p = plan(&job("failed", false, 0, Some(2)));
        assert_eq!(
            p,
            TerminalPlan {
                propagate: false,
                retry: true,
                hooks: HookKind::None
            }
        );
    }

    #[test]
    fn failed_top_level_exhausted_fires_error_hooks() {
        let p = plan(&job("failed", false, 2, Some(2)));
        assert_eq!(
            p,
            TerminalPlan {
                propagate: false,
                retry: false,
                hooks: HookKind::Error
            }
        );
    }

    #[test]
    fn failed_child_propagates_and_never_retries() {
        let p = plan(&job("failed", true, 0, Some(2)));
        assert_eq!(
            p,
            TerminalPlan {
                propagate: true,
                retry: false,
                hooks: HookKind::Error
            }
        );
    }

    #[test]
    fn cancelled_fires_cancel_hooks() {
        assert_eq!(
            plan(&job("cancelled", false, 0, None)).hooks,
            HookKind::Cancel
        );
    }

    #[test]
    fn completed_fires_success_hooks() {
        assert_eq!(
            plan(&job("completed", false, 0, None)).hooks,
            HookKind::Success
        );
    }

    #[test]
    fn null_max_retries_never_retries() {
        assert!(!plan(&job("failed", false, 0, None)).retry);
    }

    #[test]
    fn hook_kind_maps_terminal_statuses() {
        assert_eq!(hook_kind("completed"), HookKind::Success);
        assert_eq!(hook_kind("failed"), HookKind::Error);
        assert_eq!(hook_kind("cancelled"), HookKind::Cancel);
    }

    #[test]
    fn hook_kind_is_none_for_non_hook_statuses() {
        assert_eq!(hook_kind("skipped"), HookKind::None);
        assert_eq!(hook_kind("running"), HookKind::None);
        assert_eq!(hook_kind("not-a-status"), HookKind::None);
    }

    #[test]
    fn hook_kind_ignores_the_retry_budget_that_plan_honours() {
        // The retry fall-through case: `plan` suppresses hooks while a retry
        // is planned, `hook_kind` still reports the status-only kind.
        let j = job("failed", false, 0, Some(2));
        assert_eq!(plan(&j).hooks, HookKind::None);
        assert_eq!(hook_kind(&j.status), HookKind::Error);
    }
}
