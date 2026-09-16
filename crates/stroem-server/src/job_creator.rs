use anyhow::{bail, Context, Result};
use sqlx::{self, PgPool};
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::resolve_step_retry_config;
use stroem_common::models::workflow::{
    ActionDef, BackoffStrategy, FlowStep, TaskDef, WorkspaceConfig,
};
use stroem_common::template::{
    merge_defaults, resolve_connection_inputs, resolve_connection_inputs_scoped, ResolveScope,
};
use stroem_common::validation::{compute_required_ability, compute_required_tags, derive_runner};
use stroem_db::{JobRepo, JobRow, JobStepRepo, NewJobStep};
use uuid::Uuid;

use crate::config::{AgentsConfig, JobDefaults};
use crate::settlement::CreatedJob;
use crate::workspace::WorkspaceManager;
use crate::workspace_set::WorkspaceSet;

/// Maximum nesting depth for type: task sub-jobs (prevents infinite recursion)
///
/// `pub(crate)` so `hooks::hook_chain_depth` can size its ancestry-walk hop
/// budget off the same constant — up to this many plain `type: task` levels
/// can sit between two `hook` links in a job's ancestry.
pub(crate) const MAX_TASK_DEPTH: u32 = 10;

/// How a job comes into being. Replaces the positional `source_job_id`, which
/// used to mean both "resolve Re-run sentinels against this job" and "persist
/// this lineage pointer".
pub enum CreationMode<'a> {
    /// Plain creation: API, scheduler, webhook, `type: task` child, hook.
    Normal,
    /// User clicked Re-run: `••••••` sentinels in `input` are replaced from the
    /// source's `raw_input`; `source_job_id` is persisted.
    Rerun { source_job_id: Uuid },
    /// Restart From Step (spec 2026-09-07): `input` is the source's `raw_input`
    /// replayed through the normal pipeline; carried rows are seeded in the
    /// creation transaction; lineage + `restart_from_step` are persisted.
    Restart {
        source: &'a JobRow,
        from_step: &'a str,
        plan: &'a crate::restart::RestartPlan,
    },
}

/// Create a job and its steps for a task in a workspace, reporting
/// `terminal_at_creation`.
///
/// Shared by the API handler (`execute_task`) and the scheduler.
/// Pass `agents_config` to enable initial dispatch of ready `type: agent` steps
/// without waiting for the orchestrator to trigger them.
///
/// `source_job_id` — when set, the new job is treated as a Re-run of that job.
/// Sentinel values in `input` are resolved against the source job's `raw_input`,
/// and both `raw_input` and `source_job_id` are persisted on the new job row.
///
/// HTTP/MCP/scheduler entry points use this and call
/// `Settlement::job_created` afterwards.
#[allow(clippy::too_many_arguments)]
pub async fn create_job_for_task_detailed(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    task_name: &str,
    input: serde_json::Value,
    source_type: &str,
    source_id: Option<&str>,
    revision: Option<&str>,
    source_job_id: Option<Uuid>,
    agents_config: Option<&AgentsConfig>,
    defaults: JobDefaults,
) -> Result<CreatedJob> {
    create_job_for_task_inner(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        task_name,
        input,
        source_type,
        source_id,
        None,
        None,
        revision,
        match source_job_id {
            Some(id) => CreationMode::Rerun { source_job_id: id },
            None => CreationMode::Normal,
        },
        agents_config,
        defaults,
    )
    .await
}

/// Restart From Step. `plan` comes from [`crate::restart::compute_restart_set`]
/// (also used by the dry-run endpoint). Input is the source's `raw_input`
/// replayed through `merge_defaults` + `resolve_connection_inputs` (spec §4.4),
/// so a restart re-resolves connections and secrets against today's workspace
/// rather than reusing the source's frozen `input`. Legacy sources without
/// `raw_input` are rejected exactly like Re-run.
///
/// Reports `terminal_at_creation` like every other creation entry point — the
/// caller must run `Settlement::job_created` when it is true (a
/// restart whose whole restart set cascades to skipped settles immediately).
#[allow(clippy::too_many_arguments)]
pub async fn create_restart_job(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    source: &JobRow,
    plan: &crate::restart::RestartPlan,
    from_step: &str,
    source_id: Option<&str>,
    revision: Option<&str>,
    defaults: JobDefaults,
) -> Result<CreatedJob> {
    debug_assert!(
        plan.restart_steps.iter().any(|s| s == from_step),
        "restart plan for '{}' does not contain it: {:?} — plan was built for a different step",
        from_step,
        plan.restart_steps
    );
    let raw = source.raw_input.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "Source job {} predates Re-run prefill (no raw_input)",
            source.job_id
        )
    })?;
    create_job_for_task_inner(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        &source.task_name,
        raw,
        "restart",
        source_id,
        None,
        None,
        revision,
        CreationMode::Restart {
            source,
            from_step,
            plan,
        },
        None,
        defaults,
    )
    .await
}

/// Create a child job with parent tracking (for `type: task` sub-jobs),
/// reporting `terminal_at_creation` so the caller can finalize (or reject) a
/// child that settled synchronously.
///
/// Used by `handle_task_steps` to create sub-jobs that propagate back to the
/// parent step on completion.
#[allow(clippy::too_many_arguments)]
pub async fn create_child_job_for_task_detailed(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    task_name: &str,
    input: serde_json::Value,
    source_type: &str,
    source_id: Option<&str>,
    parent_job_id: Uuid,
    parent_step_name: &str,
    revision: Option<&str>,
    defaults: JobDefaults,
) -> Result<CreatedJob> {
    create_job_for_task_inner(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        task_name,
        input,
        source_type,
        source_id,
        Some(parent_job_id),
        Some(parent_step_name),
        revision,
        CreationMode::Normal, // child paths never carry re-run/restart lineage
        None,
        defaults,
    )
    .await
}

