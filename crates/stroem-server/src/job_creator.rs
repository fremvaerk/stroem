use anyhow::{bail, Context, Result};
use sqlx::{self, PgPool};
use std::collections::HashMap;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::resolve_step_retry_config;
use stroem_common::models::workflow::{ActionDef, BackoffStrategy, FlowStep, WorkspaceConfig};
use stroem_common::template::{
    merge_defaults, prepare_action_input, render_input_map, resolve_connection_inputs,
    resolve_connection_inputs_scoped, ResolveScope,
};
use stroem_common::validation::{compute_required_ability, compute_required_tags, derive_runner};
use stroem_db::{JobRepo, JobRow, JobStepRepo, NewJobStep};
use uuid::Uuid;

use crate::config::{AgentsConfig, JobDefaults};
use crate::workspace::WorkspaceManager;
use crate::workspace_set::WorkspaceSet;

/// Maximum nesting depth for type: task sub-jobs (prevents infinite recursion)
///
/// `pub(crate)` so `hooks::hook_chain_depth` can size its ancestry-walk hop
/// budget off the same constant — up to this many plain `type: task` levels
/// can sit between two `hook` links in a job's ancestry.
pub(crate) const MAX_TASK_DEPTH: u32 = 10;

/// Result of job creation. `terminal_at_creation` is true when every step was
/// already terminal once creation-time promotion/expansion/dispatch finished
/// (e.g. all root steps skipped by `when`, or a server-dispatched root step
/// failed) — the caller must then run `job_recovery::finalize_created_job`
/// so hooks/metrics/log-archive fire exactly as for an orchestrator-settled job.
///
/// It is also true when post-commit initialisation itself failed: promotion,
/// `type: task` dispatch, `type: approval` dispatch and settlement all run
/// inside one coordinated result, and any error there compensates the job to
/// `failed` (job row and every non-terminal step, in one transaction) rather
/// than returning a 500 over a committed job. A DB outage during that
/// compensation still surfaces as an error to the caller.
#[derive(Debug, Clone, Copy)]
pub struct CreatedJob {
    pub job_id: Uuid,
    pub terminal_at_creation: bool,
}

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

/// Create a job and its steps for a task in a workspace.
///
/// Shared by the API handler (`execute_task`) and the scheduler.
/// Pass `agents_config` to enable initial dispatch of ready `type: agent` steps
/// without waiting for the orchestrator to trigger them.
///
/// `source_job_id` — when set, the new job is treated as a Re-run of that job.
/// Sentinel values in `input` are resolved against the source job's `raw_input`,
/// and both `raw_input` and `source_job_id` are persisted on the new job row.
///
/// **Warning: drops the `terminal_at_creation` flag.** Production callers must
/// use [`create_job_for_task_detailed`] and then call
/// `job_recovery::finalize_created_job`, or a job that settles synchronously at
/// creation never fires its hooks, metric, log archive or parent propagation.
/// Kept for tests.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip(pool, workspaces, workspace_config, agents_config, input))]
pub async fn create_job_for_task(
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
) -> Result<Uuid> {
    create_job_for_task_detailed(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        task_name,
        input,
        source_type,
        source_id,
        revision,
        source_job_id,
        agents_config,
        defaults,
    )
    .await
    .map(|c| c.job_id)
}

/// Like [`create_job_for_task`] but also reports `terminal_at_creation`.
/// HTTP/MCP/scheduler entry points use this and call
/// `job_recovery::finalize_created_job` afterwards.
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
/// caller must run `job_recovery::finalize_created_job` when it is true (a
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

/// Create a child job with parent tracking.
///
/// Used by `handle_task_steps` to create sub-jobs that propagate back to the
/// parent step on completion.
///
/// **Warning: drops the `terminal_at_creation` flag.** Production callers must
/// use [`create_child_job_for_task_detailed`] and then call
/// `job_recovery::finalize_created_job` — or, for `agent_tool` children, reject
/// the terminal case outright, since propagation of an agent-tool result
/// depends on the worker having recorded the child id first. Kept for tests.
#[allow(clippy::too_many_arguments)]
pub async fn create_child_job_for_task(
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
) -> Result<Uuid> {
    create_child_job_for_task_detailed(
        workspaces,
        pool,
        workspace_config,
        workspace_name,
        task_name,
        input,
        source_type,
        source_id,
        parent_job_id,
        parent_step_name,
        revision,
        defaults,
    )
    .await
    .map(|c| c.job_id)
}

