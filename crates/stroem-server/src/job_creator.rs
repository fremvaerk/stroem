use anyhow::{bail, Context, Result};
use sqlx::PgPool;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_common::validation::{compute_required_tags, derive_runner};
use stroem_db::{JobRepo, JobStepRepo, NewJobStep};
use uuid::Uuid;

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
    // Look up task
    let task = workspace_config.tasks.get(task_name).with_context(|| {
        format!(
            "Task '{}' not found in workspace '{}'",
            task_name, workspace_name
        )
    })?;

    // Create job record
    let job_id = JobRepo::create(
        pool,
        workspace_name,
        task_name,
        &task.mode,
        Some(input),
        source_type,
        source_id,
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

    Ok(job_id)
}