/// Error-chain marker recording that a `create_job_for_task_inner` failure
/// occurred while rendering the TASK OWNER's own config (input defaults or
/// connection resolution) — as opposed to a structural error (task not
/// found, a DB failure, a missing required field) or a value the CALLER
/// supplied. After `resolve_task_input_by_provenance` runs upstream in
/// `settlement/dispatch.rs`, every caller-supplied connection-typed value
/// arriving here is already a resolved object; any string this function
/// still tries to resolve as a connection name came from a TASK schema
/// default the task owner itself declared. `handle_task_steps_pass` uses
/// this to decide whether to withhold the error from a caller in a
/// different workspace (spec § 3.3) — find it with
/// `err.downcast_ref::<OwnerSideRender>().is_some()` (called on the
/// `anyhow::Error` itself; see `template::ProvenanceError`'s doc comment
/// for why this, not a `.chain()` walk, is the form that actually finds a
/// `.context(...)` value).
#[derive(Debug)]
pub(crate) struct OwnerSideRender;

impl std::fmt::Display for OwnerSideRender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rendering the task's own defaults")
    }
}

// `downcast_ref::<OwnerSideRender>()` on a chain link requires this — a bare
// `Display + Debug` is not enough for `std::error::Error`'s blanket downcast.
impl std::error::Error for OwnerSideRender {}

