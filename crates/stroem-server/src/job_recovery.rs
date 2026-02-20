use crate::log_storage::JobLogMeta;
use crate::orchestrator;
use crate::state::AppState;
use anyhow::Result;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{FlowStep, TaskDef};
use stroem_db::{JobRepo, JobRow, JobStepRepo, JobStepRow};
use uuid::Uuid;

/// Build a `JobLogMeta` from a `JobRow`.
fn meta_from_job(job: &JobRow) -> JobLogMeta {
    JobLogMeta {
        workspace: job.workspace.clone(),
        task_name: job.task_name.clone(),
        created_at: job.created_at,
    }
}

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

            // If a hook job failed, log it to the original job's server events
            if job_after.source_type == "hook" && job_after.status == "failed" {
                if let Some(ref source_id) = job_after.source_id {
                    if let Ok(original_job_id) = Uuid::parse_str(source_id) {
                        let error_msg = get_hook_error_summary(&state.pool, &job_after).await;
                        state
                            .append_server_log(
                                original_job_id,
                                &format!(
                                    "[hooks] Hook '{}' failed: {}",
                                    job_after.task_name, error_msg
                                ),
                            )
                            .await;
                    }
                }
            }

            // S3 upload after hooks so server events are included
            let log_storage = state.log_storage.clone();
            let meta = meta_from_job(&job_after);
            tokio::spawn(async move {
                if let Err(e) = log_storage.upload_to_s3(job_id, &meta).await {
                    tracing::warn!("Failed to upload logs to S3 for job {}: {:#}", job_id, e);
                }
            });
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

                    // If a hook job failed, log it to the original job's server events
                    if parent_after.source_type == "hook" && parent_after.status == "failed" {
                        if let Some(ref source_id) = parent_after.source_id {
                            if let Ok(original_job_id) = Uuid::parse_str(source_id) {
                                let error_msg =
                                    get_hook_error_summary(&state.pool, &parent_after).await;
                                state
                                    .append_server_log(
                                        original_job_id,
                                        &format!(
                                            "[hooks] Hook '{}' failed: {}",
                                            parent_after.task_name, error_msg
                                        ),
                                    )
                                    .await;
                            }
                        }
                    }

                    // S3 upload for parent after hooks
                    let log_storage = state.log_storage.clone();
                    let pjid = parent_job_id;
                    let meta = meta_from_job(&parent_after);
                    tokio::spawn(async move {
                        if let Err(e) = log_storage.upload_to_s3(pjid, &meta).await {
                            tracing::warn!(
                                "Failed to upload logs to S3 for parent job {}: {:#}",
                                pjid,
                                e
                            );
                        }
                    });
                }
            }
        }
    }

    Ok(())
}

/// Handle a job that has just reached terminal state (completed or failed).
///
/// Triggers parent propagation, hooks, then S3 upload. Used by `complete_job`
/// handler where the job is marked terminal without going through step-level
/// orchestration.
#[tracing::instrument(skip(state))]
pub async fn handle_job_terminal(state: &AppState, job_id: Uuid) -> Result<()> {
    let job = match JobRepo::get(&state.pool, job_id).await? {
        Some(j) if j.status == "completed" || j.status == "failed" => j,
        _ => return Ok(()),
    };

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

    // Fire hooks (best-effort, only if workspace/task can be resolved)
    if let Some(workspace) = state.get_workspace(&job.workspace).await {
        let task = match workspace.tasks.get(&job.task_name) {
            Some(t) => Some(t.clone()),
            None if job.source_type == "hook" => Some(build_minimal_task_def(state, job_id).await?),
            None => None,
        };
        if let Some(task) = task {
            crate::hooks::fire_hooks(state, &workspace, &job, &task).await;
        }
    }

    // If a hook job failed, log it to the original job's server events
    if job.source_type == "hook" && job.status == "failed" {
        if let Some(ref source_id) = job.source_id {
            if let Ok(original_job_id) = Uuid::parse_str(source_id) {
                let error_msg = get_hook_error_summary(&state.pool, &job).await;
                state
                    .append_server_log(
                        original_job_id,
                        &format!("[hooks] Hook '{}' failed: {}", job.task_name, error_msg),
                    )
                    .await;
            }
        }
    }

    // S3 upload after hooks so server events are included
    let log_storage = state.log_storage.clone();
    let meta = meta_from_job(&job);
    tokio::spawn(async move {
        if let Err(e) = log_storage.upload_to_s3(job_id, &meta).await {
            tracing::warn!("Failed to upload logs to S3 for job {}: {:#}", job_id, e);
        }
    });

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
        if step.status == "failed" {
            if let Some(ref msg) = step.error_message {
                return msg.clone();
            }
        }
    }
    "unknown error".to_string()
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
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            output: None,
            status: status.to_string(),
            worker_id: None,
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            error_message: error_message.map(String::from),
            required_tags: json!([]),
            runner: "local".to_string(),
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
