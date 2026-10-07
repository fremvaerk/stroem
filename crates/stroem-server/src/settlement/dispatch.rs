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
    merge_action_defaults, render_input_map, render_input_typed,
    resolve_task_input_by_provenance_roles, RoleConfig, RoleScope,
};
use stroem_db::{JobRepo, JobRow, JobStepRepo};
use uuid::Uuid;

use crate::config::JobDefaults;
use crate::job_creator::{
    compute_depth, create_job_for_task_inner, resolve_task_ref, task_at, CreationMode, OwnerConfig,
    ResolvedTask, MAX_TASK_DEPTH,
};
use crate::render_context::{self, JobContext, LoopSlot, Scope, Snapshots};
use crate::workspace::pins::{cannot_be_loaded, PinRef};
use crate::workspace::{ConfigHandle, WorkspaceManager};
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
    // `type: task` steps (e.g. dependents of a failed step whose dependency
    // gate passes because that failed step itself carries
    // `continue_on_failure`) that this pass's snapshot never saw — so loop
    // until a pass fails nothing. Bounded by the flow size: every failing
    // pass retires at least one step.
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
/// Debug-escaped, and any further transformation a filter chain like `{{
/// secret.X | upper | int }}` can apply — Tera's raw text quotes the
/// upper-cased value) that no finite scrub can enumerate (spec § 3.3, "Error
/// scrubbing"). Withholding at the ownership boundary, rather than trying to
/// match one more representation each time a new one is found, is the only
/// rule that actually converges. Since Tera 2 the template error carries none
/// of Tera's text (spec 2026-10-06 § 3.2); scrub and withholding stay as the
/// second and third lines.
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
    // message). Tera's own text quotes the offending value, so a template
    // touching `{{ secret.* }}` would embed the secret; the template error is
    // value-free since Tera 2 (spec 2026-10-06 § 3.2), and this scrub — here,
    // the single choke point both reach, before the message is logged or
    // persisted to `job_step.error_message` / `retry_history` — is the second
    // line.
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

/// `ws` at `pin`, a pin stamped on the step being dispatched (git-refs spec §
/// 7.3). On failure, the `[pin] … cannot be loaded` text the step fails with:
/// it keeps the pin error (F37), and a `PinLoadFailed` only ever as its fixed
/// sentence (`config_for_user`).
async fn load_step_pin(
    workspaces: &WorkspaceManager,
    ws: &str,
    pin: &PinRef,
) -> std::result::Result<ConfigHandle, String> {
    match workspaces.config_for_user(ws, Some(pin)).await {
        Ok(Some(handle)) => Ok(handle),
        Ok(None) => Err(cannot_be_loaded(
            ws,
            pin,
            &anyhow::anyhow!("workspace '{ws}' is not available"),
        )),
        Err(e) => Err(cannot_be_loaded(ws, pin, &e)),
    }
}

