use anyhow::{Context, Result};
use sqlx::PgPool;
use stroem_common::models::workflow::TaskDef;
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

/// Handle step completion and orchestrate next steps
#[tracing::instrument(skip(pool, task))]
pub async fn on_step_completed(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    task: &TaskDef,
) -> Result<()> {
    tracing::info!("Orchestrating after step '{}' completed", step_name);

    // 1. Promote pending steps to ready if their dependencies are met
    let promoted = JobStepRepo::promote_ready_steps(pool, job_id, &task.flow)
        .await
        .context("Failed to promote ready steps")?;

    if !promoted.is_empty() {
        tracing::info!("Promoted steps to ready: {:?}", promoted);
    }

    // 2. Check if all steps are terminal (completed/failed/skipped)
    let all_terminal = JobStepRepo::all_steps_terminal(pool, job_id)
        .await
        .context("Failed to check if all steps are terminal")?;

    if all_terminal {
        // 3. Check if any step failed
        let any_failed = JobStepRepo::any_step_failed(pool, job_id)
            .await
            .context("Failed to check if any step failed")?;

        if any_failed {
            tracing::info!("Job {} failed (one or more steps failed)", job_id);
            JobRepo::mark_failed(pool, job_id)
                .await
                .context("Failed to mark job as failed")?;
        } else {
            tracing::info!("Job {} completed successfully", job_id);
            JobRepo::mark_completed(pool, job_id, None)
                .await
                .context("Failed to mark job as completed")?;
        }
    }

    Ok(())
}