/// Create a job with parent tracking (for type: task sub-jobs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_job_for_task_inner<'a>(
    workspaces: &'a WorkspaceManager,
    pool: &'a PgPool,
    workspace_config: &'a WorkspaceConfig,
    workspace_name: &'a str,
    task_name: &'a str,
    input: serde_json::Value,
    source_type: &'a str,
    source_id: Option<&'a str>,
    parent_job_id: Option<Uuid>,
    parent_step_name: Option<&'a str>,
    revision: Option<&'a str>,
    mode: CreationMode<'a>,
    _agents_config: Option<&'a AgentsConfig>,
    defaults: JobDefaults,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CreatedJob>> + Send + 'a>> {
    Box::pin(async move {
        // Look up task
        let task = workspace_config.tasks.get(task_name).with_context(|| {
            format!(
                "Task '{}' not found in workspace '{}'",
                task_name, workspace_name
            )
        })?;

        // Lineage resolution.
        //
        // Re-run flow: resolve any "reuse from source" sentinels in the incoming
        // input by looking up the source's raw_input. Done BEFORE merge_defaults
        // so a sentinel that the source did not override falls through to the
        // schema default (supports secret rotation).
        //
        // Restart flow: `create_restart_job` already passed the source's
        // `raw_input` as `input`, so there is nothing to resolve — only the
        // lineage pointers to persist.
        let mut effective_input = input;
        let (lineage_source_job_id, restart_from_step): (Option<Uuid>, Option<&str>) = match &mode {
            CreationMode::Normal => (None, None),
            CreationMode::Rerun { source_job_id } => {
                let src_id = *source_job_id;
                let source_job = stroem_db::JobRepo::get(pool, src_id)
                    .await
                    .context("fetch source job for re-run")?
                    .ok_or_else(|| anyhow::anyhow!("Source job {} not found", src_id))?;
                if source_job.workspace != workspace_name {
                    bail!(
                        "Source job {} belongs to workspace '{}', cannot Re-run into '{}'",
                        src_id,
                        source_job.workspace,
                        workspace_name
                    );
                }
                let source_raw = match source_job.raw_input {
                    Some(v) => v,
                    None => bail!(
                        "Source job {} predates Re-run prefill (no raw_input)",
                        src_id
                    ),
                };
                effective_input = stroem_common::template::resolve_rerun_sentinels(
                    &effective_input,
                    &source_raw,
                    &task.input,
                )
                .context("resolve re-run sentinels")?;
                (Some(src_id), None)
            }
            CreationMode::Restart {
                source, from_step, ..
            } => {
                if source.workspace != workspace_name {
                    bail!(
                        "Source job {} belongs to workspace '{}', cannot restart into '{}'",
                        source.job_id,
                        source.workspace,
                        workspace_name
                    );
                }
                (Some(source.job_id), Some(*from_step))
            }
        };

        // Capture the user's submission verbatim before defaults/connections are merged.
        let raw_input_to_persist = Some(effective_input.clone());

        // Merge input defaults from the task schema
        let secrets_ctx = serde_json::json!({ "secret": workspace_config.secrets });
        let merged_input = merge_defaults(&effective_input, &task.input, &secrets_ctx)
            .context("Failed to merge input defaults")
            .context(OwnerSideRender)?;

        // Restart replays the source job's `raw_input` with no form in front of
        // it, so a schema that gained a required field without a default since
        // the source ran would otherwise create a job with incomplete input.
        // `merge_defaults` deliberately skips required-field validation (webhook
        // and trigger inputs do not match the task schema), so restart checks it
        // here. The message must contain "required": `classify_execute_error`
        // keys off that word in the OUTERMOST message to return 400, not 500.
        if matches!(mode, CreationMode::Restart { .. }) {
            let present = merged_input.as_object();
            let mut missing: Vec<&str> = task
                .input
                .iter()
                .filter(|(name, field)| {
                    field.required
                        && field.default.is_none()
                        && !present.map(|m| m.contains_key(*name)).unwrap_or(false)
                })
                .map(|(name, _)| name.as_str())
                .collect();
            missing.sort_unstable();
            if !missing.is_empty() {
                bail!(
                    "Restart input is missing required field(s) with no default: {}",
                    missing.join(", ")
                );
            }
        }

        // Resolve connection inputs (replace connection names with full objects).
        // Qualified names (`ws.conn`) resolve against other workspaces, gated by `shared`.
        let ws_set = WorkspaceSet::load(workspaces, workspace_name, Some(workspace_config)).await;
        let resolved_input = resolve_connection_inputs(&merged_input, &task.input, &ws_set)
            .context("Failed to resolve connection inputs")
            .context(OwnerSideRender)?;

        // Build job steps from the task flow
        let mut new_steps = Vec::new();
        // Generate job_id upfront so steps can reference it
        let job_id = Uuid::new_v4();

        for (step_name, flow_step) in &task.flow {
            // flow_step.action may be "owner_ws.action" (cross-workspace) or a local name.
            let (owner_ws, bare_action) =
                stroem_common::template::parse_qualified_ref(&flow_step.action);
            // Cross-workspace only when it isn't already a local/library-flattened key
            // AND the named workspace exists (library precedence + backward compat).
            let is_cross = owner_ws.is_some()
                && !workspace_config.actions.contains_key(&flow_step.action)
                && owner_ws
                    .map(|ws| workspaces.has_workspace(ws))
                    .unwrap_or(false);

            let (owned_action, action_workspace, action_revision, action_name, cross_cfg) =
                if is_cross {
                    let ws = owner_ws.unwrap();
                    let owner_cfg = workspaces.get_config(ws).await.ok_or_else(|| {
                        anyhow::anyhow!(
                            "action '{}': workspace '{}' is not available",
                            flow_step.action,
                            ws
                        )
                    })?;
                    let a = owner_cfg.actions.get(bare_action).cloned().ok_or_else(|| {
                        anyhow::anyhow!(
                            "action '{}': workspace '{}' has no action '{}'",
                            flow_step.action,
                            ws,
                            bare_action
                        )
                    })?;
                    (
                        a,
                        Some(ws.to_string()),
                        workspaces.get_revision(ws),
                        bare_action.to_string(),
                        Some(owner_cfg),
                    )
                } else {
                    let a = workspace_config
                        .actions
                        .get(&flow_step.action)
                        .cloned()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Action '{}' not found in workspace '{}'",
                                flow_step.action,
                                workspace_name
                            )
                        })?;
                    (a, None, None, flow_step.action.clone(), None)
                };
            let action = &owned_action;

            // Fail fast (400) on literal connection references the worker would
            // otherwise reject at claim time. Templated values cannot be checked here.
            if action.action_type == "task" {
                let task_ref = action
                    .task
                    .as_deref()
                    .context("type: task action missing task field")?;
                let base_ws: &str = action_workspace.as_deref().unwrap_or(workspace_name);
                let base_cfg: &WorkspaceConfig = match cross_cfg.as_deref() {
                    Some(c) => c,
                    None => workspace_config,
                };
                let resolved = resolve_task_ref(workspaces, base_ws, base_cfg, task_ref).await?;
                if resolved.workspace == workspace_name && resolved.task_name == task_name {
                    bail!(
                        "task '{}' is a self-reference to '{}/{}' (invalid)",
                        task_ref,
                        workspace_name,
                        task_name
                    );
                }
                precheck_task_step_literals(
                    step_name,
                    flow_step,
                    &resolved,
                    workspaces,
                    workspace_name,
                    base_cfg,
                )
                .await?;
            } else {
                precheck_literal_connection_inputs(
                    step_name,
                    flow_step,
                    action,
                    &ws_set,
                    workspace_name,
                    action_workspace.as_deref(),
                )?;
            }

            let status = if flow_step.for_each.is_some() {
                // For-each steps always start pending — expanded at promotion time
                StepStatus::Pending
            } else if flow_step.depends_on.is_empty() && flow_step.when.is_none() {
                StepStatus::Ready
            } else {
                // Steps with `when` conditions start as pending even if they
                // have no deps — the post-creation promote loop evaluates them.
                StepStatus::Pending
            };

            new_steps.push(build_step(
                job_id,
                step_name,
                action_name,
                flow_step,
                action,
                Some(serde_json::to_value(&flow_step.input).unwrap_or_default()),
                status,
                defaults,
                action_workspace,
                action_revision,
            ));
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
                .map(|d| i32::try_from(d.as_secs()).expect("timeout validated to fit i32"))
                .or(defaults.job_timeout_secs),
            revision,
            raw_input_to_persist,
            lineage_source_job_id,
            restart_from_step,
            // `max_attempts` counts total executions; the DB column counts
            // retries only, so it's stored as `max_attempts - 1` (same
            // convention as the step-level `max_retries` above). Validation
            // guarantees `max_attempts >= 1`, so this subtraction never
            // underflows. Child (`type: task`) jobs carry this too, but
            // `terminal::plan` gates the retry decision on top-level, so
            // they never actually retry.
            task.retry
                .as_ref()
                .map(|r| i32::try_from(r.max_attempts - 1).expect("max_attempts fits i32")),
        )
        .await
        .context("Failed to create job")?;

        JobStepRepo::create_steps_tx(&mut *tx, &new_steps)
            .await
            .context("Failed to create job steps")?;

        // Restart: overwrite the freshly created rows outside the restart set
        // with the source job's terminal state, inside the SAME transaction —
        // a job must never be visible with carried rows still `ready`, or a
        // worker could claim a step that is meant to be skipped entirely.
        if let CreationMode::Restart { plan, .. } = &mode {
            JobStepRepo::seed_steps_tx(&mut tx, job_id, &plan.carried)
                .await
                .context("seed carried-over steps")?;
        }

        tx.commit().await.context("Failed to commit job creation")?;

        metrics::counter!(
            crate::metrics::STROEM_JOBS_CREATED_TOTAL,
            "source_type" => source_type.to_owned(),
        )
        .increment(1);

        tracing::info!("Created job {} with {} steps", job_id, new_steps.len());

        // ── Post-commit initialisation ────────────────────────────────────
        // The job row is committed; anything that fails from here on must be
        // made visible on the job instead of surfacing as a 500 with a
        // committed `pending` job left behind (spec §6.2 / P8).
        //
        // Everything a freshly committed job owes before it can be handed back
        // lives inside this block: root-step promotion/expansion, `type: task`
        // dispatch, `type: approval` dispatch, and final settlement. Approval
        // dispatch in particular MUST be covered — a transient failure before
        // `mark_suspended` leaves an approval step `ready` forever, since
        // neither workers nor the unmatched-step sweep ever touch approvals.
        // Settlement is covered for the same reason: an error there would
        // otherwise return a 500 over a committed, non-terminal job.
        //
        // The block's value is the settled status, if the job reached a
        // terminal state during initialisation.
        let init = crate::settlement::dispatch::init(
            pool,
            workspaces,
            workspace_config,
            workspace_name,
            job_id,
            task_name,
            task,
            defaults,
        )
        .await;

        let settled = match init {
            Ok(settled) => settled,
            Err(e) => {
                let msg = format!("[creation] initialisation failed: {:#}", e);
                tracing::error!(job_id = %job_id, "{}", msg);
                // One transaction: a half-applied compensation would leave
                // failed steps under a non-terminal job with no live step to
                // trigger another sweep.
                let mut tx = pool
                    .begin()
                    .await
                    .context("begin compensation transaction after initialisation error")?;
                JobStepRepo::fail_non_terminal_steps_tx(&mut *tx, job_id, &msg)
                    .await
                    .context("fail steps after initialisation error")?;
                JobRepo::mark_failed_tx(&mut *tx, job_id)
                    .await
                    .context("mark job failed after initialisation error")?;
                tx.commit()
                    .await
                    .context("commit compensation after initialisation error")?;
                return Ok(CreatedJob::new(job_id, true));
            }
        };

        Ok(CreatedJob::new(job_id, settled.is_some()))
    })
}

