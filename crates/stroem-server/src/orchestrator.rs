use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashSet;
use stroem_common::models::job::JobStatus;
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

/// Handle step completion and orchestrate next steps.
///
/// `workspace_config` is optional — when provided, `when` conditions on steps
/// are evaluated (secrets are available for template rendering). When `None`,
/// steps with `when` conditions stay pending until a context is available.
#[tracing::instrument(skip(pool, task, workspace_config))]
pub async fn on_step_completed(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    task: &TaskDef,
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<()> {
    tracing::info!("Orchestrating after step '{}' completed", step_name);

    // Fetch job_row once — it doesn't change during orchestration, but is only
    // needed when workspace_config is provided (for render context construction).
    let job_row = if workspace_config.is_some() {
        Some(JobRepo::get(pool, job_id).await?.context("Job not found")?)
    } else {
        None
    };

    // 1. Promote pending steps to ready if their dependencies are met.
    //    Loop because conditional skips may cascade and unblock further steps.
    //    Rebuild the render context each iteration so newly-skipped steps are
    //    visible to subsequent `when` condition evaluations.
    //    Safety bound: at most (flow length + 1) iterations — each iteration
    //    must promote or skip at least one step, so this bound can never be
    //    reached in correct operation.
    let max_iterations = task.flow.len() + 1;
    for _iteration in 0..max_iterations {
        // Rebuild render context from fresh step data each iteration
        let render_ctx = if let Some(ws_config) = workspace_config {
            let steps_snapshot = JobStepRepo::get_steps_for_job(pool, job_id).await?;
            Some(crate::job_creator::build_step_render_context(
                job_row.as_ref().unwrap(),
                &steps_snapshot,
                ws_config,
            ))
        } else {
            None
        };

        // TODO(optimize): promote_ready_steps and skip_unreachable_steps each
        // call get_steps_for_job internally, so this loop issues two separate
        // DB fetches per iteration. A future refactor could load the step list
        // once and pass it into both functions to halve the round-trips.
        let changed =
            JobStepRepo::promote_ready_steps(pool, job_id, &task.flow, render_ctx.as_ref())
                .await
                .context("Failed to promote ready steps")?;

        if !changed.is_empty() {
            tracing::info!("Promoted/skipped steps: {:?}", changed);
        }

        // Skip unreachable pending steps (cascade until stable)
        let skipped = JobStepRepo::skip_unreachable_steps(pool, job_id, &task.flow)
            .await
            .context("Failed to skip unreachable steps")?;

        if !skipped.is_empty() {
            tracing::info!("Skipped unreachable steps: {:?}", skipped);
        }

        // If nothing changed in this iteration, we're stable
        if changed.is_empty() && skipped.is_empty() {
            break;
        }

        if _iteration + 1 == max_iterations {
            tracing::warn!(
                job_id = %job_id,
                "Cascade loop reached iteration limit ({}) — breaking to avoid infinite loop",
                max_iterations
            );
        }
    }

    // 2. Check if all steps are terminal (completed/failed/skipped/cancelled)
    let all_terminal = JobStepRepo::all_steps_terminal(pool, job_id)
        .await
        .context("Failed to check if all steps are terminal")?;

    if all_terminal {
        // If the job is already cancelled, don't overwrite it
        let current_job = JobRepo::get(pool, job_id).await?;
        if let Some(ref j) = current_job {
            if j.status == JobStatus::Cancelled.as_ref() {
                tracing::info!(
                    "Job {} is already cancelled, skipping status update",
                    job_id
                );
                return Ok(());
            }
        }
        // 3. Check if any step failed
        let failed_names = JobStepRepo::get_failed_step_names(pool, job_id)
            .await
            .context("Failed to get failed step names")?;

        if !failed_names.is_empty() {
            // Check if ALL failures are tolerable (continue_on_failure steps).
            // Loop instance steps (e.g. "process[0]") are not in task.flow —
            // look up by their source name instead. Loop instance failures
            // are managed by `check_loop_completion` via the placeholder step.
            let all_tolerable = failed_names.iter().all(|name| {
                let flow_name = if let Some(bracket) = name.find('[') {
                    &name[..bracket]
                } else {
                    name.as_str()
                };
                task.flow
                    .get(flow_name)
                    .map(|fs| fs.continue_on_failure)
                    .unwrap_or(false)
            });

            if !all_tolerable {
                tracing::info!("Job {} failed (one or more steps failed)", job_id);
                JobRepo::mark_failed(pool, job_id)
                    .await
                    .context("Failed to mark job as failed")?;
                return Ok(());
            }

            tracing::info!(
                "Job {} completed with {} tolerable failure(s)",
                job_id,
                failed_names.len()
            );
        } else {
            tracing::info!("Job {} completed successfully", job_id);
        }

        // Collect output from terminal steps (steps no other step depends on)
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

    Ok(())
}
