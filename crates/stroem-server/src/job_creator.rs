use anyhow::{bail, Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_common::template::{
    merge_defaults, prepare_action_input, render_input_map, resolve_connection_inputs,
};
use stroem_common::validation::{compute_required_tags, derive_runner};
use stroem_db::{JobRepo, JobRow, JobStepRepo, NewJobStep};
use uuid::Uuid;

/// Maximum nesting depth for type: task sub-jobs (prevents infinite recursion)
const MAX_TASK_DEPTH: u32 = 10;

/// Create a job and its steps for a task in a workspace.
///
/// Shared by the API handler (`execute_task`) and the scheduler.
#[tracing::instrument(skip(pool, workspace_config, input))]
pub async fn create_job_for_task(
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    task_name: &str,
    input: serde_json::Value,
    source_type: &str,
    source_id: Option<&str>,
) -> Result<Uuid> {
    create_job_for_task_inner(
        pool,
        workspace_config,
        workspace_name,
        task_name,
        input,
        source_type,
        source_id,
        None,
        None,
    )
    .await
}

/// Create a job with parent tracking (for type: task sub-jobs).
#[allow(clippy::too_many_arguments)]
fn create_job_for_task_inner<'a>(
    pool: &'a PgPool,
    workspace_config: &'a WorkspaceConfig,
    workspace_name: &'a str,
    task_name: &'a str,
    input: serde_json::Value,
    source_type: &'a str,
    source_id: Option<&'a str>,
    parent_job_id: Option<Uuid>,
    parent_step_name: Option<&'a str>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Uuid>> + Send + 'a>> {
    Box::pin(async move {
        // Look up task
        let task = workspace_config.tasks.get(task_name).with_context(|| {
            format!(
                "Task '{}' not found in workspace '{}'",
                task_name, workspace_name
            )
        })?;

        // Merge input defaults from the task schema
        let secrets_ctx = serde_json::json!({ "secret": workspace_config.secrets });
        let merged_input = merge_defaults(&input, &task.input, &secrets_ctx)
            .context("Failed to merge input defaults")?;

        // Resolve connection inputs (replace connection names with full objects)
        let resolved_input =
            resolve_connection_inputs(&merged_input, &task.input, workspace_config)
                .context("Failed to resolve connection inputs")?;

        // Build job steps from the task flow
        let mut new_steps = Vec::new();
        // Generate job_id upfront so steps can reference it
        let job_id = Uuid::new_v4();

        for (step_name, flow_step) in &task.flow {
            let action = match workspace_config.actions.get(&flow_step.action) {
                Some(a) => a,
                None => {
                    bail!(
                        "Action '{}' not found in workspace '{}'",
                        flow_step.action,
                        workspace_name
                    );
                }
            };

            let status = if flow_step.depends_on.is_empty() && flow_step.when.is_none() {
                StepStatus::Ready
            } else {
                // Steps with `when` conditions start as pending even if they
                // have no deps — the post-creation promote loop evaluates them.
                StepStatus::Pending
            };

            let action_spec = serde_json::to_value(action).ok();
            let required_tags = compute_required_tags(action);
            let runner = derive_runner(action);

            new_steps.push(NewJobStep {
                job_id,
                step_name: step_name.clone(),
                action_name: flow_step.action.clone(),
                action_type: action.action_type.clone(),
                action_image: action.image.clone(),
                action_spec,
                input: Some(serde_json::to_value(&flow_step.input).unwrap_or_default()),
                status: status.to_string(), // NewJobStep.status is String for DB compatibility
                required_tags,
                runner,
                timeout_secs: flow_step
                    .timeout
                    .map(|d| i32::try_from(d.as_secs()).expect("timeout validated to fit i32")),
                when_condition: flow_step.when.clone(),
            });
        }

        // Create job and steps atomically in a transaction
        let mut tx = pool.begin().await.context("Failed to begin transaction")?;

        JobRepo::create_with_parent_tx_id(
            &mut *tx,
            job_id,
            workspace_name,
            task_name,
            &task.mode,
            Some(resolved_input),
            source_type,
            source_id,
            parent_job_id,
            parent_step_name,
            task.timeout
                .map(|d| i32::try_from(d.as_secs()).expect("timeout validated to fit i32")),
        )
        .await
        .context("Failed to create job")?;

        JobStepRepo::create_steps_tx(&mut *tx, &new_steps)
            .await
            .context("Failed to create job steps")?;

        tx.commit().await.context("Failed to commit job creation")?;

        tracing::info!("Created job {} with {} steps", job_id, new_steps.len());

        // Evaluate root steps with `when` conditions (no deps, but conditional)
        let has_root_conditions = task
            .flow
            .values()
            .any(|fs| fs.depends_on.is_empty() && fs.when.is_some());
        if has_root_conditions {
            // Fetch job_row once — it doesn't change, but the step snapshot
            // must be refreshed each iteration as steps are promoted/skipped.
            let job_row = JobRepo::get(pool, job_id).await?.context("Job not found")?;

            // Safety bound: at most (flow length + 1) iterations.
            let max_iterations = task.flow.len() + 1;
            for _iteration in 0..max_iterations {
                let steps_snapshot = JobStepRepo::get_steps_for_job(pool, job_id).await?;
                let render_ctx =
                    build_step_render_context(&job_row, &steps_snapshot, workspace_config);

                // Promote/skip loop: root conditions may cascade
                let changed =
                    JobStepRepo::promote_ready_steps(pool, job_id, &task.flow, Some(&render_ctx))
                        .await?;
                let skipped = JobStepRepo::skip_unreachable_steps(pool, job_id, &task.flow).await?;
                if changed.is_empty() && skipped.is_empty() {
                    break;
                }

                if _iteration + 1 == max_iterations {
                    tracing::warn!(
                        job_id = %job_id,
                        "Root-condition cascade loop reached iteration limit ({}) — breaking to avoid infinite loop",
                        max_iterations
                    );
                }
            }
        }

        // Handle any initially-ready type: task steps
        handle_task_steps(pool, workspace_config, workspace_name, job_id).await?;

        // If all steps ended up terminal (e.g. all skipped by when conditions),
        // mark the job as completed now rather than waiting for the recovery sweep.
        if has_root_conditions {
            let all_terminal = JobStepRepo::all_steps_terminal(pool, job_id).await?;
            if all_terminal {
                JobRepo::mark_completed(pool, job_id, None).await?;
                tracing::info!(job_id = %job_id, "All steps terminal at creation — job marked completed");
            }
        }

        Ok(job_id)
    })
}

/// Create sub-jobs for any "ready" type:task steps in a job.
///
/// Called after job creation and after orchestrator promotes steps.
/// This is the server-side dispatch for task-action steps — workers never claim them.
#[tracing::instrument(skip(pool, workspace_config))]
pub async fn handle_task_steps(
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
) -> Result<()> {
    let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
    let job = JobRepo::get(pool, job_id).await?.context("Job not found")?;

    for step in &steps {
        if step.status != StepStatus::Ready.as_ref() || step.action_type != "task" {
            continue;
        }

        // Get the referenced task name from action_spec
        let action_spec = step
            .action_spec
            .as_ref()
            .context("Missing action_spec for task step")?;
        let task_ref = action_spec["task"]
            .as_str()
            .context("Missing task field in action_spec")?;

        // Check recursion depth
        let depth = compute_depth(pool, &job).await?;
        if depth >= MAX_TASK_DEPTH {
            let err = format!(
                "Maximum task nesting depth ({}) exceeded for task '{}'",
                MAX_TASK_DEPTH, task_ref
            );
            JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
            tracing::error!("{}", err);
            continue;
        }

        // Build render context (same as claim_job, with secrets)
        let context_value = build_step_render_context(&job, &steps, workspace_config);

        // Render step input templates
        let rendered_input = if let Some(ref input) = step.input {
            if let Some(input_map) = input.as_object() {
                if input_map.is_empty() {
                    serde_json::json!({})
                } else {
                    let map: HashMap<String, serde_json::Value> = input_map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    render_input_map(&map, &context_value).with_context(|| {
                        format!("Failed to render input for task step '{}'", step.step_name)
                    })?
                }
            } else {
                serde_json::json!({})
            }
        } else {
            serde_json::json!({})
        };

        // Merge action-level input defaults and resolve connection inputs
        let rendered_input = if let Some(action) = workspace_config.actions.get(&step.action_name) {
            if !action.input.is_empty() {
                match prepare_action_input(&rendered_input, &action.input, workspace_config) {
                    Ok(prepared) => prepared,
                    Err(e) => {
                        tracing::warn!("Failed to prepare action input: {:#}", e);
                        rendered_input
                    }
                }
            } else {
                rendered_input
            }
        } else {
            rendered_input
        };

        // Mark step as running (server-side, so we don't process it again)
        JobStepRepo::mark_running_server(pool, job_id, &step.step_name).await?;

        // Transition parent job to running if still pending
        JobRepo::mark_running_if_pending_server(pool, job_id).await?;

        let source_id = format!("{}/{}", job_id, step.step_name);

        // Create child job with parent tracking
        match create_job_for_task_inner(
            pool,
            workspace_config,
            workspace_name,
            task_ref,
            rendered_input,
            "task",
            Some(&source_id),
            Some(job_id),
            Some(&step.step_name),
        )
        .await
        {
            Ok(child_job_id) => {
                tracing::info!(
                    "Created child job {} for task step '{}' -> task '{}'",
                    child_job_id,
                    step.step_name,
                    task_ref
                );
            }
            Err(e) => {
                let err = format!(
                    "Failed to create child job for task '{}': {:#}",
                    task_ref, e
                );
                JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
                tracing::error!("{}", err);
            }
        }
    }

    Ok(())
}

/// Build a template render context from a job and its steps.
/// Same logic as claim_job but without DB access (steps already loaded).
/// Includes workspace secrets under the `secret` key.
pub fn build_step_render_context(
    job: &JobRow,
    steps: &[stroem_db::JobStepRow],
    workspace_config: &WorkspaceConfig,
) -> serde_json::Value {
    let mut ctx = serde_json::Map::new();
    if let Some(ref input) = job.input {
        ctx.insert("input".to_string(), input.clone());
    }
    for s in steps {
        if s.status == StepStatus::Completed.as_ref() {
            let mut step_ctx = serde_json::Map::new();
            if let Some(ref output) = s.output {
                step_ctx.insert("output".to_string(), output.clone());
            }
            let safe_name = s.step_name.replace('-', "_");
            ctx.insert(safe_name, serde_json::Value::Object(step_ctx));
        } else if s.status == StepStatus::Skipped.as_ref() {
            // Include skipped steps with null output so downstream `when`
            // expressions can reference them without Tera undefined errors.
            let mut step_ctx = serde_json::Map::new();
            step_ctx.insert("output".to_string(), serde_json::Value::Null);
            let safe_name = s.step_name.replace('-', "_");
            ctx.insert(safe_name, serde_json::Value::Object(step_ctx));
        } else if s.status == StepStatus::Failed.as_ref() {
            // Include failed steps with null output and their error message so
            // downstream `when` expressions can inspect them.
            let mut step_ctx = serde_json::Map::new();
            step_ctx.insert("output".to_string(), serde_json::Value::Null);
            if let Some(ref err) = s.error_message {
                step_ctx.insert("error".to_string(), serde_json::Value::String(err.clone()));
            }
            let safe_name = s.step_name.replace('-', "_");
            ctx.insert(safe_name, serde_json::Value::Object(step_ctx));
        }
    }
    if !workspace_config.secrets.is_empty() {
        if let Ok(secrets_value) = serde_json::to_value(&workspace_config.secrets) {
            ctx.insert("secret".to_string(), secrets_value);
        }
    }
    serde_json::Value::Object(ctx)
}

/// Compute the nesting depth of a job by walking the parent chain.
async fn compute_depth(pool: &PgPool, job: &JobRow) -> Result<u32> {
    let mut depth = 0u32;
    let mut current_parent = job.parent_job_id;
    while let Some(parent_id) = current_parent {
        depth += 1;
        if depth >= MAX_TASK_DEPTH {
            break;
        }
        let parent = JobRepo::get(pool, parent_id).await?;
        current_parent = parent.and_then(|p| p.parent_job_id);
    }
    Ok(depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn make_job(input: Option<serde_json::Value>) -> JobRow {
        JobRow {
            job_id: Uuid::new_v4(),
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            mode: "distributed".to_string(),
            input,
            output: None,
            status: "running".to_string(),
            source_type: "api".to_string(),
            source_id: None,
            worker_id: None,
            revision: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            log_path: None,
            parent_job_id: None,
            parent_step_name: None,
            timeout_secs: None,
        }
    }

    fn make_step(
        job_id: Uuid,
        name: &str,
        status: &str,
        output: Option<serde_json::Value>,
    ) -> stroem_db::JobStepRow {
        stroem_db::JobStepRow {
            job_id,
            step_name: name.to_string(),
            action_name: name.to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: None,
            input: None,
            output,
            status: status.to_string(),
            worker_id: None,
            started_at: None,
            completed_at: None,
            error_message: None,
            required_tags: json!([]),
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        }
    }

    #[test]
    fn test_build_step_render_context_with_secrets() {
        let job = make_job(Some(json!({"env": "prod"})));
        let steps = vec![make_step(
            job.job_id,
            "build",
            "completed",
            Some(json!({"tag": "v1.0"})),
        )];

        let mut ws = WorkspaceConfig::new();
        ws.secrets
            .insert("API_KEY".to_string(), json!("secret-value-123"));
        ws.secrets.insert(
            "WEBHOOK_URL".to_string(),
            json!("https://hooks.example.com/x"),
        );

        let ctx = build_step_render_context(&job, &steps, &ws);

        // Input is present
        assert_eq!(ctx["input"]["env"], "prod");
        // Completed step output is present
        assert_eq!(ctx["build"]["output"]["tag"], "v1.0");
        // Secrets are present
        assert_eq!(ctx["secret"]["API_KEY"], "secret-value-123");
        assert_eq!(ctx["secret"]["WEBHOOK_URL"], "https://hooks.example.com/x");
    }

    #[test]
    fn test_build_step_render_context_no_secrets() {
        let job = make_job(Some(json!({"env": "staging"})));
        let steps = vec![];
        let ws = WorkspaceConfig::new();

        let ctx = build_step_render_context(&job, &steps, &ws);

        assert_eq!(ctx["input"]["env"], "staging");
        // No secret key when secrets are empty
        assert!(ctx.get("secret").is_none());
    }

    #[test]
    fn test_build_step_render_context_hyphen_sanitization() {
        let job = make_job(None);
        let steps = vec![make_step(
            job.job_id,
            "build-app",
            "completed",
            Some(json!({"image": "app:latest"})),
        )];
        let ws = WorkspaceConfig::new();

        let ctx = build_step_render_context(&job, &steps, &ws);

        // Hyphens in step names become underscores
        assert_eq!(ctx["build_app"]["output"]["image"], "app:latest");
        assert!(ctx.get("build-app").is_none());
    }

    #[test]
    fn test_build_step_render_context_skipped_step_has_null_output() {
        let job = make_job(None);
        let steps = vec![
            make_step(job.job_id, "build", "completed", Some(json!({"tag": "v1"}))),
            make_step(job.job_id, "deploy", "skipped", None),
        ];
        let ws = WorkspaceConfig::new();

        let ctx = build_step_render_context(&job, &steps, &ws);

        assert_eq!(ctx["build"]["output"]["tag"], "v1");
        // Skipped step should be present with null output
        assert!(ctx.get("deploy").is_some());
        assert!(ctx["deploy"]["output"].is_null());
    }

    #[test]
    fn test_build_step_render_context_failed_step_has_null_output_and_error() {
        let job = make_job(None);
        let mut failed_step = make_step(job.job_id, "risky", "failed", None);
        failed_step.error_message = Some("command failed".to_string());
        let steps = vec![failed_step];
        let ws = WorkspaceConfig::new();

        let ctx = build_step_render_context(&job, &steps, &ws);

        assert!(ctx.get("risky").is_some());
        assert!(ctx["risky"]["output"].is_null());
        assert_eq!(ctx["risky"]["error"], "command failed");
    }

    #[test]
    fn test_build_step_render_context_only_completed_steps() {
        let job = make_job(None);
        let steps = vec![
            make_step(
                job.job_id,
                "step1",
                "completed",
                Some(json!({"result": "ok"})),
            ),
            make_step(
                job.job_id,
                "step2",
                "running",
                Some(json!({"partial": true})),
            ),
            make_step(job.job_id, "step3", "pending", None),
        ];
        let ws = WorkspaceConfig::new();

        let ctx = build_step_render_context(&job, &steps, &ws);

        assert_eq!(ctx["step1"]["output"]["result"], "ok");
        assert!(ctx.get("step2").is_none());
        assert!(ctx.get("step3").is_none());
    }
}
