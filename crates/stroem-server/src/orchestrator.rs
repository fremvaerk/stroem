use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashSet;
use stroem_common::models::job::{JobStatus, StepStatus};
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

/// Handle step completion and orchestrate next steps.
///
/// `workspace_config` is optional — when provided, `when` conditions and
/// `for_each` expressions are evaluated (secrets are available for template
/// rendering). When `None`, steps that need a render context stay pending until
/// one is available; loop rollup and sequential advance still run, since they
/// need no context.
#[tracing::instrument(skip(pool, task, workspace_config))]
pub async fn on_step_completed(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    task: &TaskDef,
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<()> {
    tracing::info!("Orchestrating after step '{}' completed", step_name);

    // 1. Move every step the cascade can move: rollup/advance loops, promote,
    //    cascade-skip, retire/expand placeholders — one pure fixpoint, one
    //    transaction (see cascade.rs).
    crate::cascade::execute(pool, job_id, task, workspace_config)
        .await
        .context("Failed to run step cascade")?;

    // 2. Settle the job if every step is terminal.
    settle_if_all_terminal(pool, job_id, task).await?;
    Ok(())
}

/// If every step of the job is terminal, decide and persist the job's final
/// status and return it; otherwise return `None` and touch nothing.
///
/// Single source of truth for terminal settlement — called from
/// `on_step_completed` AND from job creation (`create_job_for_task_inner`), so
/// a job that is already terminal at creation (all steps skipped by `when`, or
/// a server-dispatched root step that failed) gets exactly the same rules:
/// `continue_on_failure` tolerance, `cancelled` propagation, and aggregated
/// output of the flow's terminal steps.
#[tracing::instrument(skip(pool, task))]
pub async fn settle_if_all_terminal(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
) -> Result<Option<JobStatus>> {
    let all_terminal = JobStepRepo::all_steps_terminal(pool, job_id)
        .await
        .context("Failed to check if all steps are terminal")?;
    if !all_terminal {
        return Ok(None);
    }

    // Never overwrite an explicit cancellation.
    if let Some(j) = JobRepo::get(pool, job_id).await? {
        if j.status == JobStatus::Cancelled.as_ref() {
            tracing::info!(
                "Job {} is already cancelled, skipping status update",
                job_id
            );
            return Ok(Some(JobStatus::Cancelled));
        }
    }

    let steps = JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .context("Failed to get steps for settlement")?;

    // Loop instance steps ("process[0]") are not in task.flow — look up by
    // their placeholder name. Instance failures are already folded into the
    // placeholder by the cascade's rollup rule (R6).
    let flow_name = |name: &str| -> String {
        match name.find('[') {
            Some(i) => name[..i].to_string(),
            None => name.to_string(),
        }
    };
    let tolerated = |name: &str| -> bool {
        task.flow
            .get(&flow_name(name))
            .map(|fs| fs.continue_on_failure)
            .unwrap_or(false)
    };

    let untolerated_failure = steps
        .iter()
        .any(|s| s.status == StepStatus::Failed.as_ref() && !tolerated(&s.step_name));
    if untolerated_failure {
        tracing::info!("Job {} failed (one or more steps failed)", job_id);
        JobRepo::mark_failed(pool, job_id)
            .await
            .context("Failed to mark job as failed")?;
        return Ok(Some(JobStatus::Failed));
    }

    let any_cancelled = steps
        .iter()
        .any(|s| s.status == StepStatus::Cancelled.as_ref());
    if any_cancelled {
        tracing::info!(
            "Job {} cancelled (a step was cancelled, no untolerated failure)",
            job_id
        );
        JobRepo::mark_cancelled(pool, job_id)
            .await
            .context("Failed to mark job as cancelled")?;
        return Ok(Some(JobStatus::Cancelled));
    }

    let failed_count = steps
        .iter()
        .filter(|s| s.status == StepStatus::Failed.as_ref())
        .count();
    if failed_count > 0 {
        tracing::info!(
            "Job {} completed with {} tolerable failure(s)",
            job_id,
            failed_count
        );
    } else {
        tracing::info!("Job {} completed successfully", job_id);
    }

    // Output = outputs of the flow's terminal steps (nothing depends on them).
    let depended_on: HashSet<&str> = task
        .flow
        .values()
        .flat_map(|fs| fs.depends_on.iter().map(|s| s.as_str()))
        .collect();
    let terminal_steps: HashSet<&str> = task
        .flow
        .keys()
        .filter(|name| !depended_on.contains(name.as_str()))
        .map(|s| s.as_str())
        .collect();
    let mut job_output = serde_json::Map::new();
    for s in &steps {
        if terminal_steps.contains(s.step_name.as_str()) {
            if let Some(ref output) = s.output {
                job_output.insert(s.step_name.clone(), output.clone());
            }
        }
    }
    let output = if job_output.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(job_output))
    };
    JobRepo::mark_completed(pool, job_id, output)
        .await
        .context("Failed to mark job as completed")?;
    Ok(Some(JobStatus::Completed))
}
