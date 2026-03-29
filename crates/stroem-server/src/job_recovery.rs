use crate::job_completion::JobCompletionEvent;
use crate::log_storage::JobLogMeta;
use crate::orchestrator;
use crate::state::AppState;
use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::job::{JobStatus, SourceType, StepStatus};
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
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
        None if job.source_type == SourceType::Hook.as_ref()
            || job.source_type == SourceType::EventSource.as_ref() =>
        {
            build_minimal_task_def(state, job_id).await?
        }
        None => {
            tracing::error!(
                "Task '{}' not found in workspace '{}'",
                job.task_name,
                job.workspace
            );
            return Ok(());
        }
    };

    // Check if this is a loop instance completing — handle sequential promotion
    // and loop completion before running the orchestrator
    if let Err(e) =
        crate::job_creator::check_loop_completion(&state.pool, job_id, step_name, &task).await
    {
        tracing::error!(
            "Failed to check loop completion for job {} step '{}': {:#}",
            job_id,
            step_name,
            e
        );
        state
            .append_server_log(
                job_id,
                &format!("[orchestration] Failed to check loop completion: {:#}", e),
            )
            .await;
    }

    // Run orchestrator: promote steps, skip unreachable, check terminal
    orchestrator::on_step_completed(&state.pool, job_id, step_name, &task, Some(&workspace))
        .await?;

    // Expand any for_each steps that became eligible after orchestration
    if let Err(e) = crate::job_creator::expand_for_each_steps(
        &state.pool,
        &workspace,
        &job.workspace,
        job_id,
        &task,
    )
    .await
    {
        tracing::error!(
            "Failed to expand for_each steps for job {}: {:#}",
            job_id,
            e
        );
        state
            .append_server_log(
                job_id,
                &format!("[orchestration] Failed to expand for_each steps: {:#}", e),
            )
            .await;
    }

    // Handle any newly-promoted type: task steps (including loop instances)
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

    // Handle any newly-promoted type: approval steps and fire on_suspended hooks
    {
        // Snapshot steps before suspension to detect which steps just became suspended
        let steps_before = stroem_db::JobStepRepo::get_steps_for_job(&state.pool, job_id).await?;
        let previously_suspended: std::collections::HashSet<&str> = steps_before
            .iter()
            .filter(|s| s.status == stroem_common::models::job::StepStatus::Suspended.as_ref())
            .map(|s| s.step_name.as_str())
            .collect();

        if let Err(e) = crate::job_creator::handle_approval_steps(
            &state.pool,
            &workspace,
            &job.workspace,
            job_id,
            &task,
        )
        .await
        {
            tracing::error!(
                "Failed to handle approval steps for job {}: {:#}",
                job_id,
                e
            );
            state
                .append_server_log(
                    job_id,
                    &format!("[orchestration] Failed to handle approval steps: {:#}", e),
                )
                .await;
        }

        // Fire on_suspended hooks for steps that newly entered suspended state
        let steps_after = stroem_db::JobStepRepo::get_steps_for_job(&state.pool, job_id).await?;
        for step in &steps_after {
            if step.status == stroem_common::models::job::StepStatus::Suspended.as_ref()
                && !previously_suspended.contains(step.step_name.as_str())
            {
                let rendered_message = step
                    .output
                    .as_ref()
                    .and_then(|o| o["approval_message"].as_str())
                    .unwrap_or("")
                    .to_string();

                state
                    .append_server_log(
                        job_id,
                        &format!("[approval] Step '{}' waiting for approval", step.step_name),
                    )
                    .await;

                crate::hooks::fire_suspended_hooks(
                    state,
                    &workspace,
                    &job,
                    &task,
                    &step.step_name,
                    &rendered_message,
                )
                .await;
            }
        }
    }

    // Check if job reached terminal state
    if let Ok(Some(job_after)) = JobRepo::get(&state.pool, job_id).await {
        if matches!(
            job_after.status.parse::<JobStatus>().ok(),
            Some(JobStatus::Completed)
                | Some(JobStatus::Failed)
                | Some(JobStatus::Cancelled)
                | Some(JobStatus::Skipped)
        ) {
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

            // Fire hooks, notify waiters, upload to S3.
            // This intentionally runs for child jobs too (source_type == "task"):
            // - fire_hooks() already skips workspace-level hooks for non-top-level jobs
            // - task-level hooks should fire regardless of how the task was invoked
            // - S3 upload is per-job (each child has its own log file)
            // - job_completion.notify() is a no-op when no sync waiters exist
            run_terminal_job_actions(state, &job_after, &workspace, &task).await;
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
    // Special handling for agent_tool child jobs: the parent step is an agent step
    // that needs to resume its dispatch loop on a worker. Instead of marking completed/failed,
    // we inject the tool result into agent_state and mark the step ready for re-claim.
    if child_job.source_type == "agent_tool" {
        let parent_steps = JobStepRepo::get_steps_for_job(&state.pool, parent_job_id).await?;
        if let Some(parent_step_row) = parent_steps.iter().find(|s| s.step_name == parent_step) {
            if let Some(ref state_val) = parent_step_row.agent_state {
                if let Ok(mut conv_state) = serde_json::from_value::<
                    stroem_agent::state::AgentConversationState,
                >(state_val.clone())
                {
                    let tool_result_text = if child_job.status == JobStatus::Completed.as_ref() {
                        child_job
                            .output
                            .as_ref()
                            .map(|o| serde_json::to_string(o).unwrap_or_default())
                            .unwrap_or_else(|| "Task completed successfully".to_string())
                    } else {
                        format!("Task failed: {}", child_job.status)
                    };

                    if let Some(resolved) = conv_state.resolve_tool_call(child_job.job_id) {
                        conv_state.resolved_tool_results.push(
                            stroem_agent::state::ResolvedToolResult {
                                tool_call_id: resolved.tool_call_id,
                                result_text: tool_result_text,
                            },
                        );
                    }

                    let updated_state = serde_json::to_value(&conv_state)
                        .context("serialize agent conversation state")?;
                    JobStepRepo::update_agent_state(
                        &state.pool,
                        parent_job_id,
                        parent_step,
                        updated_state,
                    )
                    .await?;

                    if conv_state.all_tool_calls_resolved() {
                        // All tools done — mark step ready so a worker can re-claim it
                        sqlx::query(
                            "UPDATE job_step SET status = 'ready', ready_at = NOW(), worker_id = NULL \
                             WHERE job_id = $1 AND step_name = $2 AND status = 'running'",
                        )
                        .bind(parent_job_id)
                        .bind(parent_step)
                        .execute(&state.pool)
                        .await?;

                        tracing::info!(
                            parent_job_id = %parent_job_id,
                            step = %parent_step,
                            "Agent step marked ready for re-claim after all task tools completed"
                        );
                    } else {
                        tracing::info!(
                            parent_job_id = %parent_job_id,
                            step = %parent_step,
                            pending = conv_state.pending_tool_calls.len(),
                            "Agent tool completed, still waiting for more tools"
                        );
                    }

                    return Ok(());
                }
            }
        }

        // Fallback: if we could not parse agent state, fall through to normal propagation
        tracing::warn!(
            parent_job_id = %parent_job_id,
            step = %parent_step,
            "Could not process agent_tool child completion — falling through to normal propagation"
        );
    }

    if child_job.status == JobStatus::Completed.as_ref() {
        JobStepRepo::mark_completed(
            &state.pool,
            parent_job_id,
            parent_step,
            child_job.output.clone(),
        )
        .await?;
    } else if child_job.status == JobStatus::Cancelled.as_ref() {
        JobStepRepo::mark_cancelled(&state.pool, parent_job_id, parent_step).await?;
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
                None if parent_job.source_type == SourceType::Hook.as_ref()
                    || parent_job.source_type == SourceType::EventSource.as_ref() =>
                {
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

            // Check if the completed parent step is a loop instance — handle
            // sequential promotion and loop completion before orchestrating
            if let Err(e) = crate::job_creator::check_loop_completion(
                &state.pool,
                parent_job_id,
                parent_step,
                &parent_task,
            )
            .await
            {
                tracing::error!(
                    "Failed to check loop completion for parent step '{}': {:#}",
                    parent_step,
                    e
                );
            }

            // Run orchestrator for parent job
            orchestrator::on_step_completed(
                &state.pool,
                parent_job_id,
                parent_step,
                &parent_task,
                Some(&parent_ws),
            )
            .await?;

            // Expand any for_each steps in the parent job
            if let Err(e) = crate::job_creator::expand_for_each_steps(
                &state.pool,
                &parent_ws,
                &parent_job.workspace,
                parent_job_id,
                &parent_task,
            )
            .await
            {
                tracing::error!(
                    "Failed to expand for_each steps for parent job {}: {:#}",
                    parent_job_id,
                    e
                );
            }

            // Handle any newly-promoted task steps in the parent
            crate::job_creator::handle_task_steps(
                &state.pool,
                &parent_ws,
                &parent_job.workspace,
                parent_job_id,
            )
            .await?;

            // Handle any newly-promoted approval steps in the parent,
            // and fire on_suspended hooks for steps that just became suspended (FIX 3).
            {
                let steps_before =
                    stroem_db::JobStepRepo::get_steps_for_job(&state.pool, parent_job_id).await?;
                let pre_suspended: std::collections::HashSet<&str> = steps_before
                    .iter()
                    .filter(|s| {
                        s.status == stroem_common::models::job::StepStatus::Suspended.as_ref()
                    })
                    .map(|s| s.step_name.as_str())
                    .collect();

                if let Err(e) = crate::job_creator::handle_approval_steps(
                    &state.pool,
                    &parent_ws,
                    &parent_job.workspace,
                    parent_job_id,
                    &parent_task,
                )
                .await
                {
                    tracing::error!(
                        "Failed to handle approval steps for parent job {}: {:#}",
                        parent_job_id,
                        e
                    );
                }

                let steps_after =
                    stroem_db::JobStepRepo::get_steps_for_job(&state.pool, parent_job_id).await?;
                for step in &steps_after {
                    if step.status == stroem_common::models::job::StepStatus::Suspended.as_ref()
                        && !pre_suspended.contains(step.step_name.as_str())
                    {
                        let rendered_message = step
                            .output
                            .as_ref()
                            .and_then(|o| o["approval_message"].as_str())
                            .unwrap_or("")
                            .to_string();

                        state
                            .append_server_log(
                                parent_job_id,
                                &format!(
                                    "[approval] Step '{}' waiting for approval",
                                    step.step_name
                                ),
                            )
                            .await;

                        crate::hooks::fire_suspended_hooks(
                            state,
                            &parent_ws,
                            parent_job,
                            &parent_task,
                            &step.step_name,
                            &rendered_message,
                        )
                        .await;
                    }
                }
            }

            // Check if parent job is now terminal — propagate recursively
            if let Ok(Some(parent_after)) = JobRepo::get(&state.pool, parent_job_id).await {
                if matches!(
                    parent_after.status.parse::<JobStatus>().ok(),
                    Some(JobStatus::Completed)
                        | Some(JobStatus::Failed)
                        | Some(JobStatus::Cancelled)
                        | Some(JobStatus::Skipped)
                ) {
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

                    // Fire hooks, notify waiters, upload to S3
                    run_terminal_job_actions(state, &parent_after, &parent_ws, &parent_task).await;
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
        Some(j)
            if matches!(
                j.status.parse::<JobStatus>().ok(),
                Some(JobStatus::Completed)
                    | Some(JobStatus::Failed)
                    | Some(JobStatus::Cancelled)
                    | Some(JobStatus::Skipped)
            ) =>
        {
            j
        }
        _ => return Ok(()),
    };

    // Remove from the in-memory cancelled set to prevent unbounded growth.
    // Safe to call unconditionally — no-op if not present.
    crate::cancellation::clear_cancelled(state, job_id);

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

    // Fire hooks, notify waiters, upload to S3 (best-effort, only if workspace/task can be resolved)
    if let Some(workspace) = state.get_workspace(&job.workspace).await {
        let task = match workspace.tasks.get(&job.task_name) {
            Some(t) => Some(t.clone()),
            None if job.source_type == SourceType::Hook.as_ref()
                || job.source_type == SourceType::EventSource.as_ref() =>
            {
                Some(build_minimal_task_def(state, job_id).await?)
            }
            None => None,
        };
        if let Some(task) = task {
            run_terminal_job_actions(state, &job, &workspace, &task).await;
        }
    } else {
        tracing::warn!(
            "Workspace '{}' not found for terminal job {} — skipping hooks and S3 upload",
            job.workspace,
            job_id
        );
    }

    Ok(())
}

/// Perform all side effects for a job that has just reached terminal state:
/// fire hooks, log hook failure to the originating job, notify sync waiters,
/// and upload logs to S3. This consolidates logic that is otherwise duplicated
/// across `orchestrate_after_step`, `propagate_to_parent`, and `handle_job_terminal`.
async fn run_terminal_job_actions(
    state: &AppState,
    job: &JobRow,
    workspace: &WorkspaceConfig,
    task: &TaskDef,
) {
    let job_id = job.job_id;

    // Fire hooks (best-effort)
    crate::hooks::fire_hooks(state, workspace, job, task).await;

    // If a hook job failed, log it to the original job's server events
    if job.source_type == SourceType::Hook.as_ref() && job.status == JobStatus::Failed.as_ref() {
        if let Some(ref source_id) = job.source_id {
            if let Some(original_job_id) = source_id
                .split('/')
                .next()
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                let error_msg = get_hook_error_summary(&state.pool, job).await;
                state
                    .append_server_log(
                        original_job_id,
                        &format!("[hooks] Hook '{}' failed: {}", job.task_name, error_msg),
                    )
                    .await;
            }
        }
    }

    // Notify sync webhook waiters
    state
        .job_completion
        .notify(JobCompletionEvent {
            job_id,
            status: job.status.clone(),
            output: job.output.clone(),
        })
        .await;

    // Flush and close the cached log file handle before S3 upload
    state.log_storage.close_log(job_id).await;

    // S3 upload after hooks so server events are included
    let log_storage = state.log_storage.clone();
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
async fn build_minimal_task_def(state: &AppState, job_id: Uuid) -> Result<TaskDef> {
    let steps = JobStepRepo::get_steps_for_job(&state.pool, job_id).await?;
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
        on_success: vec![],
        on_error: vec![],
        on_suspended: vec![],
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
            action_image: None,
            action_spec: None,
            input: None,
            output: None,
            status: status.to_string(), // DB model stays as String
            worker_id: None,
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            error_message: error_message.map(String::from),
            required_tags: json!([]),
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
            for_each_expr: None,
            loop_source: None,
            loop_index: None,
            loop_total: None,
            loop_item: None,
            agent_state: None,
            suspended_at: None,
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
