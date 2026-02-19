use crate::orchestrator;
use crate::state::AppState;
use anyhow::Result;
use std::collections::HashMap;
use stroem_common::models::workflow::{FlowStep, TaskDef};
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

/// After a step reaches terminal state, run the orchestrator for its job,
/// handle task steps, and propagate to parent if this is a child job.
///
/// Used by:
/// - `complete_step` handler (worker reports step done)
/// - `recovery` sweeper (step failed due to stale worker)
#[tracing::instrument(skip(state))]
pub async fn orchestrate_after_step(state: &AppState, job_id: Uuid, step_name: &str) -> Result<()> {
    // Get the job to retrieve task info
    let job = match JobRepo::get(&state.pool, job_id).await? {
        Some(j) => j,
        None => {
            tracing::warn!("Job {} not found during orchestration", job_id);
            return Ok(());
        }
    };

    // Get task definition from workspace (with hook fallback)
    let workspace = match state.get_workspace(&job.workspace).await {
        Some(w) => w,
        None => {
            tracing::error!("Workspace '{}' not found", job.workspace);
            return Ok(());
        }
    };

    let task = match workspace.tasks.get(&job.task_name) {
        Some(t) => t.clone(),
        None if job.source_type == "hook" => build_minimal_task_def(state, job_id).await?,
        None => {
            tracing::error!(
                "Task '{}' not found in workspace '{}'",
                job.task_name,
                job.workspace
            );
            return Ok(());
        }
    };

    // Run orchestrator: promote steps, skip unreachable, check terminal
    orchestrator::on_step_completed(&state.pool, job_id, step_name, &task).await?;

    // Handle any newly-promoted type: task steps
    if let Err(e) =
        crate::job_creator::handle_task_steps(&state.pool, &workspace, &job.workspace, job_id).await
    {
        tracing::error!("Failed to handle task steps for job {}: {:#}", job_id, e);
        state
            .append_server_log(
                job_id,
                &format!("[orchestration] Failed to handle task steps: {:#}", e),
            )
            .await;
    }

    // Check if job reached terminal state
    if let Ok(Some(job_after)) = JobRepo::get(&state.pool, job_id).await {
        if job_after.status == "completed" || job_after.status == "failed" {
            // Spawn background S3 upload
            let log_storage = state.log_storage.clone();
            tokio::spawn(async move {
                if let Err(e) = log_storage.upload_to_s3(job_id).await {
                    tracing::warn!("Failed to upload logs to S3 for job {}: {:#}", job_id, e);
                }
            });

            // If this is a child job, propagate to parent
            if let (Some(parent_job_id), Some(ref parent_step)) =
                (job_after.parent_job_id, &job_after.parent_step_name)
            {
                if let Err(e) =
                    propagate_to_parent(state, &job_after, parent_job_id, parent_step).await
                {
                    tracing::error!(
                        "Failed to propagate child job {} to parent {}: {:#}",
                        job_after.job_id,
                        parent_job_id,
                        e
                    );
                    state
                        .append_server_log(
                            job_id,
                            &format!(
                                "[orchestration] Failed to propagate to parent job {}: {:#}",
                                parent_job_id, e
                            ),
                        )
                        .await;
                }
            }

            // Fire hooks (best-effort)
            crate::hooks::fire_hooks(state, &workspace, &job_after, &task).await;
        }
    }

    Ok(())
}