/// The one place a `NewJobStep` is built from a flow step and its resolved
/// action. Used by job creation and by hook-job creation (spec §8.2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_step(
    job_id: Uuid,
    step_name: &str,
    action_name: String,
    flow_step: &FlowStep,
    action: &ActionDef,
    input: Option<serde_json::Value>,
    status: StepStatus,
    defaults: JobDefaults,
    action_workspace: Option<String>,
    action_revision: Option<String>,
) -> NewJobStep {
    let action_spec = serde_json::to_value(action).ok();
    let required_ability = compute_required_ability(action);
    let required_tags = compute_required_tags(action);
    let runner = derive_runner(action);
    let retry = resolve_step_retry_config(flow_step, action);

    NewJobStep {
        job_id,
        step_name: step_name.to_string(),
        action_name,
        action_type: action.action_type.clone(),
        action_image: action.image.clone(),
        action_spec,
        input,
        status: status.to_string(), // NewJobStep.status is String for DB compatibility
        required_ability,
        required_tags,
        runner,
        timeout_secs: flow_step
            .timeout
            .map(|d| i32::try_from(d.as_secs()).expect("timeout validated to fit i32"))
            .or(defaults.step_timeout_secs),
        when_condition: flow_step.when.clone(),
        for_each_expr: flow_step.for_each.as_ref().map(|v| {
            // Store human-readable form: raw string for templates, compact JSON for arrays
            match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            }
        }),
        loop_source: None,
        loop_index: None,
        loop_total: None,
        loop_item: None,
        // `max_attempts` counts total executions; the DB column counts
        // retries only, so it's stored as `max_attempts - 1`. Validation
        // guarantees `max_attempts >= 1`, so this subtraction never
        // underflows.
        max_retries: retry
            .as_ref()
            .map(|r| i32::try_from(r.max_attempts - 1).expect("max_attempts fits i32")),
        retry_backoff_secs: retry
            .as_ref()
            .map(|r| i32::try_from(r.delay.as_secs()).expect("retry delay fits i32")),
        retry_strategy: retry.as_ref().map(|r| {
            if r.backoff == BackoffStrategy::Exponential {
                "exponential".to_string()
            } else {
                "fixed".to_string()
            }
        }),
        retry_jitter: retry.as_ref().is_some_and(|r| r.jitter),
        action_workspace,
        action_revision,
    }
}

