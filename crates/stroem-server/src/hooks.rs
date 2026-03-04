use crate::state::AppState;
use anyhow::Context;
use serde::Serialize;
use sqlx::PgPool;
use stroem_common::models::job::{JobStatus, SourceType, StepStatus};
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
/// - Task-level hooks take priority; workspace-level hooks fire as fallback for top-level jobs only.
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
    if job.source_type == SourceType::Hook.as_ref() {
        return;
    }

    // Select task-level and workspace-level hooks for this event type
    // Cancelled jobs fire on_error hooks (treated as failure)
    let (task_hooks, ws_hooks) = match job.status.parse::<JobStatus>().ok() {
        Some(JobStatus::Completed) => (&task.on_success, &workspace_config.on_success),
        Some(JobStatus::Failed) | Some(JobStatus::Cancelled) => {
            (&task.on_error, &workspace_config.on_error)
        }
        _ => return,
    };

    // Task hooks take priority. Workspace hooks are fallback for top-level jobs only.
    let is_top_level = matches!(
        job.source_type.as_str(),
        "api" | "user" | "trigger" | "webhook"
    );
    let hooks: &[HookDef] = if !task_hooks.is_empty() {
        task_hooks
    } else if is_top_level {
        ws_hooks
    } else {
        return;
    };

    if hooks.is_empty() {
        return;
    }

    // Build hook context
    let ctx = match build_hook_context(&state.pool, job, task).await {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!(
                "Failed to build hook context for job {}: {:#}",
                job.job_id,
                e
            );
            state
                .append_server_log(
                    job.job_id,
                    &format!("[hooks] Failed to build hook context: {:#}", e),
                )
                .await;
            return;
        }
    };

    let ctx_value = match serde_json::to_value(&ctx) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to serialize hook context: {:#}", e);
            state
                .append_server_log(
                    job.job_id,
                    &format!("[hooks] Failed to serialize hook context: {:#}", e),
                )
                .await;
            return;
        }
    };

    let hook_type = if job.status == JobStatus::Completed.as_ref() {
        "on_success"
    } else {
        "on_error" // covers both "failed" and "cancelled"
    };

    for (i, hook) in hooks.iter().enumerate() {
        let source_id = job.job_id.to_string();

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
                "Failed to fire hook {}[{}] for job {}: {:#}",
                hook_type,
                i,
                job.job_id,
                e
            );
            state
                .append_server_log(
                    job.job_id,
                    &format!(
                        "[hooks] Failed to fire hook {}[{}] for action '{}': {:#}",
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
        .filter(|s| s.status == StepStatus::Failed.as_ref())
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
        is_success: job.status == JobStatus::Completed.as_ref(),
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

    // Build Tera context: { "hook": <HookContext>, "secret": <workspace secrets> }
    let mut template_context = serde_json::json!({ "hook": ctx_value });
    if !workspace_config.secrets.is_empty() {
        if let Ok(secrets_value) = serde_json::to_value(&workspace_config.secrets) {
            template_context["secret"] = secrets_value;
        }
    }

    // Render hook input through Tera templates
    let rendered_input = if hook.input.is_empty() {
        serde_json::json!({})
    } else {
        render_input_map(&hook.input, &template_context)
            .context("Failed to render hook input templates")?
    };

    // If the action is type: task, create a full job for the referenced task
    if action.action_type == "task" {
        // action_type is a String field on ActionDef
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
        status: StepStatus::Ready.to_string(),
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

/// Select which hooks to fire for a job, applying the priority and fallback rules.
///
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn select_hooks_for_job<'a>(
        workspace_config: &'a WorkspaceConfig,
        job_source_type: &str,
        job_status: &str,
        task: &'a TaskDef,
    ) -> Option<&'a [HookDef]> {
        if job_source_type == "hook" {
            return None;
        }

        let (task_hooks, ws_hooks) = match job_status {
            "completed" => (&task.on_success, &workspace_config.on_success),
            "failed" | "cancelled" => (&task.on_error, &workspace_config.on_error),
            _ => return None,
        };

        let is_top_level = matches!(job_source_type, "api" | "user" | "trigger" | "webhook");
        let hooks: &[HookDef] = if !task_hooks.is_empty() {
            task_hooks
        } else if is_top_level {
            ws_hooks
        } else {
            return None;
        };

        if hooks.is_empty() {
            None
        } else {
            Some(hooks)
        }
    }

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

    #[test]
    fn test_hook_template_with_secrets() {
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
            duration_secs: None,
            failed_steps: vec![],
        };

        let ctx_value = serde_json::to_value(&ctx).unwrap();

        // Simulate what fire_single_hook builds: { "hook": ..., "secret": ... }
        let mut template_context = json!({ "hook": ctx_value });
        let secrets: HashMap<String, serde_json::Value> = [
            (
                "WEBHOOK_URL".to_string(),
                json!("https://chat.example.com/webhook"),
            ),
            ("API_KEY".to_string(), json!("ref+vault://secret/key")),
        ]
        .into();
        template_context["secret"] = serde_json::to_value(&secrets).unwrap();

        let mut input = std::collections::HashMap::new();
        input.insert("webhook_url".to_string(), json!("{{ secret.WEBHOOK_URL }}"));
        input.insert("api_key".to_string(), json!("{{ secret.API_KEY }}"));
        input.insert(
            "message".to_string(),
            json!("Job {{ hook.job_id }} {{ hook.status }}"),
        );

        let result = render_input_map(&input, &template_context).unwrap();
        assert_eq!(result["webhook_url"], "https://chat.example.com/webhook");
        assert_eq!(result["api_key"], "ref+vault://secret/key");
        assert_eq!(result["message"], "Job abc-123 completed");
    }

    #[test]
    fn test_hook_template_with_nested_secrets() {
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
            duration_secs: None,
            failed_steps: vec![],
        };

        let ctx_value = serde_json::to_value(&ctx).unwrap();
        let mut template_context = json!({ "hook": ctx_value });
        template_context["secret"] = json!({
            "notifications": {
                "google_chat": "https://chat.googleapis.com/webhook/123"
            }
        });

        let mut input = std::collections::HashMap::new();
        input.insert(
            "url".to_string(),
            json!("{{ secret.notifications.google_chat }}"),
        );

        let result = render_input_map(&input, &template_context).unwrap();
        assert_eq!(result["url"], "https://chat.googleapis.com/webhook/123");
    }

    // ─── Workspace-level hook fallback selection tests ───────────────────────

    fn make_hook(action: &str) -> HookDef {
        HookDef {
            action: action.to_string(),
            input: HashMap::new(),
        }
    }

    fn make_task_def(on_success: Vec<HookDef>, on_error: Vec<HookDef>) -> TaskDef {
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            on_success,
            on_error,
        }
    }

    fn make_workspace_config(on_success: Vec<HookDef>, on_error: Vec<HookDef>) -> WorkspaceConfig {
        let mut config = WorkspaceConfig::new();
        config.on_success = on_success;
        config.on_error = on_error;
        config
    }

    /// Task has its own on_success hooks — workspace on_success hooks must NOT be used.
    #[test]
    fn test_task_on_success_takes_priority_over_workspace() {
        let task = make_task_def(vec![make_hook("task-notify")], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "completed", &task).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "task-notify");
    }

    /// Task has no on_success hooks — workspace on_success hooks are used as fallback
    /// for a top-level job (source_type = "api").
    #[test]
    fn test_workspace_on_success_fallback_when_task_has_none() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "completed", &task).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "ws-notify");
    }

    /// Task has its own on_error hooks — workspace on_error hooks must NOT be used.
    #[test]
    fn test_task_on_error_takes_priority_over_workspace() {
        let task = make_task_def(vec![], vec![make_hook("task-alert")]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")]);

        let selected = select_hooks_for_job(&ws, "api", "failed", &task).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "task-alert");
    }

    /// Task has no on_error hooks — workspace on_error hooks are used as fallback
    /// for a top-level job (source_type = "api").
    #[test]
    fn test_workspace_on_error_fallback_when_task_has_none() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")]);

        let selected = select_hooks_for_job(&ws, "api", "failed", &task).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "ws-alert");
    }

    /// Task has on_success but no on_error — workspace fallback applies only for on_error.
    #[test]
    fn test_workspace_fallback_only_for_missing_hook_type() {
        let task = make_task_def(vec![make_hook("task-notify")], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![make_hook("ws-alert")]);

        // on_success: task hooks win
        let success_hooks = select_hooks_for_job(&ws, "api", "completed", &task).unwrap();
        assert_eq!(success_hooks.len(), 1);
        assert_eq!(success_hooks[0].action, "task-notify");

        // on_error: task has none, so workspace fallback fires
        let error_hooks = select_hooks_for_job(&ws, "api", "failed", &task).unwrap();
        assert_eq!(error_hooks.len(), 1);
        assert_eq!(error_hooks[0].action, "ws-alert");
    }

    /// Workspace fallback does NOT fire for child jobs (source_type = "task").
    #[test]
    fn test_workspace_fallback_not_used_for_child_jobs() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![]);

        // Child jobs (source_type = "task") must not use workspace fallback
        let selected = select_hooks_for_job(&ws, "task", "completed", &task);
        assert!(
            selected.is_none(),
            "workspace fallback must not fire for child jobs"
        );
    }

    /// Workspace fallback does NOT fire for hook jobs (recursion guard).
    #[test]
    fn test_recursion_guard_prevents_hook_jobs_from_firing_hooks() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![]);

        let selected = select_hooks_for_job(&ws, "hook", "completed", &task);
        assert!(
            selected.is_none(),
            "hook jobs must never trigger further hooks"
        );
    }

    /// Workspace fallback fires for trigger-sourced top-level jobs.
    #[test]
    fn test_workspace_fallback_fires_for_trigger_sourced_jobs() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![]);

        let selected = select_hooks_for_job(&ws, "trigger", "completed", &task).unwrap();
        assert_eq!(selected[0].action, "ws-notify");
    }

    /// Workspace fallback fires for user-sourced (authenticated API) top-level jobs.
    #[test]
    fn test_workspace_fallback_fires_for_user_sourced_jobs() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")]);

        let selected = select_hooks_for_job(&ws, "user", "failed", &task).unwrap();
        assert_eq!(selected[0].action, "ws-alert");
    }

    /// Workspace fallback fires for webhook-sourced top-level jobs.
    #[test]
    fn test_workspace_fallback_fires_for_webhook_sourced_jobs() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")]);

        let selected = select_hooks_for_job(&ws, "webhook", "failed", &task).unwrap();
        assert_eq!(selected[0].action, "ws-alert");
    }

    /// No hooks at all — returns None rather than an empty slice.
    #[test]
    fn test_no_hooks_anywhere_returns_none() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![]);

        assert!(select_hooks_for_job(&ws, "api", "completed", &task).is_none());
        assert!(select_hooks_for_job(&ws, "api", "failed", &task).is_none());
    }

    /// Non-terminal statuses (e.g. "pending", "running") never produce hooks.
    #[test]
    fn test_non_terminal_status_returns_none() {
        let task = make_task_def(
            vec![make_hook("task-notify")],
            vec![make_hook("task-alert")],
        );
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![make_hook("ws-alert")]);

        for status in &["pending", "running", "unknown"] {
            assert!(
                select_hooks_for_job(&ws, "api", status, &task).is_none(),
                "status '{status}' should not trigger hooks"
            );
        }
    }

    /// Cancelled jobs fire on_error hooks (treated as failure).
    #[test]
    fn test_cancelled_fires_on_error_hooks() {
        let task = make_task_def(vec![], vec![make_hook("task-alert")]);
        let ws = make_workspace_config(vec![], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "cancelled", &task).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "task-alert");
    }

    /// Multiple workspace fallback hooks are all returned.
    #[test]
    fn test_workspace_fallback_returns_all_hooks_when_multiple() {
        let task = make_task_def(vec![], vec![]);
        let ws = make_workspace_config(
            vec![make_hook("ws-slack"), make_hook("ws-pagerduty")],
            vec![],
        );

        let selected = select_hooks_for_job(&ws, "api", "completed", &task).unwrap();
        assert_eq!(selected.len(), 2);
        let actions: Vec<&str> = selected.iter().map(|h| h.action.as_str()).collect();
        assert!(actions.contains(&"ws-slack"));
        assert!(actions.contains(&"ws-pagerduty"));
    }
}