/// Propagate a child job's terminal state to the parent step and orchestrate the parent.
async fn propagate_to_parent(
    state: &AppState,
    child_job: &stroem_db::JobRow,
    parent_job_id: Uuid,
    parent_step: &str,
) -> Result<()> {
    if child_job.status == "completed" {
        JobStepRepo::mark_completed(
            &state.pool,
            parent_job_id,
            parent_step,
            child_job.output.clone(),
        )
        .await?;
    } else {
        let err = format!("Child job {} failed", child_job.job_id);
        JobStepRepo::mark_failed(&state.pool, parent_job_id, parent_step, &err).await?;
    }

    // Get parent job info to orchestrate
    let parent_job = JobRepo::get(&state.pool, parent_job_id).await?;
    if let Some(ref parent_job) = parent_job {
        if let Some(parent_ws) = state.get_workspace(&parent_job.workspace).await {
            let parent_task = match parent_ws.tasks.get(&parent_job.task_name) {
                Some(t) => t.clone(),
                None if parent_job.source_type == "hook" => {
                    build_minimal_task_def(state, parent_job_id).await?
                }
                None => {
                    tracing::warn!(
                        "Parent task '{}' not found in workspace '{}'",
                        parent_job.task_name,
                        parent_job.workspace
                    );
                    return Ok(());
                }
            };

            // Run orchestrator for parent job
            orchestrator::on_step_completed(&state.pool, parent_job_id, parent_step, &parent_task)
                .await?;

            // Handle any newly-promoted task steps in the parent
            crate::job_creator::handle_task_steps(
                &state.pool,
                &parent_ws,
                &parent_job.workspace,
                parent_job_id,
            )
            .await?;

            // Check if parent job is now terminal — propagate recursively
            if let Ok(Some(parent_after)) = JobRepo::get(&state.pool, parent_job_id).await {
                if parent_after.status == "completed" || parent_after.status == "failed" {
                    // S3 upload for parent
                    let log_storage = state.log_storage.clone();
                    let pjid = parent_job_id;
                    tokio::spawn(async move {
                        if let Err(e) = log_storage.upload_to_s3(pjid).await {
                            tracing::warn!(
                                "Failed to upload logs to S3 for parent job {}: {:#}",
                                pjid,
                                e
                            );
                        }
                    });

                    // Propagate up the chain if parent is also a child
                    if let (Some(grandparent_id), Some(ref grandparent_step)) =
                        (parent_after.parent_job_id, &parent_after.parent_step_name)
                    {
                        Box::pin(propagate_to_parent(
                            state,
                            &parent_after,
                            grandparent_id,
                            grandparent_step,
                        ))
                        .await?;
                    }

                    // Fire hooks for the parent
                    crate::hooks::fire_hooks(state, &parent_ws, &parent_after, &parent_task).await;
                }
            }
        }
    }

    Ok(())
}

/// Handle a job that has just reached terminal state (completed or failed).
///
/// Triggers S3 upload, parent propagation, and hooks. Used by `complete_job`
/// handler where the job is marked terminal without going through step-level
/// orchestration.
#[tracing::instrument(skip(state))]
pub async fn handle_job_terminal(state: &AppState, job_id: Uuid) -> Result<()> {
    let job = match JobRepo::get(&state.pool, job_id).await? {
        Some(j) if j.status == "completed" || j.status == "failed" => j,
        _ => return Ok(()),
    };

    // S3 upload
    let log_storage = state.log_storage.clone();
    tokio::spawn(async move {
        if let Err(e) = log_storage.upload_to_s3(job_id).await {
            tracing::warn!("Failed to upload logs to S3 for job {}: {:#}", job_id, e);
        }
    });

    // Propagate to parent
    if let (Some(parent_job_id), Some(ref parent_step)) = (job.parent_job_id, &job.parent_step_name)
    {
        if let Err(e) = propagate_to_parent(state, &job, parent_job_id, parent_step).await {
            tracing::error!(
                "Failed to propagate child job {} to parent {}: {:#}",
                job.job_id,
                parent_job_id,
                e
            );
            state
                .append_server_log(
                    job_id,
                    &format!(
                        "[orchestration] Failed to propagate to parent job {}: {:#}",
                        parent_job_id, e
                    ),
                )
                .await;
        }
    }

    // Fire hooks
    if let Some(workspace) = state.get_workspace(&job.workspace).await {
        let task = match workspace.tasks.get(&job.task_name) {
            Some(t) => t.clone(),
            None if job.source_type == "hook" => build_minimal_task_def(state, job_id).await?,
            None => return Ok(()),
        };
        crate::hooks::fire_hooks(state, &workspace, &job, &task).await;
    }

    Ok(())
}

/// Build a minimal TaskDef for hook jobs (which use synthetic task names).
async fn build_minimal_task_def(state: &AppState, job_id: Uuid) -> Result<TaskDef> {
    let steps = JobStepRepo::get_steps_for_job(&state.pool, job_id).await?;
    let mut flow = HashMap::new();
    for step in &steps {
        flow.insert(
            step.step_name.clone(),
            FlowStep {
                action: step.action_name.clone(),
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
    }
    Ok(TaskDef {
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        on_success: vec![],
        on_error: vec![],
    })
}