/// Compute the nesting depth of a job by walking the parent chain.
pub(crate) async fn compute_depth(pool: &PgPool, job: &JobRow) -> Result<u32> {
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

/// Which config a resolved task lives in: the base config the caller already
/// holds, or another workspace's snapshot.
#[derive(Debug)]
pub(crate) enum OwnerConfig {
    Base,
    Foreign(Arc<WorkspaceConfig>),
}

/// A `type: task` reference resolved to its owner (spec § 3.1).
#[derive(Debug)]
pub(crate) struct ResolvedTask {
    pub workspace: String,
    pub task_name: String,
    pub task: TaskDef,
    pub config: OwnerConfig,
}

impl ResolvedTask {
    pub(crate) fn config<'a>(&'a self, base: &'a WorkspaceConfig) -> &'a WorkspaceConfig {
        match &self.config {
            OwnerConfig::Base => base,
            OwnerConfig::Foreign(c) => c.as_ref(),
        }
    }
}

/// Resolve `task_ref` relative to `base_ws` (the ACTION's owner): a key of
/// `base_cfg.tasks` first (local and library-flattened names), else a dotted
/// `ws.task` against another loaded workspace. Errors are the outermost
/// message on purpose — `classify_execute_error` keys off them.
pub(crate) async fn resolve_task_ref(
    workspaces: &WorkspaceManager,
    base_ws: &str,
    base_cfg: &WorkspaceConfig,
    task_ref: &str,
) -> Result<ResolvedTask> {
    if let Some(task) = base_cfg.tasks.get(task_ref) {
        return Ok(ResolvedTask {
            workspace: base_ws.to_string(),
            task_name: task_ref.to_string(),
            task: task.clone(),
            config: OwnerConfig::Base,
        });
    }
    if let (Some(ws), name) = stroem_common::template::parse_qualified_ref(task_ref) {
        if !workspaces.has_workspace(ws) {
            bail!("task '{}': unknown workspace '{}'", task_ref, ws);
        }
        let cfg = workspaces.get_config(ws).await.ok_or_else(|| {
            anyhow::anyhow!("task '{}': workspace '{}' is not available", task_ref, ws)
        })?;
        let task = cfg.tasks.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "task '{}': workspace '{}' has no task '{}'",
                task_ref,
                ws,
                name
            )
        })?;
        return Ok(ResolvedTask {
            workspace: ws.to_string(),
            task_name: name.to_string(),
            task,
            config: OwnerConfig::Foreign(cfg),
        });
    }
    bail!("Task '{}' not found in workspace '{}'", task_ref, base_ws)
}

/// Resolve the flow step's connection-typed inputs that are plain string
/// literals (no `{{`), using the same scope the claim path will use, so an
/// author mistake surfaces as a job-creation error instead of a failed step.
pub(crate) fn precheck_literal_connection_inputs(
    step_name: &str,
    flow_step: &FlowStep,
    action: &ActionDef,
    set: &WorkspaceSet,
    caller_ws: &str,
    owner_ws: Option<&str>,
) -> Result<()> {
    if flow_step.when.is_some() {
        // A `when`-guarded step may never run at all (condition false, or the
        // step cascade-skipped). Pre-checking its literal connection inputs
        // at job creation would reject jobs that are perfectly fine to
        // create — keep today's behaviour: a bad literal fails the step at
        // claim time, only if the step is actually reached.
        return Ok(());
    }
    let owner_ws = owner_ws.unwrap_or(caller_ws);
    let mut literal_schema = HashMap::new();
    let mut literal_values = serde_json::Map::new();
    for (field, def) in &action.input {
        if stroem_common::template::PRIMITIVE_TYPES.contains(&def.field_type.as_str()) {
            continue;
        }
        if let Some(serde_json::Value::String(s)) = flow_step.input.get(field) {
            if !s.contains("{{") {
                literal_schema.insert(field.clone(), def.clone());
                literal_values.insert(field.clone(), serde_json::Value::String(s.clone()));
            }
        }
    }
    if literal_schema.is_empty() {
        return Ok(());
    }
    resolve_connection_inputs_scoped(
        &serde_json::Value::Object(literal_values),
        &literal_schema,
        &ResolveScope {
            lookup: set,
            schema_ws: owner_ws,
            value_ws: caller_ws,
            fallback_ws: if owner_ws == caller_ws {
                None
            } else {
                Some(owner_ws)
            },
        },
    )
    .with_context(|| format!("step '{}': failed to resolve connection inputs", step_name))
    .map(|_| ())
}

