//! Server-side step dispatch: `type: task` and `type: approval` steps are
//! never claimed by workers, so their execution lives here instead of the
//! worker claim path. Also owns the creation-time post-commit initialisation
//! (`init`) that a freshly committed job needs before it can be handed back.

use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::job::{JobStatus, StepStatus};
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_common::template::{prepare_action_input, render_input_map};
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

use crate::config::JobDefaults;
use crate::job_creator::{
    build_step_render_context, compute_depth, create_job_for_task_inner, CreationMode,
    MAX_TASK_DEPTH,
};
use crate::workspace::WorkspaceManager;
use crate::workspace_set::WorkspaceSet;

/// Create sub-jobs for any "ready" type:task steps in a job.
///
/// Called after job creation and after orchestrator promotes steps.
/// This is the server-side dispatch for task-action steps — workers never claim them.
#[tracing::instrument(skip(workspaces, pool, workspace_config))]
pub async fn handle_task_steps(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &stroem_common::models::workflow::TaskDef,
    defaults: JobDefaults,
) -> Result<()> {
    // A dispatch failure re-runs the orchestrator, which may promote further
    // `type: task` steps (e.g. `continue_on_failure` dependents) that this
    // pass's snapshot never saw — so loop until a pass fails nothing. Bounded
    // by the flow size: every failing pass retires at least one step.
    for _ in 0..(task.flow.len() + 1) {
        let failed_any = handle_task_steps_pass(
            workspaces,
            pool,
            workspace_config,
            workspace_name,
            job_id,
            task,
            defaults,
        )
        .await?;
        if !failed_any {
            break;
        }
    }
    Ok(())
}

/// Mark a server-dispatched step failed and immediately re-orchestrate so its
/// dependents are cascade-skipped / the job closes. Every failure branch in
/// `handle_task_steps_pass` must go through here.
async fn fail_task_step(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    err: &str,
    task: &stroem_common::models::workflow::TaskDef,
    workspace_config: &WorkspaceConfig,
) -> Result<()> {
    tracing::error!("{}", err);
    JobStepRepo::mark_failed(pool, job_id, step_name, err).await?;
    orchestrate_after_server_step_failure(pool, job_id, step_name, task, workspace_config).await;
    Ok(())
}

/// One dispatch pass over the currently-ready `type: task` steps.
/// Returns `true` if any step was marked failed during this pass.
async fn handle_task_steps_pass(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &stroem_common::models::workflow::TaskDef,
    defaults: JobDefaults,
) -> Result<bool> {
    let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
    let job = JobRepo::get(pool, job_id).await?.context("Job not found")?;
    let mut failed_any = false;

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
            fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config).await?;
            failed_any = true;
            continue;
        }

        // Build render context (same as claim_job, with secrets)
        let mut context_value = build_step_render_context(&job, &steps, workspace_config);

        // For loop instances, inject `each` variable into render context
        if let (Some(ref loop_item), Some(loop_index)) = (&step.loop_item, step.loop_index) {
            if let Some(ctx_obj) = context_value.as_object_mut() {
                ctx_obj.insert(
                    "each".to_string(),
                    serde_json::json!({
                        "item": loop_item,
                        "index": loop_index,
                        "total": step.loop_total,
                    }),
                );
            }
        }

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
                    match render_input_map(&map, &context_value) {
                        Ok(rendered) => rendered,
                        Err(e) => {
                            let err = format!(
                                "Failed to render input for task step '{}': {:#}",
                                step.step_name, e
                            );
                            fail_task_step(
                                pool,
                                job_id,
                                &step.step_name,
                                &err,
                                task,
                                workspace_config,
                            )
                            .await?;
                            failed_any = true;
                            continue;
                        }
                    }
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
                let ws_set =
                    WorkspaceSet::load(workspaces, workspace_name, Some(workspace_config)).await;
                match prepare_action_input(&rendered_input, &action.input, &ws_set) {
                    Ok(prepared) => prepared,
                    Err(e) => {
                        let err = format!(
                            "Failed to prepare action input for task step '{}': {:#}",
                            step.step_name, e
                        );
                        fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config)
                            .await?;
                        failed_any = true;
                        continue;
                    }
                }
            } else {
                rendered_input
            }
        } else {
            rendered_input
        };

        // Persist rendered input to DB so the job detail API shows resolved values.
        if let Err(e) =
            JobStepRepo::update_input(pool, job_id, &step.step_name, Some(rendered_input.clone()))
                .await
        {
            tracing::warn!("Failed to persist rendered input: {:#}", e);
        }

        // Mark step as running (server-side, so we don't process it again)
        JobStepRepo::mark_running_server(pool, job_id, &step.step_name).await?;

        // Transition parent job to running if still pending
        JobRepo::mark_running_if_pending_server(pool, job_id).await?;

        let source_id = format!("{}/{}", job_id, step.step_name);

        // Create child job with parent tracking (inherits parent revision).
        // agents_config is not available here (handle_task_steps only has pool),
        // so agent steps in child jobs will be dispatched by the orchestrator
        // when it processes the child job's ready steps. `defaults` is threaded
        // through so sub-jobs inherit the server-level timeout defaults.
        match create_job_for_task_inner(
            workspaces,
            pool,
            workspace_config,
            workspace_name,
            task_ref,
            rendered_input,
            "task",
            Some(&source_id),
            Some(job_id),
            Some(&step.step_name),
            job.revision.as_deref(),
            CreationMode::Normal, // child task jobs never inherit re-run/restart lineage
            None,                 // agents_config not available; orchestrator will dispatch
            defaults,
        )
        .await
        {
            Ok(created) => {
                let child_job_id = created.job_id;
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
                fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config).await?;
                failed_any = true;
            }
        }
    }

    Ok(failed_any)
}