/// The task a step's `task_*` stamp names (git-refs spec § 7.3): `T`@commit,
/// looked up by its full key first (library-flattened names) and its bare
/// name otherwise. The pinned config is immutable, so the answer never drifts.
async fn resolve_stamped_task(
    workspaces: &WorkspaceManager,
    t_ws: &str,
    pin: &PinRef,
    task_ref: &str,
) -> Result<ResolvedTask> {
    let cfg = load_step_pin(workspaces, t_ws, pin)
        .await
        .map_err(anyhow::Error::msg)?
        .arc();
    let (task_name, task) = task_at(t_ws, &cfg, task_ref, &pin.git_ref)?;
    Ok(ResolvedTask {
        workspace: t_ws.to_string(),
        task_name,
        task,
        config: OwnerConfig::Foreign(cfg),
    })
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
        job_ref: job.git_ref.as_deref(),
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

        // 1. Action owner O and its config: the action's pin when the step was
        //    resolved through `ref:` (git-refs spec § 7.3), else today's rule.
        let base_ws: &str = step.action_workspace.as_deref().unwrap_or(workspace_name);
        let base_handle: Option<ConfigHandle> = match PinRef::of_step_action(step) {
            Some(pin) => match load_step_pin(workspaces, base_ws, &pin).await {
                Ok(handle) => Some(handle),
                Err(line) => {
                    let err = format!("{} (owner of action '{}')", line, step.action_name);
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
            },
            None if base_ws == workspace_name => None,
            None => match workspaces.get_config(base_ws).await {
                Some(c) => Some(ConfigHandle::Live(c)),
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
            },
        };
        let base_cfg: &WorkspaceConfig = base_handle
            .as_ref()
            .map(|h| h.config())
            .unwrap_or(workspace_config);
        let mut scrub = crate::workspace_set::collect_config_secret_values(base_cfg);

        // 2. Task owner T: the `task_*` stamp when there is one (git-refs spec
        //    § 7.3 — never inferred from the parent job), else today's live
        //    resolution.
        let task_stamp = PinRef::of_step_task(step);
        let resolved_result = match &task_stamp {
            Some((t_ws, pin)) => resolve_stamped_task(workspaces, t_ws, pin, task_ref).await,
            None => resolve_task_ref(workspaces, base_ws, base_cfg, task_ref).await,
        };
        let resolved = match resolved_result {
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
                    // Bucket C lands in task T's input: T's schema decides
                    // which fields are `json` (spec 2026-10-06-json-input-type D6).
                    match render_input_typed(&map, Some(&resolved.task.input), context_value) {
                        Ok(rendered) => rendered,
                        Err(e) => {
                            let err = format!(
                                "Failed to render input for task step '{}': {:#}",
                                step.step_name, e
                            );
                            // Bucket C (caller-side): this is the CALLER's own
                            // input rendering in the CALLER's own context, never
                            // the owner's — never withheld, whatever O/T are.
                            // (No origin marker needed here: this branch
                            // renders only the caller's step input in the
                            // caller's own context, so the phase alone
                            // already identifies the origin for THIS render
                            // call — unlike the owner-side branches (a)/(b)/(c)
                            // below, which is why those need typed markers.)
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
                    // secret.X | upper | int }}`) can transform a secret into
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
        //    Role-scoped (spec § 7.4): A, O and T may be one workspace at
        //    different commits; each bucket reads its own role's config.
        let ws_set = WorkspaceSet::load(workspaces, &resolved.workspace, Some(t_cfg)).await;
        let roles = RoleScope {
            caller: RoleConfig {
                workspace: workspace_name,
                config: workspace_config,
            },
            action_owner: Some(RoleConfig {
                workspace: base_ws,
                config: base_cfg,
            }),
            task_owner: Some(RoleConfig {
                workspace: &resolved.workspace,
                config: t_cfg,
            }),
            others: &ws_set,
        };
        let rendered_input = match resolve_task_input_by_provenance_roles(
            &caller_bucket,
            &default_bucket,
            &resolved.task.input,
            &roles,
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

        // 7. Revision + pin (git-refs spec § 7.3): a stamped task runs its
        //    stamped commit; an unstamped same-workspace child of an UNPINNED
        //    parent inherits the parent's revision (spec § 3.3 step 7); any
        //    other unstamped child is live and takes T's current revision.
        let (revision, child_git_ref): (Option<String>, Option<String>) = match &task_stamp {
            Some((_, pin)) => (Some(pin.commit.clone()), Some(pin.git_ref.clone())),
            None if resolved.workspace == job.workspace && job.git_ref.is_none() => {
                (job.revision.clone(), None)
            }
            None => (workspaces.get_revision(&resolved.workspace), None),
        };

        // Create child job with parent tracking, at the revision (and pin)
        // chosen in step 7.
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
            child_git_ref.as_deref(),
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
        job_ref: job.git_ref.as_deref(),
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
///
/// The config comes from the job row (git-refs spec § 7.4), never from the
/// creating caller: for a cross-workspace or pinned target it is the target's.
#[tracing::instrument(skip_all, fields(job_id = %job_id))]
pub async fn fire_initial_suspended_hooks(state: &crate::state::AppState, job_id: uuid::Uuid) {
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

    let Some((handle, task)) = job_config_and_task(state, &job).await else {
        return;
    };
    let workspace_config = handle.config();

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
            &task,
            &step.step_name,
            &rendered_message,
        )
        .await;
    }
}

/// The job's OWN config and task (git-refs spec § 7.3, § 7.4), for firing its
/// `on_suspended` hooks outside `advance`: a pinned job's hook definitions
/// come from its commit, the same commit `fire_single_hook` stamps on the hook
/// job. `None` when either is unavailable, logged; a pin that cannot load is
/// also written to the job log, since its hooks then do not fire.
pub(crate) async fn job_config_and_task(
    state: &crate::state::AppState,
    job: &JobRow,
) -> Option<(ConfigHandle, TaskDef)> {
    let pin = PinRef::of_job(job);
    let handle = match state
        .workspaces
        .config_for_user(&job.workspace, pin.as_ref())
        .await
    {
        Ok(Some(h)) => h,
        Ok(None) => {
            tracing::warn!(
                job_id = %job.job_id,
                "workspace '{}' not available — on_suspended hooks not fired",
                job.workspace
            );
            return None;
        }
        Err(e) => {
            // `config_for_user` already withheld a `PinLoadFailed` (its
            // scrubbed chain went to `tracing::error!` only).
            let detail = match &pin {
                Some(pin) => cannot_be_loaded(&job.workspace, pin, &e),
                None => format!("{e:#}"),
            };
            let line = format!(
                "[hooks] on_suspended hooks not fired: {}",
                state.workspaces.scrub_live(&job.workspace, &detail).await
            );
            tracing::error!(job_id = %job.job_id, "{line}");
            state.append_server_log(job.job_id, &line).await;
            return None;
        }
    };
    let Some(task) = handle.config().tasks.get(&job.task_name).cloned() else {
        tracing::warn!(
            job_id = %job.job_id,
            task = %job.task_name,
            "task not found in workspace — on_suspended hooks not fired"
        );
        return None;
    };
    Some((handle, task))
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
    // The job's own state partition (spec § 7.6). Best-effort: job creation
    // must never fail on this lookup.
    let git_ref = match JobRepo::get(pool, job_id).await {
        Ok(job) => job.and_then(|j| j.git_ref),
        Err(e) => {
            tracing::warn!(%job_id, "Failed to load job for init snapshot partition: {:#}", e);
            None
        }
    };
    let snapshots = render_context::latest_snapshots(
        pool,
        workspace_name,
        task_name,
        git_ref.as_deref(),
        "init",
    )
    .await;

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
