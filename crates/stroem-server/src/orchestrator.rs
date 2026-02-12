use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashSet;
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

    // 2. Skip unreachable pending steps (cascade until stable)
    loop {
        let skipped = JobStepRepo::skip_unreachable_steps(pool, job_id, &task.flow)
            .await
            .context("Failed to skip unreachable steps")?;
        if skipped.is_empty() {
            break;
        }
        tracing::info!("Skipped unreachable steps: {:?}", skipped);
    }

    // 3. Check if all steps are terminal (completed/failed/skipped)
    let all_terminal = JobStepRepo::all_steps_terminal(pool, job_id)
        .await
        .context("Failed to check if all steps are terminal")?;

    if all_terminal {
        // 4. Check if any step failed
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

            // Collect output from terminal steps (steps no other step depends on)
            let depended_on: HashSet<&str> = task
                .flow
                .iter()
                .flat_map(|(_, fs)| fs.depends_on.iter().map(|s| s.as_str()))
                .collect();
            let terminal_steps: HashSet<&str> = task
                .flow
                .keys()
                .filter(|name| !depended_on.contains(name.as_str()))
                .map(|s| s.as_str())
                .collect();

            let steps = JobStepRepo::get_steps_for_job(pool, job_id)
                .await
                .context("Failed to get steps for job output aggregation")?;
            let mut job_output = serde_json::Map::new();
            for step in &steps {
                if terminal_steps.contains(step.step_name.as_str()) {
                    if let Some(ref output) = step.output {
                        job_output.insert(step.step_name.clone(), output.clone());
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
        }
    }

    Ok(())
}