/// Suspend any "ready" approval steps in a job.
///
/// Called after job creation and after the orchestrator promotes steps.
/// Renders the approval message through Tera, stores it as step output,
/// and transitions the step from `ready` to `suspended`.
#[tracing::instrument(skip(pool, workspace_config))]
pub async fn handle_approval_steps(
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &stroem_common::models::workflow::TaskDef,
) -> Result<()> {
    let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
    let job = JobRepo::get(pool, job_id).await?.context("Job not found")?;

    for step in &steps {
        if step.status != StepStatus::Ready.as_ref() || step.action_type != "approval" {
            continue;
        }

        // Get the message template from action_spec
        let raw_message = step
            .action_spec
            .as_ref()
            .and_then(|spec| spec["message"].as_str())
            .unwrap_or("")
            .to_string();

        // Build render context (same pattern as handle_task_steps)
        let mut context_value = build_step_render_context(&job, &steps, workspace_config);

        // For loop instances, inject `each` variable into render context
        if let (Some(ref loop_item), Some(loop_index)) = (&step.loop_item, step.loop_index) {
            if let Some(ctx_obj) = context_value.as_object_mut() {
                ctx_obj.insert(
                    "each".to_string(),
                    serde_json::json!({
                        "item": loop_item,
                        "index": loop_index,
                        "total": step.loop_total,
                    }),
                );
            }
        }

        // Render the step's flow-level input (resolves templates like
        // {{ prepare.output.changelog }}) and replace `input` in the context
        // so the message template can use {{ input.changelog }} to reference
        // resolved step input, not just job-level input.
        if let Some(ref input) = step.input {
            if let Some(input_map) = input.as_object() {
                if !input_map.is_empty() {
                    let map: HashMap<String, serde_json::Value> = input_map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    match render_input_map(&map, &context_value) {
                        Ok(resolved) => {
                            // Persist rendered input to DB for the job detail API.
                            if let Err(e) = JobStepRepo::update_input(
                                pool,
                                job_id,
                                &step.step_name,
                                Some(resolved.clone()),
                            )
                            .await
                            {
                                tracing::warn!("Failed to persist rendered input: {:#}", e);
                            }
                            if let Some(ctx_obj) = context_value.as_object_mut() {
                                ctx_obj.insert("input".to_string(), resolved);
                            }
                        }
                        Err(e) => {
                            let err = format!(
                                "Failed to render input for approval step '{}': {:#}",
                                step.step_name, e
                            );
                            fail_task_step(
                                pool,
                                job_id,
                                &step.step_name,
                                &err,
                                task,
                                workspace_config,
                            )
                            .await?;
                            continue;
                        }
                    }
                }
            }
        }

        // Warn when no message template is configured — action_spec may be missing
        // or the action was defined without a `message` field. (FIX 8)
        if raw_message.is_empty() {
            tracing::warn!(
                job_id = %job_id,
                step = %step.step_name,
                "Approval step has no message template — action_spec may be missing or malformed"
            );
        }

        // Render the message template through Tera
        let rendered_message = if raw_message.is_empty() {
            String::new()
        } else {
            match stroem_common::template::render_template(&raw_message, &context_value) {
                Ok(msg) => msg,
                Err(e) => {
                    let err = format!(
                        "Failed to render approval message for step '{}': {:#}",
                        step.step_name, e
                    );
                    fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config)
                        .await?;
                    continue;
                }
            }
        };

        // Transition step to suspended
        JobStepRepo::mark_suspended(pool, job_id, &step.step_name).await?;

        // Store the rendered message as output so it is visible to the API/UI
        sqlx::query("UPDATE job_step SET output = $1 WHERE job_id = $2 AND step_name = $3")
            .bind(serde_json::json!({ "approval_message": rendered_message }))
            .bind(job_id)
            .bind(&step.step_name)
            .execute(pool)
            .await
            .context("Failed to store approval message")?;

        // Transition job to running if still pending
        JobRepo::mark_running_if_pending_server(pool, job_id).await?;

        tracing::info!(
            job_id = %job_id,
            workspace = %workspace_name,
            step = %step.step_name,
            "Approval step suspended, waiting for approval"
        );
    }

    Ok(())
}

