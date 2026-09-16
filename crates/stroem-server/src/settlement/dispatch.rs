//! Server-side step dispatch: `type: task` and `type: approval` steps are
//! never claimed by workers, so their execution lives here instead of the
//! worker claim path. Also owns the creation-time post-commit initialisation
//! (`init`) that a freshly committed job needs before it can be handed back.

use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::job::{JobStatus, StepStatus};
use stroem_common::models::workflow::{InputFieldDef, TaskDef, WorkspaceConfig};
use stroem_common::template::{
    merge_action_defaults, render_input_map, resolve_task_input_by_provenance,
};
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

use crate::config::JobDefaults;
use crate::job_creator::{
    compute_depth, create_job_for_task_inner, resolve_task_ref, CreationMode, MAX_TASK_DEPTH,
};
use crate::render_context::{self, JobContext, LoopSlot, Scope, Snapshots};
use crate::workspace::WorkspaceManager;
use crate::workspace_set::WorkspaceSet;

/// Create sub-jobs for any "ready" type:task steps in a job.
///
/// Called after job creation and after orchestrator promotes steps.
/// This is the server-side dispatch for task-action steps — workers never claim them.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip(workspaces, pool, workspace_config, snapshots))]
pub async fn handle_task_steps(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &stroem_common::models::workflow::TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
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
            snapshots,
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
///
/// `err` is always scrubbed (`redact_secrets_in_str`) and logged via
/// `tracing::error!`, so operators always see the full (scrubbed) chain in
/// the server log. `persist_override`, when `Some`, is what gets persisted
/// to `job_step.error_message` (and so returned by REST/MCP) INSTEAD of the
/// scrubbed `err` — used when an owner-side render error's value can take an
/// unbounded number of representations (raw, JSON-escaped, Rust
/// Debug-escaped, and any further wrapping a filter chain like `{{ secret.X
/// | json_encode | round }}` can apply) that no finite scrub can enumerate
/// (spec § 3.3, "Error scrubbing"). Withholding at the ownership boundary,
/// rather than trying to match one more representation each time a new one
/// is found, is the only rule that actually converges.
#[allow(clippy::too_many_arguments)]
async fn fail_task_step(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    err: &str,
    task: &stroem_common::models::workflow::TaskDef,
    workspace_config: &WorkspaceConfig,
    snapshots: &Snapshots,
    extra_secret_values: &[String],
    persist_override: Option<&str>,
) -> Result<()> {
    // Two of the callers pass a Tera render error (task-step input, approval
    // message), and Tera quotes the offending value — so a template touching
    // `{{ secret.* }}` embeds the secret. Scrub here, the single choke point
    // both reach, before the message is logged or persisted to
    // `job_step.error_message` / `retry_history`.
    //
    // The caller's own secrets plus — for a `type: task` step — the action
    // owner's and task owner's (spec § 3.3), because an owner-side default
    // rendering error quotes the owner's value.
    let mut secret_values = crate::workspace_set::collect_config_secret_values(workspace_config);
    secret_values.extend_from_slice(extra_secret_values);
    let scrubbed = crate::workspace_set::redact_secrets_in_str(err, &secret_values);

    tracing::error!("{}", scrubbed);
    let persisted = persist_override.unwrap_or(&scrubbed);
    JobStepRepo::mark_failed(pool, job_id, step_name, persisted).await?;
    orchestrate_after_server_step_failure(
        pool,
        job_id,
        step_name,
        task,
        workspace_config,
        snapshots,
    )
    .await;
    Ok(())
}

/// Build the value-free message persisted for an owner-side render error
/// when the step crosses a workspace boundary (spec § 3.3): no representation
/// of the owner's value, not even a scrubbed one, ever reaches the caller's
/// job. `owner` is the workspace whose config/secrets were being rendered —
/// the action owner O for the action-defaults merge and the connection
/// resolution, the task owner T for child-job creation.
fn withheld_owner_render_error(step_name: &str, owner: &str) -> String {
    format!(
        "Failed to prepare input for task step '{step_name}': rendering in workspace '{owner}' \
         failed (details withheld from this job; see the server log or validate the owner \
         workspace)"
    )
}

/// One dispatch pass over the currently-ready `type: task` steps.
/// Returns `true` if any step was marked failed during this pass.
#[allow(clippy::too_many_arguments)]
async fn handle_task_steps_pass(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &stroem_common::models::workflow::TaskDef,
    defaults: JobDefaults,
    snapshots: &Snapshots,
) -> Result<bool> {
    let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
    let job = JobRepo::get(pool, job_id).await?.context("Job not found")?;
    let mut failed_any = false;

    let job_ctx = JobContext {
        job_id,
        job_input: job.input.as_ref(),
        caller_secrets: &workspace_config.secrets,
        owner_secrets: &workspace_config.secrets,
        snapshots,
        job_revision: job.revision.as_deref(),
    };
    let step_views = render_context::views(&steps);

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
            fail_task_step(
                pool,
                job_id,
                &step.step_name,
                &err,
                task,
                workspace_config,
                snapshots,
                &[],
                None,
            )
            .await?;
            failed_any = true;
            continue;
        }

        // 1. Action owner O and its config snapshot.
        let base_ws: &str = step.action_workspace.as_deref().unwrap_or(workspace_name);
        let base_arc = if base_ws == workspace_name {
            None
        } else {
            workspaces.get_config(base_ws).await
        };
        let base_cfg: &WorkspaceConfig = if base_ws == workspace_name {
            workspace_config
        } else {
            match base_arc.as_deref() {
                Some(c) => c,
                None => {
                    let err = format!(
                        "workspace '{}' is not available (owner of action '{}')",
                        base_ws, step.action_name
                    );
                    fail_task_step(
                        pool,
                        job_id,
                        &step.step_name,
                        &err,
                        task,
                        workspace_config,
                        snapshots,
                        &[],
                        None,
                    )
                    .await?;
                    failed_any = true;
                    continue;
                }
            }
        };
        let mut scrub = crate::workspace_set::collect_config_secret_values(base_cfg);

        // 2. Task owner T.
        let resolved = match resolve_task_ref(workspaces, base_ws, base_cfg, task_ref).await {
            Ok(r) => r,
            Err(e) => {
                let err = format!(
                    "Failed to resolve task for step '{}': {:#}",
                    step.step_name, e
                );
                fail_task_step(
                    pool,
                    job_id,
                    &step.step_name,
                    &err,
                    task,
                    workspace_config,
                    snapshots,
                    &scrub,
                    None,
                )
                .await?;
                failed_any = true;
                continue;
            }
        };
        let t_cfg: &WorkspaceConfig = resolved.config(base_cfg);
        scrub.extend(crate::workspace_set::collect_config_secret_values(t_cfg));

        // S6: the child task's input. `each` comes from the step row, not a
        // post-hoc patch — `build` owns the whole context.
        let input_ctx = render_context::build(
            &job_ctx,
            &step_views,
            LoopSlot::of(step),
            Scope::ChildTaskInput,
        );
        let context_value = input_ctx.as_value();

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
                    match render_input_map(&map, context_value) {
                        Ok(rendered) => rendered,
                        Err(e) => {
                            let err = format!(
                                "Failed to render input for task step '{}': {:#}",
                                step.step_name, e
                            );
                            // Bucket C (caller-side): this is the CALLER's own
                            // input rendering in the CALLER's own context, never
                            // the owner's — never withheld, whatever O/T are.
                            // (No origin marker needed here: this whole
                            // function only ever renders the caller's step
                            // input, so the phase itself already identifies
                            // the origin.)
                            fail_task_step(
                                pool,
                                job_id,
                                &step.step_name,
                                &err,
                                task,
                                workspace_config,
                                snapshots,
                                &scrub,
                                None,
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

        // 4. Action-level defaults from the PERSISTED action_spec (never a live
        //    lookup), rendered with O's live secrets. Keys it adds are bucket D.
        let action_schema: HashMap<String, InputFieldDef> = match action_spec.get("input") {
            Some(v) if !v.is_null() => match serde_json::from_value(v.clone()) {
                Ok(s) => s,
                Err(e) => {
                    let err = format!(
                        "step '{}': action_spec.input is not an input schema: {}",
                        step.step_name, e
                    );
                    fail_task_step(
                        pool,
                        job_id,
                        &step.step_name,
                        &err,
                        task,
                        workspace_config,
                        snapshots,
                        &scrub,
                        None,
                    )
                    .await?;
                    failed_any = true;
                    continue;
                }
            },
            _ => HashMap::new(),
        };
        let caller_bucket = rendered_input;
        let default_bucket = if action_schema.is_empty() {
            serde_json::json!({})
        } else {
            let secrets_ctx = serde_json::json!({ "secret": &base_cfg.secrets });
            match merge_action_defaults(&caller_bucket, &action_schema, &secrets_ctx) {
                Ok(merged) => {
                    let caller_keys = caller_bucket.as_object().cloned().unwrap_or_default();
                    let mut d = serde_json::Map::new();
                    if let Some(m) = merged.as_object() {
                        for (k, v) in m {
                            if !caller_keys.contains_key(k) {
                                d.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    serde_json::Value::Object(d)
                }
                Err(e) => {
                    let err = format!(
                        "Failed to prepare action input for task step '{}': {:#}",
                        step.step_name, e
                    );
                    // (a) Owner-side default rendering, against O's secrets —
                    // every error `merge_action_defaults` can raise here
                    // renders one of O's OWN template defaults, never a
                    // caller-supplied value, so origin and phase agree: it is
                    // withheld whenever O != A. A filter chain (e.g. `{{
                    // secret.X | json_encode | round }}`) can wrap a secret in
                    // an unbounded number of representations no finite scrub
                    // enumerates.
                    let owner_boundary = base_ws != workspace_name;
                    let persist_override = owner_boundary
                        .then(|| withheld_owner_render_error(&step.step_name, base_ws));
                    fail_task_step(
                        pool,
                        job_id,
                        &step.step_name,
                        &err,
                        task,
                        workspace_config,
                        snapshots,
                        &scrub,
                        persist_override.as_deref(),
                    )
                    .await?;
                    failed_any = true;
                    continue;
                }
            }
        };

        // 5. Connection resolution against the TASK's schema, by provenance.
        let ws_set = WorkspaceSet::load(workspaces, &resolved.workspace, Some(t_cfg)).await;
        let rendered_input = match resolve_task_input_by_provenance(
            &caller_bucket,
            &default_bucket,
            &resolved.task.input,
            &ws_set,
            workspace_name,
            base_ws,
            &resolved.workspace,
        ) {
            Ok(v) => v,
            Err(e) => {
                let err = format!(
                    "Failed to resolve connection inputs for task step '{}': {:#}",
                    step.step_name, e
                );
                // (b) Connection resolution against the task's schema — this
                // can fail on the CALLER's own bucket (a bad literal the
                // caller itself supplied, safe to show) or on the
                // ActionDefault bucket (O's own default, must be withheld
                // when O != A). Decide by ORIGIN, not phase: only withhold
                // an ActionDefault-bucket failure, and only when the
                // boundary is actually crossed.
                let is_owner_side = e
                    .downcast_ref::<stroem_common::template::ProvenanceError>()
                    .is_some_and(|p| {
                        p.bucket == stroem_common::template::ProvenanceBucket::ActionDefault
                    });
                let owner_boundary = is_owner_side && base_ws != workspace_name;
                let persist_override =
                    owner_boundary.then(|| withheld_owner_render_error(&step.step_name, base_ws));
                fail_task_step(
                    pool,
                    job_id,
                    &step.step_name,
                    &err,
                    task,
                    workspace_config,
                    snapshots,
                    &scrub,
                    persist_override.as_deref(),
                )
                .await?;
                failed_any = true;
                continue;
            }
        };

        // Persist rendered input to DB so the job detail API shows resolved values.
        // When O == A the merged input (bucket C + D) is safe to show on the
        // caller's own step, as before. When O != A, bucket D can carry the
        // owner's UNSHARED connections fully resolved (F2) — persist only the
        // caller-supplied bucket C on the parent step; the child job row still
        // gets the full merged `rendered_input` via create_job_for_task_inner below.
        let persisted_input = if base_ws == workspace_name {
            rendered_input.clone()
        } else {
            caller_bucket.clone()
        };
        if let Err(e) =
            JobStepRepo::update_input(pool, job_id, &step.step_name, Some(persisted_input)).await
        {
            tracing::warn!("Failed to persist rendered input: {:#}", e);
        }

        // Mark step as running (server-side, so we don't process it again)
        JobStepRepo::mark_running_server(pool, job_id, &step.step_name).await?;

        // Transition parent job to running if still pending
        JobRepo::mark_running_if_pending_server(pool, job_id).await?;

        let source_id = format!("{}/{}", job_id, step.step_name);

        // 7. Revision: inherit the parent's for a same-workspace child; the
        //    owner's current one for a foreign child (spec § 3.3 step 7).
        let revision: Option<String> = if resolved.workspace == job.workspace {
            job.revision.clone()
        } else {
            workspaces.get_revision(&resolved.workspace)
        };

        // Create child job with parent tracking (inherits parent revision, or
        // the task owner's current revision for a foreign child).
        // agents_config is not available here (handle_task_steps only has pool),
        // so agent steps in child jobs will be dispatched by the orchestrator
        // when it processes the child job's ready steps. `defaults` is threaded
        // through so sub-jobs inherit the server-level timeout defaults.
        match create_job_for_task_inner(
            workspaces,
            pool,
            t_cfg,
            &resolved.workspace,
            &resolved.task_name,
            rendered_input,
            "task",
            Some(&source_id),
            Some(job_id),
            Some(&step.step_name),
            revision.as_deref(),
            CreationMode::Normal, // child task jobs never inherit re-run/restart lineage
            None,                 // agents_config not available; orchestrator will dispatch
            defaults,
        )
        .await
        {
            Ok(created) => {
                if resolved.workspace == job.workspace {
                    tracing::info!(
                        "Created child job {} for task step '{}' -> task '{}'",
                        created.job_id,
                        step.step_name,
                        resolved.task_name
                    );
                } else {
                    tracing::info!(
                        "Created child job {} for task step '{}' -> task '{}' in workspace '{}'",
                        created.job_id,
                        step.step_name,
                        resolved.task_name,
                        resolved.workspace
                    );
                }
            }
            Err(e) => {
                let err = format!(
                    "Failed to create child job for task '{}': {:#}",
                    task_ref, e
                );
                // (c) Child-job creation can fail for structural reasons
                // that must stay visible (the task doesn't exist, a DB
                // error, a missing required field, a nested step's own
                // dispatch failure bubbling up) as well as for T's own
                // input defaults / connections failing to render
                // (`job_creator::OwnerSideRender`). Only the latter is
                // withheld, and only when T actually differs from A — the
                // owner named is T (`resolved.workspace`), since this is
                // T's own config being rendered.
                let is_owner_side = e
                    .downcast_ref::<crate::job_creator::OwnerSideRender>()
                    .is_some();
                let owner_boundary = is_owner_side && resolved.workspace != workspace_name;
                let persist_override = owner_boundary
                    .then(|| withheld_owner_render_error(&step.step_name, &resolved.workspace));
                fail_task_step(
                    pool,
                    job_id,
                    &step.step_name,
                    &err,
                    task,
                    workspace_config,
                    snapshots,
                    &scrub,
                    persist_override.as_deref(),
                )
                .await?;
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
#[tracing::instrument(skip(pool, workspace_config, snapshots))]
pub async fn handle_approval_steps(
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &stroem_common::models::workflow::TaskDef,
    snapshots: &Snapshots,
) -> Result<()> {
    let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
    let job = JobRepo::get(pool, job_id).await?.context("Job not found")?;

    let job_ctx = JobContext {
        job_id,
        job_input: job.input.as_ref(),
        caller_secrets: &workspace_config.secrets,
        owner_secrets: &workspace_config.secrets,
        snapshots,
        job_revision: job.revision.as_deref(),
    };
    let step_views = render_context::views(&steps);

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

        // Phase 1 — the step's flow-level input (resolves templates like
        // {{ prepare.output.changelog }}), rendered against the same context
        // a `type: task` step's input gets.
        let phase1 = render_context::build(
            &job_ctx,
            &step_views,
            LoopSlot::of(step),
            Scope::ChildTaskInput,
        );
        let mut rendered: Option<serde_json::Value> = None;
        if let Some(ref input) = step.input {
            if let Some(input_map) = input.as_object() {
                if !input_map.is_empty() {
                    let map: HashMap<String, serde_json::Value> = input_map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    match render_input_map(&map, phase1.as_value()) {
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
                            rendered = Some(resolved);
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
                                snapshots,
                                &[],
                                None,
                            )
                            .await?;
                            continue;
                        }
                    }
                }
            }
        }

        // Phase 2 — the message. `Scope::ApprovalMessage` owns the rule that
        // a nonempty rendered input replaces `input`, so the message template
        // can say {{ input.changelog }} instead of job-level input.
        let message_ctx = render_context::build(
            &job_ctx,
            &step_views,
            LoopSlot::of(step),
            Scope::ApprovalMessage {
                rendered_input: rendered.as_ref(),
            },
        );

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
            match stroem_common::template::render_template(&raw_message, message_ctx.as_value()) {
                Ok(msg) => msg,
                Err(e) => {
                    let err = format!(
                        "Failed to render approval message for step '{}': {:#}",
                        step.step_name, e
                    );
                    fail_task_step(
                        pool,
                        job_id,
                        &step.step_name,
                        &err,
                        task,
                        workspace_config,
                        snapshots,
                        &[],
                        None,
                    )
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

        crate::settlement::hooks::fire_suspended_hooks(
            &state.settlement(),
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
    snapshots: &Snapshots,
) {
    if let Err(e) =
        crate::settlement::cascade_and_settle(pool, job_id, task, workspace_config, snapshots).await
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
#[allow(clippy::too_many_arguments)]
pub async fn init(
    pool: &PgPool,
    workspaces: &WorkspaceManager,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task_name: &str,
    task: &TaskDef,
    defaults: JobDefaults,
) -> Result<Option<JobStatus>> {
    // One sample per entry (spec §3.4): the creation-time cascade, the
    // `type: task` input and the approval messages all see the same snapshot.
    let snapshots = render_context::latest_snapshots(pool, workspace_name, task_name, "init").await;

    crate::cascade::execute(pool, job_id, task, Some(workspace_config), &snapshots)
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
        &snapshots,
    )
    .await?;

    handle_approval_steps(
        pool,
        workspace_config,
        workspace_name,
        job_id,
        task,
        &snapshots,
    )
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