/// Creation-time pre-check for a `type: task` step (spec § 3.2 item 3): the
/// caller's LITERAL values for the TASK's connection-typed inputs are checked
/// with the same scope dispatch will use (caller first, task owner if shared),
/// including the cross-workspace shape rule. `when`-guarded steps are not
/// pre-checked (the step may never run). The error is wrapped so the
/// classifier's "resolve connection" phrase answers 400; an unavailable owner
/// inside the chain still answers 500.
pub(crate) async fn precheck_task_step_literals(
    step_name: &str,
    flow_step: &FlowStep,
    resolved: &ResolvedTask,
    workspaces: &WorkspaceManager,
    caller_ws: &str,
    base_cfg: &WorkspaceConfig,
) -> Result<()> {
    if flow_step.when.is_some() {
        return Ok(());
    }
    let mut literals = serde_json::Map::new();
    for (field, def) in &resolved.task.input {
        if stroem_common::template::PRIMITIVE_TYPES.contains(&def.field_type.as_str()) {
            continue;
        }
        match flow_step.input.get(field) {
            Some(serde_json::Value::String(s)) if s.contains("{{") => {}
            Some(v) => {
                literals.insert(field.clone(), v.clone());
            }
            None => {}
        }
    }
    if literals.is_empty() {
        return Ok(());
    }
    let t_cfg = resolved.config(base_cfg);
    let set = WorkspaceSet::load(workspaces, &resolved.workspace, Some(t_cfg)).await;
    stroem_common::template::resolve_task_input_by_provenance(
        &serde_json::Value::Object(literals),
        &serde_json::json!({}),
        &resolved.task.input,
        &set,
        caller_ws,
        caller_ws,
        &resolved.workspace,
    )
    .with_context(|| format!("step '{}': failed to resolve connection inputs", step_name))
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precheck_rejects_literal_unshared_ref_and_ignores_templates() {
        use crate::workspace_set::WorkspaceSet;
        use std::sync::Arc;
        use stroem_common::models::workflow::{
            ActionDef, ConnectionDef, ConnectionTypeDef, FlowStep, InputFieldDef, WorkspaceConfig,
        };

        let mut owner = WorkspaceConfig::default();
        owner.connection_types.insert(
            "ch".to_string(),
            ConnectionTypeDef {
                properties: Default::default(),
            },
        );
        owner.connections.insert(
            "private".to_string(),
            ConnectionDef {
                connection_type: Some("ch".into()),
                shared: false,
                values: Default::default(),
            },
        );
        owner.connections.insert(
            "open".to_string(),
            ConnectionDef {
                connection_type: Some("ch".into()),
                shared: true,
                values: Default::default(),
            },
        );
        let caller = WorkspaceConfig::default();
        let set = WorkspaceSet::from_parts(
            "caller",
            Some(&caller),
            vec![("owner".to_string(), Arc::new(owner))],
            vec![],
        );

        let mut action: ActionDef = serde_yaml::from_str("type: script\nscript: echo").unwrap();
        action.input.insert(
            "conn".to_string(),
            InputFieldDef {
                field_type: "owner.ch".to_string(),
                ..serde_yaml::from_str("type: string").unwrap()
            },
        );

        let step = |v: &str| -> FlowStep {
            serde_yaml::from_str(&format!("action: a\ninput:\n  conn: \"{v}\"")).unwrap()
        };

        // Literal, unshared → error mentioning "is not shared"
        let err = precheck_literal_connection_inputs(
            "s",
            &step("owner.private"),
            &action,
            &set,
            "caller",
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("is not shared"), "{err:#}");
        // Literal, shared → ok
        precheck_literal_connection_inputs("s", &step("owner.open"), &action, &set, "caller", None)
            .unwrap();
        // Templated → skipped (no error even though it would not resolve)
        precheck_literal_connection_inputs(
            "s",
            &step("{{ input.pick }}"),
            &action,
            &set,
            "caller",
            None,
        )
        .unwrap();
    }

    #[test]
    fn precheck_skips_when_guarded_step_with_bad_literal() {
        use crate::workspace_set::WorkspaceSet;
        use std::sync::Arc;
        use stroem_common::models::workflow::{
            ActionDef, ConnectionDef, ConnectionTypeDef, FlowStep, InputFieldDef, WorkspaceConfig,
        };

        let mut owner = WorkspaceConfig::default();
        owner.connection_types.insert(
            "ch".to_string(),
            ConnectionTypeDef {
                properties: Default::default(),
            },
        );
        owner.connections.insert(
            "private".to_string(),
            ConnectionDef {
                connection_type: Some("ch".into()),
                shared: false,
                values: Default::default(),
            },
        );
        let caller = WorkspaceConfig::default();
        let set = WorkspaceSet::from_parts(
            "caller",
            Some(&caller),
            vec![("owner".to_string(), Arc::new(owner))],
            vec![],
        );

        let mut action: ActionDef = serde_yaml::from_str("type: script\nscript: echo").unwrap();
        action.input.insert(
            "conn".to_string(),
            InputFieldDef {
                field_type: "owner.ch".to_string(),
                ..serde_yaml::from_str("type: string").unwrap()
            },
        );

        // Same literal, unshared reference as the rejected case above, but
        // this step is `when`-guarded: the pre-check must not run at all, so
        // job creation is not blocked by a step that may never execute.
        let mut step: FlowStep =
            serde_yaml::from_str("action: a\ninput:\n  conn: \"owner.private\"").unwrap();
        step.when = Some("input.flag".to_string());

        precheck_literal_connection_inputs("s", &step, &action, &set, "caller", None).unwrap();
    }

    fn cfg_with_task(task: &str) -> WorkspaceConfig {
        let mut c = WorkspaceConfig::default();
        c.tasks.insert(
            task.to_string(),
            TaskDef {
                name: None,
                description: None,
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow: HashMap::new(),
                timeout: None,
                retry: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
                on_cancel: vec![],
            },
        );
        c
    }

    #[tokio::test]
    async fn resolve_task_ref_local_hit() {
        let a = cfg_with_task("deploy");
        let mgr = WorkspaceManager::from_configs(vec![("A".into(), a.clone(), None)]);
        let r = resolve_task_ref(&mgr, "A", &a, "deploy").await.unwrap();
        assert_eq!(r.workspace, "A");
        assert_eq!(r.task_name, "deploy");
        assert!(matches!(r.config, OwnerConfig::Base));
    }

    #[tokio::test]
    async fn resolve_task_ref_library_flattened_key_wins() {
        let a = cfg_with_task("common.deploy");
        let mgr = WorkspaceManager::from_configs(vec![
            ("A".into(), a.clone(), None),
            ("common".into(), cfg_with_task("deploy"), None),
        ]);
        let r = resolve_task_ref(&mgr, "A", &a, "common.deploy")
            .await
            .unwrap();
        assert_eq!(r.workspace, "A");
        assert_eq!(r.task_name, "common.deploy");
    }

    #[tokio::test]
    async fn resolve_task_ref_qualified_hit() {
        let a = WorkspaceConfig::default();
        let mgr = WorkspaceManager::from_configs(vec![
            ("A".into(), a.clone(), None),
            ("B".into(), cfg_with_task("deploy"), Some("rev-b".into())),
        ]);
        let r = resolve_task_ref(&mgr, "A", &a, "B.deploy").await.unwrap();
        assert_eq!(r.workspace, "B");
        assert_eq!(r.task_name, "deploy");
        assert!(matches!(r.config, OwnerConfig::Foreign(_)));
        assert!(r.config(&a).tasks.contains_key("deploy"));
    }

    #[tokio::test]
    async fn resolve_task_ref_error_phrases() {
        let a = WorkspaceConfig::default();
        let mgr = WorkspaceManager::from_configs(vec![
            ("A".into(), a.clone(), None),
            ("B".into(), cfg_with_task("deploy"), None),
            // Registered so `mark_unavailable_for_test` has an entry to flip;
            // its content is irrelevant once marked unhealthy.
            ("C".into(), cfg_with_task("deploy"), None),
        ]);
        mgr.mark_unavailable_for_test("C");

        let e = resolve_task_ref(&mgr, "A", &a, "Z.deploy")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(e, "task 'Z.deploy': unknown workspace 'Z'");

        let e = resolve_task_ref(&mgr, "A", &a, "B.nope")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(e, "task 'B.nope': workspace 'B' has no task 'nope'");

        let e = resolve_task_ref(&mgr, "A", &a, "deploy")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(e, "Task 'deploy' not found in workspace 'A'");

        // `has_workspace` is true for "C" (an entry exists), but the entry is
        // unhealthy — the "configured but unavailable" state, distinct from
        // "Z" above which has no entry at all.
        let e = resolve_task_ref(&mgr, "A", &a, "C.deploy")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(e, "task 'C.deploy': workspace 'C' is not available");
    }

    /// Task owner `T`: a `deploy` task with a connection-typed input `db`
    /// (type `pg`, declared in `T`) plus a plain-string `name`; and `T`'s own
    /// connection type `pg` with a shared connection `pg-prod` and an
    /// unshared `pg-private`.
    fn precheck_task_config() -> WorkspaceConfig {
        use stroem_common::models::workflow::{ConnectionDef, ConnectionTypeDef, InputFieldDef};

        let mut t = WorkspaceConfig::default();
        t.connection_types.insert(
            "pg".to_string(),
            ConnectionTypeDef {
                properties: Default::default(),
            },
        );
        t.connections.insert(
            "pg-prod".to_string(),
            ConnectionDef {
                connection_type: Some("pg".into()),
                shared: true,
                values: Default::default(),
            },
        );
        t.connections.insert(
            "pg-private".to_string(),
            ConnectionDef {
                connection_type: Some("pg".into()),
                shared: false,
                values: Default::default(),
            },
        );
        let mut input = HashMap::new();
        input.insert(
            "db".to_string(),
            InputFieldDef {
                field_type: "pg".to_string(),
                ..serde_yaml::from_str("type: string").unwrap()
            },
        );
        input.insert(
            "name".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                ..serde_yaml::from_str("type: string").unwrap()
            },
        );
        t.tasks.insert(
            "deploy".to_string(),
            TaskDef {
                name: None,
                description: None,
                mode: "distributed".to_string(),
                folder: None,
                input,
                flow: HashMap::new(),
                timeout: None,
                retry: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
                on_cancel: vec![],
            },
        );
        t
    }

    /// Caller `A`: its own connection `pg-local`, typed against `T`'s `pg`
    /// connection type via the qualified `T.pg` reference.
    fn precheck_caller_config() -> WorkspaceConfig {
        use stroem_common::models::workflow::ConnectionDef;

        let mut a = WorkspaceConfig::default();
        a.connections.insert(
            "pg-local".to_string(),
            ConnectionDef {
                connection_type: Some("T.pg".into()),
                shared: false,
                values: Default::default(),
            },
        );
        a
    }

    fn precheck_task_manager() -> (WorkspaceManager, WorkspaceConfig, WorkspaceConfig) {
        let a_cfg = precheck_caller_config();
        let t_cfg = precheck_task_config();
        let mgr = WorkspaceManager::from_configs(vec![
            ("A".into(), a_cfg.clone(), None),
            ("T".into(), t_cfg.clone(), None),
        ]);
        (mgr, a_cfg, t_cfg)
    }

    fn deploy_step(input_yaml: &str) -> FlowStep {
        serde_yaml::from_str(&format!("action: a\ninput:\n{input_yaml}")).unwrap()
    }

    #[tokio::test]
    async fn precheck_task_step_literals_skips_when_guarded_step() {
        let (mgr, a_cfg, t_cfg) = precheck_task_manager();
        let resolved = ResolvedTask {
            workspace: "T".to_string(),
            task_name: "deploy".to_string(),
            task: t_cfg.tasks.get("deploy").unwrap().clone(),
            config: OwnerConfig::Foreign(Arc::new(t_cfg.clone())),
        };
        // A literal object would normally trip the cross-workspace boundary
        // rule below — the `when` guard must short-circuit before that.
        let mut step = deploy_step("  db: {}");
        step.when = Some("input.flag".to_string());

        precheck_task_step_literals("s", &step, &resolved, &mgr, "A", &a_cfg)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn precheck_task_step_literals_caller_local_connection_ok() {
        let (mgr, a_cfg, t_cfg) = precheck_task_manager();
        let resolved = ResolvedTask {
            workspace: "T".to_string(),
            task_name: "deploy".to_string(),
            task: t_cfg.tasks.get("deploy").unwrap().clone(),
            config: OwnerConfig::Foreign(Arc::new(t_cfg.clone())),
        };
        let step = deploy_step("  db: \"pg-local\"");

        precheck_task_step_literals("s", &step, &resolved, &mgr, "A", &a_cfg)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn precheck_task_step_literals_shared_fallback_ok_unshared_rejected() {
        let (mgr, a_cfg, t_cfg) = precheck_task_manager();
        let resolved = ResolvedTask {
            workspace: "T".to_string(),
            task_name: "deploy".to_string(),
            task: t_cfg.tasks.get("deploy").unwrap().clone(),
            config: OwnerConfig::Foreign(Arc::new(t_cfg.clone())),
        };

        let shared_step = deploy_step("  db: \"pg-prod\"");
        precheck_task_step_literals("s", &shared_step, &resolved, &mgr, "A", &a_cfg)
            .await
            .unwrap();

        let unshared_step = deploy_step("  db: \"pg-private\"");
        let err = precheck_task_step_literals("s", &unshared_step, &resolved, &mgr, "A", &a_cfg)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("is not shared"),
            "chain was: {err:#}"
        );
        assert!(err
            .to_string()
            .contains("failed to resolve connection inputs"));
    }

    #[tokio::test]
    async fn precheck_task_step_literals_foreign_object_and_null_rejected() {
        let (mgr, a_cfg, t_cfg) = precheck_task_manager();
        let resolved = ResolvedTask {
            workspace: "T".to_string(),
            task_name: "deploy".to_string(),
            task: t_cfg.tasks.get("deploy").unwrap().clone(),
            config: OwnerConfig::Foreign(Arc::new(t_cfg.clone())),
        };

        let object_step = deploy_step("  db: {}");
        let err = precheck_task_step_literals("s", &object_step, &resolved, &mgr, "A", &a_cfg)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}")
                .contains("a connection passed across workspaces must be a connection name"),
            "chain was: {err:#}"
        );
        assert!(err
            .to_string()
            .contains("failed to resolve connection inputs"));

        let null_step = deploy_step("  db: null");
        let err = precheck_task_step_literals("s", &null_step, &resolved, &mgr, "A", &a_cfg)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}")
                .contains("a connection passed across workspaces must be a connection name"),
            "chain was: {err:#}"
        );
        assert!(err
            .to_string()
            .contains("failed to resolve connection inputs"));
    }

    #[tokio::test]
    async fn precheck_task_step_literals_same_workspace_object_passthrough_ok() {
        let (mgr, _a_cfg, t_cfg) = precheck_task_manager();
        // base == T == A: the caller IS the task's own workspace, so the
        // cross-workspace boundary rule never applies and an already-object
        // literal passes through unchanged (same as the local-only path).
        let resolved = ResolvedTask {
            workspace: "T".to_string(),
            task_name: "deploy".to_string(),
            task: t_cfg.tasks.get("deploy").unwrap().clone(),
            config: OwnerConfig::Base,
        };
        let step = deploy_step("  db: {}");

        precheck_task_step_literals("s", &step, &resolved, &mgr, "T", &t_cfg)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn precheck_task_step_literals_templated_value_not_checked() {
        let (mgr, a_cfg, t_cfg) = precheck_task_manager();
        let resolved = ResolvedTask {
            workspace: "T".to_string(),
            task_name: "deploy".to_string(),
            task: t_cfg.tasks.get("deploy").unwrap().clone(),
            config: OwnerConfig::Foreign(Arc::new(t_cfg.clone())),
        };
        let step = deploy_step("  db: \"{{ prev.output.db }}\"");

        precheck_task_step_literals("s", &step, &resolved, &mgr, "A", &a_cfg)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn precheck_task_step_literals_primitive_field_skipped() {
        let (mgr, a_cfg, t_cfg) = precheck_task_manager();
        let resolved = ResolvedTask {
            workspace: "T".to_string(),
            task_name: "deploy".to_string(),
            task: t_cfg.tasks.get("deploy").unwrap().clone(),
            config: OwnerConfig::Foreign(Arc::new(t_cfg.clone())),
        };
        // "name" is primitive-typed; an object literal there is never even
        // considered, regardless of the cross-workspace boundary rule.
        let step = deploy_step("  name: {}");

        precheck_task_step_literals("s", &step, &resolved, &mgr, "A", &a_cfg)
            .await
            .unwrap();
    }
}