/// Fire `on_suspended` hooks for any approval steps that are already suspended
/// when a job is first created.
///
/// Called immediately after [`create_job_for_task`] from every call-site that
/// has access to [`AppState`] (API handler, scheduler, webhook handler, MCP
/// tools, and `fire_single_hook` for type:task hooks).
///
/// Root-level approval steps (no dependencies) are suspended inside
/// `create_job_for_task_inner`, but at that point we only have a `&PgPool` and
/// cannot call into the hooks module.  This function bridges the gap by doing a
/// lightweight post-creation sweep.
#[tracing::instrument(skip(state, workspace_config))]
pub async fn fire_initial_suspended_hooks(
    state: &crate::state::AppState,
    workspace_config: &stroem_common::models::workflow::WorkspaceConfig,
    workspace_name: &str,
    task_name: &str,
    job_id: uuid::Uuid,
) {
    let steps = match JobStepRepo::get_steps_for_job(&state.pool, job_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                job_id = %job_id,
                "fire_initial_suspended_hooks: failed to load steps: {:#}",
                e
            );
            return;
        }
    };

    let job = match JobRepo::get(&state.pool, job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            tracing::warn!(job_id = %job_id, "fire_initial_suspended_hooks: job not found");
            return;
        }
        Err(e) => {
            tracing::error!(
                job_id = %job_id,
                "fire_initial_suspended_hooks: failed to load job: {:#}",
                e
            );
            return;
        }
    };

    let task = match workspace_config.tasks.get(task_name) {
        Some(t) => t,
        None => {
            tracing::warn!(
                job_id = %job_id,
                task = %task_name,
                "fire_initial_suspended_hooks: task not found in workspace"
            );
            return;
        }
    };

    for step in &steps {
        if step.status != stroem_common::models::job::StepStatus::Suspended.as_ref() {
            continue;
        }

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
            workspace_config,
            &job,
            task,
            &step.step_name,
            &rendered_message,
        )
        .await;
    }
}

/// Run the orchestrator after a server-dispatched step (approval, task) is
/// marked failed outside the worker `complete_step` path. Cascade-skips
/// downstream steps and closes the job as failed if everything is terminal.
/// Without this, dependents stay `pending` and the job sits in `running`
/// forever (prod job 201012e5, 2026-09-07).
async fn orchestrate_after_server_step_failure(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    task: &stroem_common::models::workflow::TaskDef,
    workspace_config: &WorkspaceConfig,
) {
    if let Err(e) =
        crate::settlement::cascade_and_settle(pool, job_id, task, workspace_config).await
    {
        tracing::error!(
            "Failed to orchestrate after server-side step '{}' failure in job {}: {:#}",
            step_name,
            job_id,
            e
        );
    }
}

/// The creator's post-commit initialisation: everything a freshly committed
/// job owes before it is handed back — root-step cascade, `type: task`
/// dispatch, `type: approval` dispatch, settlement. Returns the settled
/// status when the job reached a terminal state during initialisation.
///
/// Compensation on `Err` stays with the caller (`create_job_for_task_inner`):
/// it belongs to the creation transaction's contract, not to settlement.
pub async fn init(
    pool: &PgPool,
    workspaces: &WorkspaceManager,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &TaskDef,
    defaults: JobDefaults,
) -> Result<Option<JobStatus>> {
    crate::cascade::execute(pool, job_id, task, Some(workspace_config))
        .await
        .context("creation-time step cascade")?;

    handle_task_steps(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        job_id,
        task,
        defaults,
    )
    .await?;

    handle_approval_steps(pool, workspace_config, workspace_name, job_id, task)
        .await
        .context("dispatch initial approval steps")?;

    let settled = crate::settlement::settle_if_all_terminal(pool, job_id, task)
        .await
        .context("settle job at creation")?;
    if let Some(ref status) = settled {
        tracing::info!(job_id = %job_id, ?status, "All steps terminal at creation — job settled");
    }
    Ok(settled)
}
