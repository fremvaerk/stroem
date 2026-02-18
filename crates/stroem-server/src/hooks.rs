use crate::state::AppState;
use anyhow::Context;
use serde::Serialize;
use sqlx::PgPool;
use stroem_common::models::workflow::{HookDef, TaskDef, WorkspaceConfig};
use stroem_common::template::render_input_map;
use stroem_common::validation::{compute_required_tags, derive_runner};
use stroem_db::{JobRepo, JobStepRepo, NewJobStep};

/// Context available to hook templates as `hook.*`
#[derive(Debug, Serialize)]
pub struct HookContext {
    pub workspace: String,
    pub task_name: String,
    pub job_id: String,
    pub status: String,
    pub is_success: bool,
    pub error_message: Option<String>,
    pub source_type: String,
    pub source_id: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub duration_secs: Option<f64>,
    pub failed_steps: Vec<FailedStepInfo>,
}

/// Info about a single failed step, available in `hook.failed_steps`
#[derive(Debug, Serialize)]
pub struct FailedStepInfo {
    pub step_name: String,
    pub action_name: String,
    pub error_message: Option<String>,
    pub continue_on_failure: bool,
}

/// Fire hooks for a job that has reached a terminal state.
///
/// - Jobs with `source_type = "hook"` never trigger further hooks (recursion guard).
/// - Selects `on_success` or `on_error` hooks based on job status.
/// - Each hook creates a new single-step job with `source_type = "hook"`.
/// - Failures are logged but never affect the original job.
#[tracing::instrument(skip(state, workspace_config, task))]
pub async fn fire_hooks(
    state: &AppState,
    workspace_config: &WorkspaceConfig,
    job: &stroem_db::JobRow,
    task: &TaskDef,
) {
    // Recursion guard: hook jobs never trigger further hooks
    if job.source_type == "hook" {
        return;
    }

    // Only fire on terminal states
    let hooks: &[HookDef] = match job.status.as_str() {
        "completed" => &task.on_success,
        "failed" => &task.on_error,
        _ => return,
    };

    if hooks.is_empty() {
        return;
    }

    // Build hook context
    let ctx = match build_hook_context(&state.pool, job, task).await {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!("Failed to build hook context for job {}: {}", job.job_id, e);
            state
                .append_server_log(
                    job.job_id,
                    &format!("[hooks] Failed to build hook context: {}", e),
                )
                .await;
            return;
        }
    };

    let ctx_value = match serde_json::to_value(&ctx) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to serialize hook context: {}", e);
            state
                .append_server_log(
                    job.job_id,
                    &format!("[hooks] Failed to serialize hook context: {}", e),
                )
                .await;
            return;
        }
    };

    let hook_type = if job.status == "completed" {
        "on_success"
    } else {
        "on_error"
    };

    for (i, hook) in hooks.iter().enumerate() {
        let source_id = format!(
            "{}/{}/{}/{}[{}]",
            job.workspace, job.task_name, job.job_id, hook_type, i
        );

        if let Err(e) = fire_single_hook(
            &state.pool,
            workspace_config,
            &job.workspace,
            hook,
            &ctx_value,
            &source_id,
        )
        .await
        {
            tracing::error!(
                "Failed to fire hook {}[{}] for job {}: {}",
                hook_type,
                i,
                job.job_id,
                e
            );
            state
                .append_server_log(
                    job.job_id,
                    &format!(
                        "[hooks] Failed to fire hook {}[{}] for action '{}': {}",
                        hook_type, i, hook.action, e
                    ),
                )
                .await;
        }
    }
}

async fn build_hook_context(
    pool: &PgPool,
    job: &stroem_db::JobRow,
    task: &TaskDef,
) -> anyhow::Result<HookContext> {
    let steps = JobStepRepo::get_steps_for_job(pool, job.job_id)
        .await
        .context("Failed to get steps for hook context")?;

    let failed_steps: Vec<FailedStepInfo> = steps
        .iter()
        .filter(|s| s.status == "failed")
        .map(|s| {
            let continue_on_failure = task
                .flow
                .get(&s.step_name)
                .map(|fs| fs.continue_on_failure)
                .unwrap_or(false);
            FailedStepInfo {
                step_name: s.step_name.clone(),
                action_name: s.action_name.clone(),
                error_message: s.error_message.clone(),
                continue_on_failure,
            }
        })
        .collect();

    let error_message = if failed_steps.is_empty() {
        None
    } else {
        let parts: Vec<String> = failed_steps
            .iter()
            .map(|fs| {
                format!(
                    "Step '{}': {}",
                    fs.step_name,
                    fs.error_message.as_deref().unwrap_or("unknown error")
                )
            })
            .collect();
        Some(parts.join("; "))
    };

    let duration_secs = match (job.started_at, job.completed_at) {
        (Some(start), Some(end)) => Some((end - start).num_milliseconds() as f64 / 1000.0),
        _ => None,
    };

    Ok(HookContext {
        workspace: job.workspace.clone(),
        task_name: job.task_name.clone(),
        job_id: job.job_id.to_string(),
        status: job.status.clone(),
        is_success: job.status == "completed",
        error_message,
        source_type: job.source_type.clone(),
        source_id: job.source_id.clone(),
        started_at: job.started_at.map(|t| t.to_rfc3339()),
        completed_at: job.completed_at.map(|t| t.to_rfc3339()),
        duration_secs,
        failed_steps,
    })
}