/// Like [`create_child_job_for_task`] but also reports `terminal_at_creation`,
/// so the caller can finalize (or reject) a child that settled synchronously.
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

/// Create a job with parent tracking (for type: task sub-jobs).
#[allow(clippy::too_many_arguments)]
fn create_job_for_task_inner<'a>(
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
            .context("Failed to merge input defaults")?;

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
            .context("Failed to resolve connection inputs")?;

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

            let (owned_action, action_workspace, action_revision, action_name) = if is_cross {
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
                (a, None, None, flow_step.action.clone())
            };
            let action = &owned_action;

            // Fail fast (400) on literal connection references the worker would
            // otherwise reject at claim time. Templated values cannot be checked here.
            precheck_literal_connection_inputs(
                step_name,
                flow_step,
                action,
                &ws_set,
                workspace_name,
                action_workspace.as_deref(),
            )?;

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

            let action_spec = serde_json::to_value(action).ok();
            let required_ability = compute_required_ability(action);
            let required_tags = compute_required_tags(action);
            let runner = derive_runner(action);
            let retry = resolve_step_retry_config(flow_step, action);

            new_steps.push(NewJobStep {
                job_id,
                step_name: step_name.clone(),
                action_name,
                action_type: action.action_type.clone(),
                action_image: action.image.clone(),
                action_spec,
                input: Some(serde_json::to_value(&flow_step.input).unwrap_or_default()),
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
                .map(|d| i32::try_from(d.as_secs()).expect("timeout validated to fit i32"))
                .or(defaults.job_timeout_secs),
            revision,
            raw_input_to_persist,
            lineage_source_job_id,
            restart_from_step,
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
        let init: Result<Option<stroem_common::models::job::JobStatus>> = async {
            // Promote/skip/expand root steps. Runs unconditionally: cheap when
            // nothing is promotable, and required for Plan B's seeded jobs.
            crate::cascade::execute(pool, job_id, task, Some(workspace_config))
                .await
                .context("creation-time step cascade")?;

            handle_task_steps(workspaces, pool, workspace_config, workspace_name, job_id, task, defaults)
                .await?;

            handle_approval_steps(pool, workspace_config, workspace_name, job_id, task)
                .await
                .context("dispatch initial approval steps")?;

            // Shared settlement — identical rules to the orchestrator path.
            let settled = crate::orchestrator::settle_if_all_terminal(pool, job_id, task)
                .await
                .context("settle job at creation")?;
            if let Some(ref status) = settled {
                tracing::info!(job_id = %job_id, ?status, "All steps terminal at creation — job settled");
            }
            Ok(settled)
        }
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
                return Ok(CreatedJob {
                    job_id,
                    terminal_at_creation: true,
                });
            }
        };

        Ok(CreatedJob {
            job_id,
            terminal_at_creation: settled.is_some(),
        })
    })
}

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
                            tracing::error!("{}", err);
                            JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
                            orchestrate_after_server_step_failure(
                                pool,
                                job_id,
                                &step.step_name,
                                task,
                                workspace_config,
                            )
                            .await;
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
                    tracing::error!("{}", err);
                    JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
                    orchestrate_after_server_step_failure(
                        pool,
                        job_id,
                        &step.step_name,
                        task,
                        workspace_config,
                    )
                    .await;
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
    // Job metadata: always present so `when` expressions and step inputs can
    // reference `{{ job.revision }}` without undefined-variable errors.
    // Inserted BEFORE step outputs so a step literally named `job` shadows it
    // (backward compatibility for workflows predating job metadata).
    ctx.insert(
        "job".to_string(),
        crate::web::worker_api::rendering::job_context(job.revision.as_deref()),
    );
    for s in steps {
        // Skip loop instance steps — only the placeholder's aggregated output
        // should be in the context (under the original step name)
        if s.loop_source.is_some() {
            continue;
        }
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
        } else if s.status == StepStatus::Suspended.as_ref() {
            // Include suspended (approval) steps with null output so downstream
            // `when` expressions can reference them without Tera undefined errors.
            let mut step_ctx = serde_json::Map::new();
            step_ctx.insert("output".to_string(), serde_json::Value::Null);
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
    if let Err(e) = crate::orchestrator::on_step_completed(
        pool,
        job_id,
        step_name,
        task,
        Some(workspace_config),
    )
    .await
    {
        tracing::error!(
            "Failed to orchestrate after server-side step '{}' failure in job {}: {:#}",
            step_name,
            job_id,
            e
        );
    }
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

/// Resolve the flow step's connection-typed inputs that are plain string
/// literals (no `{{`), using the same scope the claim path will use, so an
/// author mistake surfaces as a job-creation error instead of a failed step.
fn precheck_literal_connection_inputs(
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
            retry_of_job_id: None,
            retry_job_id: None,
            retry_attempt: 0,
            max_retries: None,
            raw_input: None,
            source_job_id: None,
            restart_from_step: None,
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
            output,
            status: status.to_string(),
            required_ability: "script".to_string(),
            required_tags: json!([]),
            runner: "local".to_string(),
            retry_history: json!([]),
            ..Default::default()
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

    #[test]
    fn test_build_step_render_context_suspended_step_has_null_output() {
        // Suspended approval steps should be included with null output so
        // downstream `when` expressions don't get Tera "undefined variable" errors.
        let job = make_job(None);
        let steps = vec![
            make_step(job.job_id, "build", "completed", Some(json!({"tag": "v1"}))),
            make_step(job.job_id, "approve", "suspended", None),
        ];
        let ws = WorkspaceConfig::new();

        let ctx = build_step_render_context(&job, &steps, &ws);

        assert_eq!(ctx["build"]["output"]["tag"], "v1");
        // Suspended step should be present with null output
        assert!(ctx.get("approve").is_some());
        assert!(ctx["approve"]["output"].is_null());
    }

    // --- build_step_render_context with for_each aggregated output ---

    #[test]
    fn test_build_step_render_context_includes_loop_placeholder_output() {
        // When a for_each placeholder step completes with an aggregated array output,
        // downstream steps should be able to reference it via the render context.
        let job = make_job(Some(json!({"env": "prod"})));
        let mut placeholder = make_step(
            job.job_id,
            "process",
            "completed",
            Some(json!(["result1", "result2"])),
        );
        placeholder.for_each_expr = Some(r#"["a","b"]"#.to_string());
        let steps = vec![placeholder];
        let ws = WorkspaceConfig::new();

        let ctx = build_step_render_context(&job, &steps, &ws);

        // The aggregated output (an array) should be accessible
        assert_eq!(ctx["process"]["output"], json!(["result1", "result2"]));
    }

    // --- build_step_render_context: job metadata ---

    #[test]
    fn test_build_step_render_context_includes_job_revision() {
        let mut job = make_job(None);
        job.revision = Some("abc123def".to_string());
        let ws = WorkspaceConfig::default();

        let ctx = build_step_render_context(&job, &[], &ws);

        assert_eq!(ctx["job"]["revision"], json!("abc123def"));
    }

    #[test]
    fn test_build_step_render_context_job_revision_null_when_missing() {
        // `job` must always be present (with revision: null) so `when`
        // expressions referencing job.revision never hit an undefined variable.
        let job = make_job(None);
        let ws = WorkspaceConfig::default();

        let ctx = build_step_render_context(&job, &[], &ws);

        let job_obj = ctx
            .get("job")
            .expect("`job` must always be present in the render context");
        assert_eq!(job_obj["revision"], serde_json::Value::Null);
    }

    #[test]
    fn test_build_step_render_context_step_named_job_shadows_job_metadata() {
        // A completed step literally named `job` must keep its output —
        // backward compatibility for workflows predating job metadata.
        let mut job = make_job(None);
        job.revision = Some("abc123def".to_string());
        let steps = vec![make_step(
            job.job_id,
            "job",
            "completed",
            Some(json!({"result": "step-wins"})),
        )];
        let ws = WorkspaceConfig::default();

        let ctx = build_step_render_context(&job, &steps, &ws);

        assert_eq!(ctx["job"]["output"]["result"], json!("step-wins"));
    }

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
}
