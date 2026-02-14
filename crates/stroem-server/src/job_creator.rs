use anyhow::{bail, Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_common::template::render_input_map;
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

        // Create job record
        let job_id = JobRepo::create_with_parent(
            pool,
            workspace_name,
            task_name,
            &task.mode,
            Some(input),
            source_type,
            source_id,
            parent_job_id,
            parent_step_name,
        )
        .await
        .context("Failed to create job")?;

        // Build job steps from the task flow
        let mut new_steps = Vec::new();

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

            let status = if flow_step.depends_on.is_empty() {
                "ready"
            } else {
                "pending"
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
                status: status.to_string(),
                required_tags,
                runner,
            });
        }

        JobStepRepo::create_steps(pool, &new_steps)
            .await
            .context("Failed to create job steps")?;

        tracing::info!("Created job {} with {} steps", job_id, new_steps.len());

        // Handle any initially-ready type: task steps
        handle_task_steps(pool, workspace_config, workspace_name, job_id).await?;

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
        if step.status != "ready" || step.action_type != "task" {
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

        // Build render context (same as claim_job)
        let context_value = build_step_render_context(&job, &steps);

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
                let err = format!("Failed to create child job for task '{}': {}", task_ref, e);
                JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
                tracing::error!("{}", err);
            }
        }
    }

    Ok(())
}

/// Build a template render context from a job and its steps.
/// Same logic as claim_job but without DB access (steps already loaded).
fn build_step_render_context(job: &JobRow, steps: &[stroem_db::JobStepRow]) -> serde_json::Value {
    let mut ctx = serde_json::Map::new();
    if let Some(ref input) = job.input {
        ctx.insert("input".to_string(), input.clone());
    }
    for s in steps {
        if s.status == "completed" {
            let mut step_ctx = serde_json::Map::new();
            if let Some(ref output) = s.output {
                step_ctx.insert("output".to_string(), output.clone());
            }
            let safe_name = s.step_name.replace('-', "_");
            ctx.insert(safe_name, serde_json::Value::Object(step_ctx));
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