async fn fire_single_hook(
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace: &str,
    hook: &HookDef,
    ctx_value: &serde_json::Value,
    source_id: &str,
) -> anyhow::Result<()> {
    // Resolve action
    let action = workspace_config
        .actions
        .get(&hook.action)
        .with_context(|| {
            format!(
                "Hook action '{}' not found in workspace '{}'",
                hook.action, workspace
            )
        })?;

    // Build Tera context: { "hook": <HookContext>, "input": <rendered_input> }
    let template_context = serde_json::json!({ "hook": ctx_value });

    // Render hook input through Tera templates
    let rendered_input = if hook.input.is_empty() {
        serde_json::json!({})
    } else {
        render_input_map(&hook.input, &template_context)
            .context("Failed to render hook input templates")?
    };

    // If the action is type: task, create a full job for the referenced task
    if action.action_type == "task" {
        let task_ref = action
            .task
            .as_ref()
            .context("type: task action missing task field")?;

        let job_id = crate::job_creator::create_job_for_task(
            pool,
            workspace_config,
            workspace,
            task_ref,
            rendered_input,
            "hook",
            Some(source_id),
        )
        .await
        .context("Failed to create hook task job")?;

        tracing::info!(
            "Fired hook task job {} for action '{}' -> task '{}' (source: {})",
            job_id,
            hook.action,
            task_ref,
            source_id
        );

        return Ok(());
    }

    // Create the hook job (single-step, always distributed)
    let task_name = format!("_hook:{}", hook.action);
    let job_id = JobRepo::create(
        pool,
        workspace,
        &task_name,
        "distributed",
        Some(rendered_input.clone()),
        "hook",
        Some(source_id),
    )
    .await
    .context("Failed to create hook job")?;

    // Create single step
    let action_spec = serde_json::to_value(action).ok();
    let required_tags = compute_required_tags(action);
    let runner = derive_runner(action);

    let step = NewJobStep {
        job_id,
        step_name: "hook".to_string(),
        action_name: hook.action.clone(),
        action_type: action.action_type.clone(),
        action_image: action.image.clone(),
        action_spec,
        input: Some(rendered_input),
        status: "ready".to_string(),
        required_tags,
        runner,
    };

    JobStepRepo::create_steps(pool, &[step])
        .await
        .context("Failed to create hook job step")?;

    tracing::info!(
        "Fired hook job {} for action '{}' (source: {})",
        job_id,
        hook.action,
        source_id
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_hook_context_serialization() {
        let ctx = HookContext {
            workspace: "default".to_string(),
            task_name: "deploy".to_string(),
            job_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            status: "failed".to_string(),
            is_success: false,
            error_message: Some("Step 'build': exit code 1".to_string()),
            source_type: "api".to_string(),
            source_id: None,
            started_at: Some("2025-01-01T00:00:00+00:00".to_string()),
            completed_at: Some("2025-01-01T00:01:30+00:00".to_string()),
            duration_secs: Some(90.0),
            failed_steps: vec![FailedStepInfo {
                step_name: "build".to_string(),
                action_name: "build-app".to_string(),
                error_message: Some("exit code 1".to_string()),
                continue_on_failure: false,
            }],
        };

        let value = serde_json::to_value(&ctx).unwrap();
        assert_eq!(value["workspace"], "default");
        assert_eq!(value["task_name"], "deploy");
        assert_eq!(value["is_success"], false);
        assert_eq!(value["failed_steps"][0]["step_name"], "build");
    }

    #[test]
    fn test_template_rendering_with_hook_context() {
        let ctx = HookContext {
            workspace: "prod".to_string(),
            task_name: "deploy".to_string(),
            job_id: "abc-123".to_string(),
            status: "completed".to_string(),
            is_success: true,
            error_message: None,
            source_type: "api".to_string(),
            source_id: None,
            started_at: None,
            completed_at: None,
            duration_secs: Some(42.5),
            failed_steps: vec![],
        };

        let ctx_value = serde_json::to_value(&ctx).unwrap();
        let template_context = json!({ "hook": ctx_value });

        let mut input = std::collections::HashMap::new();
        input.insert(
            "message".to_string(),
            json!("Job {{ hook.job_id }} in {{ hook.workspace }} {{ hook.status }}"),
        );

        let result = render_input_map(&input, &template_context).unwrap();
        assert_eq!(result["message"], "Job abc-123 in prod completed");
    }

    #[test]
    fn test_multiline_error_message_in_template() {
        let traceback = "Traceback (most recent call last):\n  File \"deploy.py\", line 42, in main\n    raise RuntimeError(\"connection refused\")\nRuntimeError: connection refused";

        let ctx = HookContext {
            workspace: "default".to_string(),
            task_name: "deploy".to_string(),
            job_id: "abc-123".to_string(),
            status: "failed".to_string(),
            is_success: false,
            error_message: Some(format!("Step 'run': {}", traceback)),
            source_type: "api".to_string(),
            source_id: None,
            started_at: None,
            completed_at: None,
            duration_secs: None,
            failed_steps: vec![FailedStepInfo {
                step_name: "run".to_string(),
                action_name: "deploy-app".to_string(),
                error_message: Some(traceback.to_string()),
                continue_on_failure: false,
            }],
        };

        let ctx_value = serde_json::to_value(&ctx).unwrap();
        let template_context = json!({ "hook": ctx_value });

        let mut input = std::collections::HashMap::new();
        input.insert("error".to_string(), json!("{{ hook.error_message }}"));

        let result = render_input_map(&input, &template_context).unwrap();
        let rendered = result["error"].as_str().unwrap();

        // Multiline error preserved through Tera rendering
        assert!(rendered.contains("Traceback (most recent call last):"));
        assert!(rendered.contains("RuntimeError: connection refused"));
        assert!(rendered.contains('\n'), "Newlines should be preserved");
    }
}
