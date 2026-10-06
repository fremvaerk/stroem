use super::terminal::{hook_kind, HookKind};
use super::{CreatedJob, Settlement};
use anyhow::Context;
use serde::Serialize;
use sqlx::PgPool;
use stroem_common::models::job::{ActionType, JobStatus, SourceType, StepStatus};
use stroem_common::models::workflow::{FlowStep, HookDef, TaskDef, WorkspaceConfig};
use stroem_common::template::render_input_map;
use stroem_db::{JobRepo, JobStepRepo, JobStepRow};

/// Context available to `on_suspended` hook templates as `hook.*`
#[derive(Debug, Serialize)]
pub struct SuspendedHookContext {
    pub workspace: String,
    pub task_name: String,
    pub job_id: String,
    pub step_name: String,
    pub message: String,
    pub source_type: String,
    pub source_id: Option<String>,
    /// Workspace revision pinned on the job (git SHA or folder hash).
    pub revision: Option<String>,
    /// The ref a pinned job runs at (`job.git_ref`); `None` for unpinned jobs.
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
}

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
    pub artifacts: Vec<HookArtifactMeta>,
    /// Workspace revision pinned on the job (git SHA or folder hash).
    pub revision: Option<String>,
    /// The ref a pinned job runs at (`job.git_ref`); `None` for unpinned jobs.
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
}

/// Info about a single failed step, available in `hook.failed_steps`
#[derive(Debug, Serialize)]
pub struct FailedStepInfo {
    pub step_name: String,
    pub action_name: String,
    pub error_message: Option<String>,
    /// `true` when this row's own flow step has `continue_on_failure`; a
    /// loop instance reads its placeholder's flag.
    pub continue_on_failure: bool,
    /// `true` when this row's own flow step has `continue_on_failure` (spec
    /// 2026-10-01 §6: self-scoped only, no downstream propagation); a loop
    /// instance is judged by its placeholder. Currently identical to
    /// `continue_on_failure` above — kept as a separate field for API
    /// stability.
    pub tolerated: bool,
    /// `true` when this failure was carried over from a restarted job's
    /// source run (`job_step.carried_over`) rather than freshly produced by
    /// this job. Lets an `on_error` hook distinguish "this job just failed"
    /// from "this job settled failed at creation because a Restart-from-step
    /// carried an old failure forward" (spec 2026-09-07 §6.3).
    pub carried_over: bool,
}

/// Metadata for one artifact, available in `hook.artifacts`.
#[derive(Debug, Serialize)]
pub struct HookArtifactMeta {
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub step_name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Path under the server origin (e.g. `/api/jobs/{id}/artifacts/{name}`).
    /// Hook templates can prefix the deployment's public base URL.
    pub url: String,
}

/// Source types whose jobs are "top-level" runs: workspace-level hooks
/// (`on_success` / `on_error` / `on_cancel` / `on_suspended`) fall back to
/// them when the task defines none. Child (`task`), hook, agent-tool and
/// event-source consumer jobs are excluded. `rerun` and `restart` are
/// user-initiated top-level runs just like `user`/`api`.
pub(crate) fn is_top_level_source(source_type: &str) -> bool {
    matches!(
        source_type,
        "api" | "user" | "trigger" | "webhook" | "mcp" | "retry" | "rerun" | "restart"
    )
}

/// Maximum number of `hook` links allowed in a job's ancestry before further
/// hooks stop firing.
///
/// The `source_type == "hook"` recursion guard alone does not bound hook
/// chains: a hook whose action is `type: task` creates a hook job, whose own
/// `type: task` step creates an ordinary `source_type = "task"` child, and that
/// child is no longer hook-sourced — so its task-level hooks fire again. Two
/// tasks referencing each other that way (A's `on_success` runs B, B's flow
/// runs A) produce an unbounded chain of fresh job ids that no CAS can stop.
/// Validation rejects only direct self-reference, so the budget is enforced
/// here at dispatch time.
pub(crate) const MAX_HOOK_CHAIN_DEPTH: usize = 3;

/// Count the `hook` links in a job's ancestry.
///
/// Called only once a job is known to have hooks to fire, so a job with none
/// configured — the overwhelming majority — costs no queries at all.
///
/// A `hook`-sourced job continues from its `source_job_id` — the job that
/// fired it — and adds one to the count; any other job continues from its
/// `parent_job_id` (no count change). A top-level, non-hook job costs no
/// queries at all. A hook row with no `source_job_id` falls back to the UUID
/// prefix of its `source_id` (`{job_id}` or `{job_id}/{hook}`): a server older
/// than migration 048 keeps writing such rows during a rolling deploy, after
/// the backfill ran, and ending the walk there would let a chain crossing the
/// deploy overrun [`MAX_HOOK_CHAIN_DEPTH`].
///
/// The hop budget must be sized in *hook links*, not raw hops: as many as
/// [`crate::job_creator::MAX_TASK_DEPTH`] plain `type: task` levels can sit
/// between two `hook` links (a hook's `type: task` action creates an ordinary
/// task-sourced child, which can itself nest `type: task` steps up to that
/// depth before the chain reaches the next hook-sourced job). Budgeting by a
/// fixed hop count instead let a long task chain between hook links exhaust
/// the walk before `depth` ever reached [`MAX_HOOK_CHAIN_DEPTH`], silently
/// under-counting and leaving the cycle unbounded. `+ 1` covers the terminal
/// hop into the `MAX_HOOK_CHAIN_DEPTH`-th hook job itself. Still fails open on
/// a lookup error or an exhausted budget — under-counting only ever errs
/// towards allowing a hook, never towards suppressing one.
async fn hook_chain_depth(pool: &PgPool, job: &stroem_db::JobRow) -> usize {
    const MAX_HOPS: usize =
        MAX_HOOK_CHAIN_DEPTH * (crate::job_creator::MAX_TASK_DEPTH as usize + 1) + 1;

    let mut depth = 0usize;
    let mut source_type = job.source_type.clone();
    let mut source_job_id = job.source_job_id;
    let mut source_id = job.source_id.clone();
    let mut parent_job_id = job.parent_job_id;

    for _ in 0..MAX_HOPS {
        let next = if source_type == SourceType::Hook.as_ref() {
            depth += 1;
            source_job_id.or_else(|| {
                source_id
                    .as_deref()
                    .and_then(|s| s.split('/').next())
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
            })
        } else {
            parent_job_id
        };

        let Some(next) = next else { break };

        match JobRepo::get(pool, next).await {
            Ok(Some(ancestor)) => {
                source_type = ancestor.source_type;
                source_job_id = ancestor.source_job_id;
                source_id = ancestor.source_id;
                parent_job_id = ancestor.parent_job_id;
            }
            Ok(None) => break,
            Err(e) => {
                // Fail open: an ancestry lookup failure must not silently
                // suppress a legitimate hook.
                tracing::warn!(
                    job_id = %job.job_id,
                    "hook_chain_depth: ancestry lookup failed, treating chain as shallow: {:#}",
                    e
                );
                break;
            }
        }
    }

    depth
}

/// Fire the hooks a terminal `job` owes, deriving the kind from its status.
///
/// Convenience wrapper over [`fire_hooks_of_kind`] for callers that hold a job
/// row but no [`super::terminal::TerminalPlan`]. `advance` passes the plan's
/// kind directly. Uses [`hook_kind`], not `plan(job).hooks`: the retry
/// suppression is `advance`'s business, and a caller with only a row wants the
/// status-only rule the pre-`Settlement` `fire_hooks` had.
pub async fn fire_hooks(
    s: &Settlement,
    workspace_config: &WorkspaceConfig,
    job: &stroem_db::JobRow,
    task: &TaskDef,
) {
    fire_hooks_of_kind(s, workspace_config, job, task, hook_kind(&job.status)).await
}

/// Fire hooks for a job that has reached a terminal state, selecting the hook
/// lists by the terminal plan's [`HookKind`] rather than by re-deriving them
/// from the job status.
///
/// - Jobs with `source_type = "hook"` never trigger further hooks (recursion guard).
/// - Task-level hooks take priority; workspace-level hooks fire as fallback for top-level jobs only.
/// - Each hook creates a new single-step job with `source_type = "hook"`.
/// - Failures are logged but never affect the original job.
#[tracing::instrument(
    skip(s, workspace_config, job, task),
    fields(job_id = %job.job_id, workspace = %job.workspace, task = %job.task_name)
)]
pub async fn fire_hooks_of_kind(
    s: &Settlement,
    workspace_config: &WorkspaceConfig,
    job: &stroem_db::JobRow,
    task: &TaskDef,
    kind: HookKind,
) {
    // Recursion guard: hook jobs never trigger further hooks
    if job.source_type == SourceType::Hook.as_ref() {
        return;
    }

    // Select task-level and workspace-level hooks for this event type
    let (task_hooks, ws_hooks) = match kind {
        HookKind::Success => (&task.on_success, &workspace_config.on_success),
        HookKind::Error => (&task.on_error, &workspace_config.on_error),
        HookKind::Cancel => (&task.on_cancel, &workspace_config.on_cancel),
        HookKind::None => return,
    };

    // Task hooks take priority. Workspace hooks are fallback for top-level jobs only.
    let is_top_level = is_top_level_source(&job.source_type);
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

    // Chain guard: bound indirect hook cycles that the recursion guard misses.
    if hook_chain_depth(&s.pool, job).await >= MAX_HOOK_CHAIN_DEPTH {
        tracing::warn!(
            job_id = %job.job_id,
            "hook chain depth limit ({}) reached — not firing hooks",
            MAX_HOOK_CHAIN_DEPTH
        );
        s.server_log(
            job.job_id,
            &format!(
                "[hooks] hook chain depth limit ({MAX_HOOK_CHAIN_DEPTH}) reached — \
                     not firing hooks for this job"
            ),
        )
        .await;
        return;
    }

    // Build hook context
    let ctx = match build_hook_context(&s.pool, job, task).await {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!(
                "Failed to build hook context for job {}: {:#}",
                job.job_id,
                e
            );
            s.server_log(
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
            s.server_log(
                job.job_id,
                &format!("[hooks] Failed to serialize hook context: {:#}", e),
            )
            .await;
            return;
        }
    };

    let hook_type = if job.status == JobStatus::Completed.as_ref() {
        "on_success"
    } else if job.status == JobStatus::Cancelled.as_ref() {
        "on_cancel"
    } else {
        "on_error"
    };

    let defaults = s.defaults;
    for (i, hook) in hooks.iter().enumerate() {
        if let Err(e) = fire_single_hook(
            s,
            workspace_config,
            &job.workspace,
            hook,
            &ctx_value,
            job.job_id,
            job.revision.as_deref(),
            job.git_ref.as_deref(),
            defaults,
        )
        .await
        {
            {
                let detail = scrub_hook_error(&format!("{e:#}"), workspace_config);
                tracing::error!(
                    "Failed to fire hook {}[{}] for job {}: {}",
                    hook_type,
                    i,
                    job.job_id,
                    detail
                );
                s.server_log(
                    job.job_id,
                    &format!(
                        "[hooks] Failed to fire hook {}[{}] for action '{}': {}",
                        hook_type, i, hook.action, detail
                    ),
                )
                .await;
            }
        }
    }
}

/// Fire `on_suspended` hooks when an approval step enters the `suspended` state.
///
/// - Recursion guard: jobs with `source_type = "hook"` never trigger further hooks.
/// - Task-level `on_suspended` hooks take priority; workspace-level fallback fires for top-level jobs.
/// - Each hook creates a new single-step job with `source_type = "hook"`.
/// - Failures are logged but never affect the original job or step.
#[tracing::instrument(
    skip(s, workspace_config, job, task, rendered_message),
    fields(job_id = %job.job_id, workspace = %job.workspace, task = %job.task_name)
)]
pub async fn fire_suspended_hooks(
    s: &Settlement,
    workspace_config: &WorkspaceConfig,
    job: &stroem_db::JobRow,
    task: &TaskDef,
    step_name: &str,
    rendered_message: &str,
) {
    // Recursion guard: hook jobs never trigger further hooks
    if job.source_type == SourceType::Hook.as_ref() {
        return;
    }

    // Select task-level then workspace-level on_suspended hooks
    let is_top_level = is_top_level_source(&job.source_type);
    let hooks: &[HookDef] = if !task.on_suspended.is_empty() {
        &task.on_suspended
    } else if is_top_level && !workspace_config.on_suspended.is_empty() {
        &workspace_config.on_suspended
    } else {
        return;
    };

    if hooks.is_empty() {
        return;
    }

    // Chain guard: bound indirect hook cycles that the recursion guard misses.
    if hook_chain_depth(&s.pool, job).await >= MAX_HOOK_CHAIN_DEPTH {
        tracing::warn!(
            job_id = %job.job_id,
            "hook chain depth limit ({}) reached — not firing on_suspended hooks",
            MAX_HOOK_CHAIN_DEPTH
        );
        s.server_log(
            job.job_id,
            &format!(
                "[hooks] hook chain depth limit ({MAX_HOOK_CHAIN_DEPTH}) reached — \
                     not firing hooks for this job"
            ),
        )
        .await;
        return;
    }

    let ctx = SuspendedHookContext {
        workspace: job.workspace.clone(),
        task_name: job.task_name.clone(),
        job_id: job.job_id.to_string(),
        step_name: step_name.to_string(),
        message: rendered_message.to_string(),
        source_type: job.source_type.clone(),
        source_id: job.source_id.clone(),
        revision: job.revision.clone(),
        git_ref: job.git_ref.clone(),
    };

    let ctx_value = match serde_json::to_value(&ctx) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to serialize suspended hook context: {:#}", e);
            s.server_log(
                job.job_id,
                &format!("[hooks] Failed to serialize on_suspended context: {:#}", e),
            )
            .await;
            return;
        }
    };

    let defaults = s.defaults;
    for (i, hook) in hooks.iter().enumerate() {
        if let Err(e) = fire_single_hook(
            s,
            workspace_config,
            &job.workspace,
            hook,
            &ctx_value,
            job.job_id,
            job.revision.as_deref(),
            job.git_ref.as_deref(),
            defaults,
        )
        .await
        {
            {
                let detail = scrub_hook_error(&format!("{e:#}"), workspace_config);
                tracing::error!(
                    "Failed to fire on_suspended hook[{}] for job {} step '{}': {}",
                    i,
                    job.job_id,
                    step_name,
                    detail
                );
                s.server_log(
                    job.job_id,
                    &format!(
                        "[hooks] Failed to fire on_suspended hook[{}] for action '{}': {}",
                        i, hook.action, detail
                    ),
                )
                .await;
            }
        }
    }
}

/// Fetch the artifact list for a job and project it into `HookArtifactMeta`.
///
/// Surfaces DB errors via `?` rather than swallowing them into an empty
/// `Vec` — a transient outage on this query should reach `fire_hooks`, which
/// logs and skips the hook, instead of silently rendering `hook.artifacts ==
/// []` (indistinguishable from a job that produced no artifacts).
async fn list_hook_artifacts(
    pool: &PgPool,
    job_id: uuid::Uuid,
) -> anyhow::Result<Vec<HookArtifactMeta>> {
    let rows = stroem_db::repos::job_artifact::JobArtifactRepo::new(pool.clone())
        .list_for_job(job_id)
        .await
        .context("Failed to list artifacts for hook context")?;

    Ok(rows
        .into_iter()
        .map(|r| HookArtifactMeta {
            url: format!(
                "/api/jobs/{}/artifacts/{}",
                job_id,
                url::form_urlencoded::byte_serialize(r.name.as_bytes()).collect::<String>()
            ),
            name: r.name,
            content_type: r.content_type,
            size_bytes: r.size_bytes,
            step_name: r.step_name,
            created_at: r.created_at,
        })
        .collect())
}

/// Pure: one `FailedStepInfo` per failed row, both flags read directly from
/// the failed row's own flow step (spec 2026-10-01 §6 — self-scoped only, no
/// downstream propagation; `hook.failed_steps` is explicitly unaffected by
/// that spec). A loop instance is judged by its placeholder
/// (`flow_step_name`).
///
/// A placeholder's own `Failed` row is excluded ONLY when at least one of
/// its instances is itself `Failed` — the instance(s) then represent the
/// failure (real per-instance error text, e.g. exit code/stderr) instead of
/// the placeholder's generic rolled-up message
/// (`cascade.rs::phase_rollup`'s R6 always rolls up `Failed` when any
/// instance failed, spec 2026-10-01 §4, regardless of the loop's own
/// `continue_on_failure`), which would otherwise double-report the same
/// underlying failure. A placeholder that fails with NO failed instance at
/// all (e.g. a `for_each` render error on a non-array value — instances
/// never existed) keeps its own row, since there's nothing else to
/// represent it. Split out from `build_hook_context` so this can be
/// unit-tested without a DB pool.
fn build_failed_steps(task: &TaskDef, steps: &[JobStepRow]) -> Vec<FailedStepInfo> {
    let placeholders_with_failed_instance: std::collections::HashSet<&str> = steps
        .iter()
        .filter(|s| s.status == StepStatus::Failed.as_ref())
        .filter_map(|s| s.loop_source.as_deref())
        .collect();

    steps
        .iter()
        .filter(|s| s.status == StepStatus::Failed.as_ref())
        .filter(|s| {
            s.loop_source.is_some()
                || !placeholders_with_failed_instance.contains(s.step_name.as_str())
        })
        .map(|s| {
            let flow_name =
                stroem_common::gate::flow_step_name(&s.step_name, s.loop_source.as_deref());
            let own_continue_on_failure = task
                .flow
                .get(flow_name)
                .is_some_and(|fs| fs.continue_on_failure);
            FailedStepInfo {
                step_name: s.step_name.clone(),
                action_name: s.action_name.clone(),
                error_message: s.error_message.clone(),
                continue_on_failure: own_continue_on_failure,
                tolerated: own_continue_on_failure,
                carried_over: s.carried_over,
            }
        })
        .collect()
}

async fn build_hook_context(
    pool: &PgPool,
    job: &stroem_db::JobRow,
    task: &TaskDef,
) -> anyhow::Result<HookContext> {
    let steps = JobStepRepo::get_steps_for_job(pool, job.job_id)
        .await
        .context("Failed to get steps for hook context")?;

    let artifacts = list_hook_artifacts(pool, job.job_id).await?;

    let failed_steps = build_failed_steps(task, &steps);

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
        artifacts,
        revision: job.revision.clone(),
        git_ref: job.git_ref.clone(),
    })
}

/// `source_job_id` is the job whose terminal state (or suspended step) fired
/// the hook. It is persisted as the hook job's `source_job_id` — the link
/// [`hook_chain_depth`] walks — and, as a string, its `source_id`.
///
/// `source_git_ref` + `revision` are the source job's pin: a hook job of a
/// pinned job runs the same commit (git-refs spec § 7.3), and
/// `workspace_config` is then that commit's config.
#[allow(clippy::too_many_arguments)]
async fn fire_single_hook(
    s: &Settlement,
    workspace_config: &WorkspaceConfig,
    workspace: &str,
    hook: &HookDef,
    ctx_value: &serde_json::Value,
    source_job_id: uuid::Uuid,
    revision: Option<&str>,
    source_git_ref: Option<&str>,
    defaults: crate::config::JobDefaults,
) -> anyhow::Result<()> {
    let workspaces = &s.workspaces;
    let pool = &s.pool;
    let source_id = source_job_id.to_string();
    // `ref` on hooks is out of v1 (git-refs spec § 4.6); serde would otherwise
    // drop it silently and run the default branch. The caller logs this to the
    // source job and fires nothing.
    if hook.git_ref.is_some() {
        anyhow::bail!(
            "`ref` is not supported on hooks yet (hook action '{}')",
            hook.action
        );
    }
    // A pin needs both halves: never persist a ref without its commit.
    if source_git_ref.is_some() && revision.is_none() {
        anyhow::bail!(
            "source job {} carries ref '{}' without its commit",
            source_job_id,
            source_git_ref.unwrap_or_default()
        );
    }
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
    if action.action_type == ActionType::Task.as_ref() {
        // action_type is a String field on ActionDef
        let task_ref = action
            .task
            .as_ref()
            .context("type: task action missing task field")?;

        // Same rule for a `type: task` hook action carrying `ref` (spec §
        // 4.6): this branch reads `action.task` directly, so the `ref` would
        // otherwise be dropped.
        if action.git_ref.is_some() {
            anyhow::bail!(
                "hook action '{}' is a `type: task` action with `ref`; `ref` is not supported on hooks yet",
                hook.action
            );
        }

        if let Some(msg) = foreign_hook_task_error(
            &hook.action,
            task_ref,
            workspace_config.tasks.contains_key(task_ref.as_str()),
        ) {
            anyhow::bail!("{msg}");
        }

        let created = crate::job_creator::create_job_for_task_inner(
            workspaces,
            pool,
            workspace_config,
            workspace,
            task_ref,
            rendered_input,
            "hook",
            Some(&source_id),
            None,
            None,
            revision,
            crate::job_creator::CreationMode::Hook { source_job_id },
            None, // agents_config not available in hook context; orchestrator dispatches agents
            defaults,
            source_git_ref,
        )
        .await
        .context("Failed to create hook task job")?;
        let job_id = created.job_id;

        tracing::info!(
            "Fired hook task job {} for action '{}' -> task '{}' (source: {})",
            job_id,
            hook.action,
            task_ref,
            source_id
        );

        Box::pin(s.job_created(created)).await;

        return Ok(());
    }

    // Create the hook job (single-step, always distributed)
    let task_name = format!("_hook:{}", hook.action);

    let flow_step = FlowStep {
        git_ref: None,
        action: hook.action.clone(),
        name: None,
        description: None,
        depends_on: vec![],
        input: std::collections::HashMap::new(),
        continue_on_failure: false,
        legacy_continue_when_skipped: None,
        timeout: None,
        when: None,
        for_each: None,
        sequential: false,
        retry: None,
        inline_action: None,
    };

    // A hook job of a pinned job runs the same commit (spec § 7.3).
    let pin_cols = source_git_ref.map(|r| stroem_db::JobPinCols {
        git_ref: r.to_string(),
        task_folder: None,
    });

    let mut tx = pool
        .begin()
        .await
        .context("Failed to begin hook job transaction")?;

    let job_id = JobRepo::create_with_parent_tx(
        &mut *tx,
        workspace,
        &task_name,
        "distributed",
        Some(rendered_input.clone()),
        "hook",
        Some(&source_id),
        None,
        None,
        None,
        revision,
        None, // raw_input: hook jobs don't persist raw_input (no re-run use case)
        Some(source_job_id),
        None,
        None, // max_retries: hook jobs have no task-level retry
        pin_cols.as_ref(),
    )
    .await
    .context("Failed to create hook job")?;

    let step = crate::job_creator::build_step(
        job_id,
        "hook",
        hook.action.clone(),
        &flow_step,
        action,
        Some(rendered_input),
        StepStatus::Ready,
        defaults,
        None,
        None,
        None,
        None,
    );

    JobStepRepo::create_steps_tx(&mut *tx, &[step])
        .await
        .context("Failed to create hook job step")?;

    tx.commit().await.context("Failed to commit hook job")?;

    tracing::info!(
        "Fired hook job {} for action '{}' (source: {})",
        job_id,
        hook.action,
        source_id
    );

    Box::pin(s.job_created(CreatedJob::new(job_id, false))).await;

    Ok(())
}

/// Hook actions are never resolved cross-workspace: a dotted `task:` that is not a
/// local (or library-flattened) key is refused before any job is created.
fn foreign_hook_task_error(
    hook_action: &str,
    task_ref: &str,
    is_local_task: bool,
) -> Option<String> {
    if is_local_task || !task_ref.contains('.') {
        return None;
    }
    let malformed = stroem_common::template::parse_qualified_ref(task_ref)
        .0
        .is_none();
    Some(if malformed {
        format!(
            "hook uses action '{hook_action}' whose task '{task_ref}' is malformed (empty workspace or task name)"
        )
    } else {
        format!(
            "hook uses action '{hook_action}' whose task '{task_ref}' is in another workspace; hook actions cannot call tasks across workspaces"
        )
    })
}

/// Defence in depth (spec § 3.4): template errors are value-free, but the
/// chain also carries our own contexts; scrub with the workspace's secrets.
fn scrub_hook_error(text: &str, cfg: &WorkspaceConfig) -> String {
    crate::workspace_set::redact_secrets_in_str(
        text,
        &crate::workspace_set::collect_config_secret_values(cfg),
    )
}

/// Select which hooks to fire for a job, applying the priority and fallback rules.
///
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn foreign_hook_task_error_distinguishes_foreign_from_malformed() {
        assert_eq!(foreign_hook_task_error("notify", "cleanup", false), None);
        assert_eq!(
            foreign_hook_task_error("notify", "common.cleanup", true),
            None,
            "a library-flattened local key is not foreign"
        );

        let foreign = foreign_hook_task_error("notify", "B.cleanup", false).unwrap();
        assert!(
            foreign.contains("is in another workspace")
                && foreign.contains("cannot call tasks across workspaces"),
            "{foreign}"
        );

        for bad in [".cleanup", "B."] {
            let msg = foreign_hook_task_error("notify", bad, false).unwrap();
            assert!(
                msg.contains("is malformed (empty workspace or task name)"),
                "{bad}: {msg}"
            );
            assert!(!msg.contains("another workspace"), "{bad}: {msg}");
        }
    }

    fn flow_step(deps: &[&str], continue_on_failure: bool) -> FlowStep {
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: deps
                .iter()
                .map(|s| stroem_common::depends_on::DependsOnEntry::Name(s.to_string()))
                .collect(),
            input: HashMap::new(),
            continue_on_failure,
            legacy_continue_when_skipped: None,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            git_ref: None,
            inline_action: None,
        }
    }

    fn task_with_flow(flow: Vec<(&str, FlowStep)>) -> TaskDef {
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: flow.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        }
    }

    fn step_row(name: &str, status: &str) -> stroem_db::JobStepRow {
        let mut r = stroem_db::JobStepRow::test_default(uuid::Uuid::nil(), name);
        r.status = status.to_string();
        r
    }

    #[test]
    fn tolerated_reflects_only_the_failed_step_s_own_flag() {
        // a(no flag) -> b(cof). a fails; b never runs (unreachable). a's
        // failed_steps entry must show tolerated: false AND
        // continue_on_failure: false, even though b would have caught it
        // under the old structural rule (spec 2026-10-01 §6).
        let task = task_with_flow(vec![
            ("a", flow_step(&[], false)),
            ("b", flow_step(&["a"], true)),
        ]);
        let steps = vec![step_row("a", "failed"), step_row("b", "skipped")];
        let failed_steps = build_failed_steps(&task, &steps);
        let a_entry = failed_steps.iter().find(|f| f.step_name == "a").unwrap();
        assert!(!a_entry.tolerated);
        assert!(!a_entry.continue_on_failure);
    }

    #[test]
    fn a_failed_loop_instance_is_reported_once_not_doubled_with_its_placeholder() {
        // A for_each placeholder's failure rolls up onto its own row
        // (cascade.rs::phase_rollup R6, spec §4 — always Failed when any
        // instance failed, cof'd or not) while the failed INSTANCE row also
        // stays `failed` in the DB. The INSTANCE represents the failure
        // (real error text); the placeholder's own generic rolled-up row is
        // dropped to avoid double-reporting the same underlying failure.
        let task = task_with_flow(vec![("loop", flow_step(&[], true))]);
        let mut instance = step_row("loop[0]", "failed");
        instance.loop_source = Some("loop".to_string());
        instance.error_message = Some("exit code 1".to_string());
        let mut placeholder = step_row("loop", "failed");
        placeholder.error_message = Some("for_each loop failed: instances [0] failed".to_string());
        let failed_steps = build_failed_steps(&task, &[instance, placeholder]);
        assert_eq!(failed_steps.len(), 1);
        assert_eq!(failed_steps[0].step_name, "loop[0]");
        assert_eq!(
            failed_steps[0].error_message.as_deref(),
            Some("exit code 1")
        );
        // The instance's flags resolve through its PLACEHOLDER's own flow
        // step (flow_step_name), not an instance-specific one (instances
        // aren't in `flow` at all) — this is the actual point of
        // `test_gate_flagged_loop_instance_failure_reports_placeholder_flag`.
        assert!(failed_steps[0].continue_on_failure);
        assert!(failed_steps[0].tolerated);
    }

    #[test]
    fn a_placeholder_with_no_failed_instances_keeps_its_own_row() {
        // A for_each render error (e.g. a non-array value) fails the
        // placeholder directly — no instances ever existed, so there's
        // nothing to represent it instead.
        let task = task_with_flow(vec![("loop", flow_step(&[], false))]);
        let placeholder = step_row("loop", "failed");
        let failed_steps = build_failed_steps(&task, &[placeholder]);
        assert_eq!(failed_steps.len(), 1);
        assert_eq!(failed_steps[0].step_name, "loop");
    }

    #[test]
    fn multiple_failed_instances_are_each_reported_and_the_placeholder_dropped() {
        let task = task_with_flow(vec![("loop", flow_step(&[], false))]);
        let mut i0 = step_row("loop[0]", "failed");
        i0.loop_source = Some("loop".to_string());
        let mut i2 = step_row("loop[2]", "failed");
        i2.loop_source = Some("loop".to_string());
        let placeholder = step_row("loop", "failed");
        let failed_steps = build_failed_steps(&task, &[i0, i2, placeholder]);
        let names: std::collections::HashSet<&str> =
            failed_steps.iter().map(|f| f.step_name.as_str()).collect();
        assert_eq!(
            names,
            std::collections::HashSet::from(["loop[0]", "loop[2]"])
        );
    }

    #[test]
    fn tolerated_is_true_when_the_failed_step_has_its_own_flag() {
        let task = task_with_flow(vec![("a", flow_step(&[], true))]);
        let steps = vec![step_row("a", "failed")];
        let failed_steps = build_failed_steps(&task, &steps);
        let a_entry = failed_steps.iter().find(|f| f.step_name == "a").unwrap();
        assert!(a_entry.tolerated);
        assert!(a_entry.continue_on_failure);
    }

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
            "failed" => (&task.on_error, &workspace_config.on_error),
            "cancelled" => (&task.on_cancel, &workspace_config.on_cancel),
            _ => return None,
        };

        let is_top_level = is_top_level_source(job_source_type);
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
                tolerated: false,
                carried_over: false,
            }],
            artifacts: vec![],
            revision: None,
            git_ref: None,
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
            artifacts: vec![],
            revision: None,
            git_ref: None,
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
    fn test_hook_revision_available_in_template() {
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
            artifacts: vec![],
            revision: Some("abc123def".to_string()),
            git_ref: None,
        };

        let ctx_value = serde_json::to_value(&ctx).unwrap();
        let template_context = json!({ "hook": ctx_value });

        let mut input = std::collections::HashMap::new();
        input.insert(
            "message".to_string(),
            json!("Deployed revision {{ hook.revision }}"),
        );

        let result = render_input_map(&input, &template_context).unwrap();
        assert_eq!(result["message"], "Deployed revision abc123def");
    }

    #[test]
    fn test_hook_ref_available_in_template() {
        let ctx = HookContext {
            workspace: "prod".to_string(),
            task_name: "deploy".to_string(),
            job_id: "abc-123".to_string(),
            status: "completed".to_string(),
            is_success: true,
            error_message: None,
            source_type: "trigger".to_string(),
            source_id: None,
            started_at: None,
            completed_at: None,
            duration_secs: None,
            failed_steps: vec![],
            artifacts: vec![],
            revision: Some("abc123def".to_string()),
            git_ref: Some("release/2.3".to_string()),
        };
        let ctx_value = serde_json::to_value(&ctx).unwrap();
        assert_eq!(ctx_value["ref"], "release/2.3", "serialised as `ref`");
        let template_context = json!({ "hook": ctx_value });
        let mut input = std::collections::HashMap::new();
        input.insert(
            "message".to_string(),
            json!("{{ hook.task_name }} ran {{ hook.ref }}"),
        );
        let result = render_input_map(&input, &template_context).unwrap();
        assert_eq!(result["message"], "deploy ran release/2.3");

        let suspended = SuspendedHookContext {
            workspace: "prod".to_string(),
            task_name: "deploy".to_string(),
            job_id: "abc-123".to_string(),
            step_name: "gate".to_string(),
            message: "ok?".to_string(),
            source_type: "trigger".to_string(),
            source_id: None,
            revision: None,
            git_ref: Some("v4.1.0".to_string()),
        };
        assert_eq!(serde_json::to_value(&suspended).unwrap()["ref"], "v4.1.0");
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
                tolerated: false,
                carried_over: false,
            }],
            artifacts: vec![],
            revision: None,
            git_ref: None,
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
            artifacts: vec![],
            revision: None,
            git_ref: None,
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
            artifacts: vec![],
            revision: None,
            git_ref: None,
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

    #[test]
    fn hook_artifacts_template_iteration() {
        use chrono::{TimeZone, Utc};
        let ctx = HookContext {
            workspace: "default".to_string(),
            task_name: "build".to_string(),
            job_id: "00000000-0000-0000-0000-000000000123".to_string(),
            status: "completed".to_string(),
            is_success: true,
            error_message: None,
            source_type: "api".to_string(),
            source_id: None,
            started_at: None,
            completed_at: None,
            duration_secs: Some(12.0),
            failed_steps: vec![],
            artifacts: vec![
                HookArtifactMeta {
                    name: "report.html".to_string(),
                    content_type: "text/html".to_string(),
                    size_bytes: 1024,
                    step_name: "build".to_string(),
                    created_at: Utc.with_ymd_and_hms(2026, 6, 1, 10, 0, 0).unwrap(),
                    url: "/api/jobs/00000000-0000-0000-0000-000000000123/artifacts/report.html"
                        .to_string(),
                },
                HookArtifactMeta {
                    name: "screenshot.png".to_string(),
                    content_type: "image/png".to_string(),
                    size_bytes: 4096,
                    step_name: "test".to_string(),
                    created_at: Utc.with_ymd_and_hms(2026, 6, 1, 10, 5, 0).unwrap(),
                    url: "/api/jobs/00000000-0000-0000-0000-000000000123/artifacts/screenshot.png"
                        .to_string(),
                },
            ],
            revision: None,
            git_ref: None,
        };

        let ctx_value = serde_json::to_value(&ctx).unwrap();
        let template_context = json!({ "hook": ctx_value });

        let mut input = std::collections::HashMap::new();
        input.insert(
            "text".to_string(),
            json!("{% for a in hook.artifacts %}{{ a.name }}={{ a.url }};{% endfor %}"),
        );

        let rendered = render_input_map(&input, &template_context).unwrap();
        let text = rendered["text"].as_str().unwrap();
        assert!(
            text.contains(
                "report.html=/api/jobs/00000000-0000-0000-0000-000000000123/artifacts/report.html"
            ),
            "missing report.html row: {text}"
        );
        assert!(
            text.contains("screenshot.png=/api/jobs/00000000-0000-0000-0000-000000000123/artifacts/screenshot.png"),
            "missing screenshot.png row: {text}"
        );
        assert_eq!(
            text.matches(';').count(),
            2,
            "expected exactly 2 rows: {text}"
        );
    }

    #[test]
    fn hook_artifacts_size_and_step_name_accessible() {
        use chrono::Utc;
        let ctx = HookContext {
            workspace: "default".to_string(),
            task_name: "build".to_string(),
            job_id: "00000000-0000-0000-0000-000000000999".to_string(),
            status: "completed".to_string(),
            is_success: true,
            error_message: None,
            source_type: "api".to_string(),
            source_id: None,
            started_at: None,
            completed_at: None,
            duration_secs: None,
            failed_steps: vec![],
            artifacts: vec![HookArtifactMeta {
                name: "dist.zip".to_string(),
                content_type: "application/zip".to_string(),
                size_bytes: 12345,
                step_name: "package".to_string(),
                created_at: Utc::now(),
                url: "/api/jobs/00000000-0000-0000-0000-000000000999/artifacts/dist.zip"
                    .to_string(),
            }],
            revision: None,
            git_ref: None,
        };

        let ctx_value = serde_json::to_value(&ctx).unwrap();
        let template_context = json!({ "hook": ctx_value });

        let mut input = std::collections::HashMap::new();
        input.insert(
            "text".to_string(),
            json!("{{ hook.artifacts[0].name }}|{{ hook.artifacts[0].size_bytes }}|{{ hook.artifacts[0].step_name }}"),
        );

        let rendered = render_input_map(&input, &template_context).unwrap();
        assert_eq!(rendered["text"], "dist.zip|12345|package");
    }

    // ─── Workspace-level hook fallback selection tests ───────────────────────

    fn make_hook(action: &str) -> HookDef {
        HookDef {
            git_ref: None,
            action: action.to_string(),
            input: HashMap::new(),
        }
    }

    fn make_task_def(
        on_success: Vec<HookDef>,
        on_error: Vec<HookDef>,
        on_cancel: Vec<HookDef>,
    ) -> TaskDef {
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success,
            on_error,
            on_suspended: vec![],
            on_cancel,
        }
    }

    fn make_workspace_config(
        on_success: Vec<HookDef>,
        on_error: Vec<HookDef>,
        on_cancel: Vec<HookDef>,
    ) -> WorkspaceConfig {
        let mut config = WorkspaceConfig::new();
        config.on_success = on_success;
        config.on_error = on_error;
        config.on_cancel = on_cancel;
        config
    }

    /// Task has its own on_success hooks — workspace on_success hooks must NOT be used.
    #[test]
    fn test_task_on_success_takes_priority_over_workspace() {
        let task = make_task_def(vec![make_hook("task-notify")], vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "completed", &task).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "task-notify");
    }

    /// Task has no on_success hooks — workspace on_success hooks are used as fallback
    /// for a top-level job (source_type = "api").
    #[test]
    fn test_workspace_on_success_fallback_when_task_has_none() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "completed", &task).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "ws-notify");
    }

    /// Task has its own on_error hooks — workspace on_error hooks must NOT be used.
    #[test]
    fn test_task_on_error_takes_priority_over_workspace() {
        let task = make_task_def(vec![], vec![make_hook("task-alert")], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "failed", &task).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "task-alert");
    }

    /// Task has no on_error hooks — workspace on_error hooks are used as fallback
    /// for a top-level job (source_type = "api").
    #[test]
    fn test_workspace_on_error_fallback_when_task_has_none() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "failed", &task).unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "ws-alert");
    }

    /// Task has on_success but no on_error — workspace fallback applies only for on_error.
    #[test]
    fn test_workspace_fallback_only_for_missing_hook_type() {
        let task = make_task_def(vec![make_hook("task-notify")], vec![], vec![]);
        let ws = make_workspace_config(
            vec![make_hook("ws-notify")],
            vec![make_hook("ws-alert")],
            vec![],
        );

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
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![], vec![]);

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
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![], vec![]);

        let selected = select_hooks_for_job(&ws, "hook", "completed", &task);
        assert!(
            selected.is_none(),
            "hook jobs must never trigger further hooks"
        );
    }

    /// Workspace fallback fires for trigger-sourced top-level jobs.
    #[test]
    fn test_workspace_fallback_fires_for_trigger_sourced_jobs() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![make_hook("ws-notify")], vec![], vec![]);

        let selected = select_hooks_for_job(&ws, "trigger", "completed", &task).unwrap();
        assert_eq!(selected[0].action, "ws-notify");
    }

    /// Workspace fallback fires for user-sourced (authenticated API) top-level jobs.
    #[test]
    fn test_workspace_fallback_fires_for_user_sourced_jobs() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")], vec![]);

        let selected = select_hooks_for_job(&ws, "user", "failed", &task).unwrap();
        assert_eq!(selected[0].action, "ws-alert");
    }

    /// Workspace fallback fires for webhook-sourced top-level jobs.
    #[test]
    fn test_workspace_fallback_fires_for_webhook_sourced_jobs() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")], vec![]);

        let selected = select_hooks_for_job(&ws, "webhook", "failed", &task).unwrap();
        assert_eq!(selected[0].action, "ws-alert");
    }

    /// Workspace fallback fires for rerun-sourced top-level jobs (a Re-run's
    /// failure must not be silently dropped just because it isn't `api`/`user`).
    #[test]
    fn test_workspace_fallback_fires_for_rerun_sourced_jobs() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")], vec![]);

        let selected = select_hooks_for_job(&ws, "rerun", "failed", &task).unwrap();
        assert_eq!(selected[0].action, "ws-alert");
    }

    /// Workspace fallback fires for restart-sourced top-level jobs.
    #[test]
    fn test_workspace_fallback_fires_for_restart_sourced_jobs() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![make_hook("ws-alert")], vec![]);

        let selected = select_hooks_for_job(&ws, "restart", "failed", &task).unwrap();
        assert_eq!(selected[0].action, "ws-alert");
    }

    /// No hooks at all — returns None rather than an empty slice.
    #[test]
    fn test_no_hooks_anywhere_returns_none() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![], vec![]);

        assert!(select_hooks_for_job(&ws, "api", "completed", &task).is_none());
        assert!(select_hooks_for_job(&ws, "api", "failed", &task).is_none());
    }

    /// Non-terminal statuses (e.g. "pending", "running") never produce hooks.
    #[test]
    fn test_non_terminal_status_returns_none() {
        let task = make_task_def(
            vec![make_hook("task-notify")],
            vec![make_hook("task-alert")],
            vec![],
        );
        let ws = make_workspace_config(
            vec![make_hook("ws-notify")],
            vec![make_hook("ws-alert")],
            vec![],
        );

        for status in &["pending", "running", "unknown"] {
            assert!(
                select_hooks_for_job(&ws, "api", status, &task).is_none(),
                "status '{status}' should not trigger hooks"
            );
        }
    }

    /// Cancelled jobs fire on_cancel hooks (not on_error).
    #[test]
    fn test_cancelled_fires_on_cancel_hooks() {
        let task = make_task_def(vec![], vec![], vec![make_hook("task-cancel-notify")]);
        let ws = make_workspace_config(vec![], vec![], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "cancelled", &task).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "task-cancel-notify");
    }

    /// Cancelled jobs do NOT fire on_error hooks (clean break).
    #[test]
    fn test_cancelled_does_not_fire_on_error_hooks() {
        let task = make_task_def(vec![], vec![make_hook("task-alert")], vec![]);
        let ws = make_workspace_config(vec![], vec![], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "cancelled", &task);
        assert!(
            selected.is_none(),
            "cancelled jobs must not fire on_error hooks"
        );
    }

    /// Cancelled job with no on_cancel hooks defined fires nothing.
    #[test]
    fn test_cancelled_with_no_on_cancel_fires_nothing() {
        let task = make_task_def(
            vec![make_hook("task-notify")],
            vec![make_hook("task-alert")],
            vec![],
        );
        let ws = make_workspace_config(vec![], vec![], vec![]);

        let selected = select_hooks_for_job(&ws, "api", "cancelled", &task);
        assert!(
            selected.is_none(),
            "cancelled jobs with no on_cancel must not fire any hooks"
        );
    }

    /// Task-level on_cancel takes priority over workspace-level on_cancel.
    #[test]
    fn test_task_on_cancel_takes_priority_over_workspace() {
        let task = make_task_def(vec![], vec![], vec![make_hook("task-cancel")]);
        let ws = make_workspace_config(vec![], vec![], vec![make_hook("ws-cancel")]);

        let selected = select_hooks_for_job(&ws, "api", "cancelled", &task).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "task-cancel");
    }

    /// Workspace on_cancel fallback fires for top-level cancelled jobs.
    #[test]
    fn test_workspace_on_cancel_fallback() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(vec![], vec![], vec![make_hook("ws-cancel")]);

        let selected = select_hooks_for_job(&ws, "api", "cancelled", &task).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].action, "ws-cancel");
    }

    /// Multiple workspace fallback hooks are all returned.
    #[test]
    fn test_workspace_fallback_returns_all_hooks_when_multiple() {
        let task = make_task_def(vec![], vec![], vec![]);
        let ws = make_workspace_config(
            vec![make_hook("ws-slack"), make_hook("ws-pagerduty")],
            vec![],
            vec![],
        );

        let selected = select_hooks_for_job(&ws, "api", "completed", &task).unwrap();
        assert_eq!(selected.len(), 2);
        let actions: Vec<&str> = selected.iter().map(|h| h.action.as_str()).collect();
        assert!(actions.contains(&"ws-slack"));
        assert!(actions.contains(&"ws-pagerduty"));
    }

    /// Skipped jobs never fire hooks (not completed/failed/cancelled).
    #[test]
    fn test_skipped_status_does_not_fire_hooks() {
        let task = make_task_def(
            vec![make_hook("task-notify")],
            vec![make_hook("task-alert")],
            vec![],
        );
        let ws = make_workspace_config(
            vec![make_hook("ws-notify")],
            vec![make_hook("ws-alert")],
            vec![],
        );

        let selected = select_hooks_for_job(&ws, "trigger", "skipped", &task);
        assert!(selected.is_none(), "skipped jobs should never fire hooks");
    }

    // ─── SuspendedHookContext serialization tests ─────────────────────────────

    #[test]
    fn test_suspended_hook_context_serialization() {
        let ctx = SuspendedHookContext {
            workspace: "prod".to_string(),
            task_name: "deploy".to_string(),
            job_id: "abc-123".to_string(),
            step_name: "approve-deploy".to_string(),
            message: "Please approve the deployment to production".to_string(),
            source_type: "api".to_string(),
            source_id: Some("user@example.com".to_string()),
            revision: None,
            git_ref: None,
        };

        let value = serde_json::to_value(&ctx).unwrap();
        assert_eq!(value["workspace"], "prod");
        assert_eq!(value["task_name"], "deploy");
        assert_eq!(value["job_id"], "abc-123");
        assert_eq!(value["step_name"], "approve-deploy");
        assert_eq!(
            value["message"],
            "Please approve the deployment to production"
        );
        assert_eq!(value["source_type"], "api");
        assert_eq!(value["source_id"], "user@example.com");
    }

    #[test]
    fn test_suspended_hook_context_source_id_optional() {
        let ctx = SuspendedHookContext {
            workspace: "dev".to_string(),
            task_name: "test".to_string(),
            job_id: "xyz-789".to_string(),
            step_name: "gate".to_string(),
            message: String::new(),
            source_type: "trigger".to_string(),
            source_id: None,
            revision: None,
            git_ref: None,
        };

        let value = serde_json::to_value(&ctx).unwrap();
        assert!(value["source_id"].is_null());
    }

    #[test]
    fn test_suspended_hook_revision_available_in_template() {
        let ctx = SuspendedHookContext {
            workspace: "prod".to_string(),
            task_name: "deploy".to_string(),
            job_id: "abc-123".to_string(),
            step_name: "gate".to_string(),
            message: "Approve?".to_string(),
            source_type: "api".to_string(),
            source_id: None,
            revision: Some("abc123def".to_string()),
            git_ref: None,
        };

        let ctx_value = serde_json::to_value(&ctx).unwrap();
        let template_context = json!({ "hook": ctx_value });

        let mut input = std::collections::HashMap::new();
        input.insert(
            "message".to_string(),
            json!("Awaiting approval at revision {{ hook.revision }}"),
        );

        let result = render_input_map(&input, &template_context).unwrap();
        assert_eq!(result["message"], "Awaiting approval at revision abc123def");
    }

    /// Regression test for F8: a DB failure while listing artifacts must
    /// propagate out of the artifact-fetch helper instead of being swallowed
    /// into an empty `Vec<HookArtifactMeta>`.
    ///
    /// Before the fix, the artifact-list call in `build_hook_context` used
    /// `.unwrap_or_default()`, which meant a transient Postgres outage would
    /// render hook templates with `hook.artifacts == []` — indistinguishable
    /// from a job that genuinely produced no artifacts. The fix replaces
    /// that with `?` (extracted into [`list_hook_artifacts`]), so
    /// `fire_hooks` can log the real cause and skip the hook instead of
    /// firing one on a silently corrupted context.
    ///
    /// The test targets [`list_hook_artifacts`] directly to isolate the
    /// artifact path — the broader `build_hook_context` also has an earlier
    /// `?` on `get_steps_for_job`, so a failing pool would Err there
    /// regardless of the artifact-list behaviour.
    #[tokio::test]
    async fn list_hook_artifacts_propagates_db_error() {
        use uuid::Uuid;

        // Lazy pool to an unreachable host — actual queries fail with a
        // connection error, which is exactly the transient-DB-blip we want
        // to test against.
        let pool = sqlx::PgPool::connect_lazy("postgres://invalid:5432/db").unwrap();

        let result = list_hook_artifacts(&pool, Uuid::new_v4()).await;
        assert!(
            result.is_err(),
            "list_hook_artifacts must surface DB errors as Err rather than yielding an empty list — \
             otherwise hook templates would render `hook.artifacts == []` on a transient outage \
             (indistinguishable from a job that legitimately produced no artifacts)"
        );
    }

    // ─── hook_chain_depth hop-budget regression (B1, carried from Plan A review) ──

    /// `hook_chain_depth` must budget its ancestry walk in *hook links*, not
    /// raw hops. Up to [`crate::job_creator::MAX_TASK_DEPTH`] (10) plain
    /// `type: task` levels can sit between two `hook` links: a hook whose
    /// action is `type: task` creates a job with `source_type = "hook"` and
    /// `parent_job_id = None` (`fire_single_hook` never threads a parent
    /// through), which resets the `type: task` nesting counter — so
    /// `MAX_TASK_DEPTH` alone does not bound how many task levels can appear
    /// between two hook links.
    ///
    /// This builds a synthetic ancestry with 3 hook links
    /// (`MAX_HOOK_CHAIN_DEPTH`), each separated by 9 plain `type: task`
    /// levels — one under the `MAX_TASK_DEPTH` cap, so the chain itself is
    /// legal. That costs 3 * (9 + 1) = 30 raw ancestry hops, which fits the
    /// fixed budget (`MAX_HOOK_CHAIN_DEPTH * (MAX_TASK_DEPTH + 1) + 1 = 34`)
    /// but blows through the old fixed 20-hop cap — under which
    /// `hook_chain_depth` would under-count this chain to a constant depth of
    /// 1 forever (`floor(20 / 10)`), no matter how deep the true chain grew,
    /// leaving the cycle this guards against unbounded.
    ///
    /// Talks to a real Postgres so the walk exercises the actual
    /// `JobRepo::get` ancestry lookups, not a mock. No workspace or task
    /// config is needed — `hook_chain_depth` only reads `source_type`,
    /// `source_job_id`, `source_id` and `parent_job_id`, so the synthetic rows below never
    /// have to resolve to a real flow.
    #[tokio::test(flavor = "multi_thread")]
    async fn hook_chain_depth_counts_hook_links_across_intermediate_task_levels() {
        const INTERMEDIATE_LEVELS: usize = 9;
        const HOOK_LINKS: usize = 3;

        let pool = stroem_test_support::test_pool().await;

        // Genesis: an ordinary top-level job whose (synthetic) "on_success"
        // fires the first hook link below.
        let mut current = JobRepo::create(
            &pool,
            "default",
            "genesis",
            "distributed",
            None,
            "api",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        for _ in 0..HOOK_LINKS {
            // The hook link itself: `source_type = "hook"`, `source_id` and
            // `source_job_id` = the job that fired it, `parent_job_id = None`
            // — exactly what `fire_single_hook`'s `type: task` branch produces.
            let mut node = JobRepo::create_with_parent(
                &pool,
                "default",
                "hook-target",
                "distributed",
                None,
                "hook",
                Some(&current.to_string()),
                None,
                None,
                None,
                None,
                None,
                Some(current),
                None,
            )
            .await
            .unwrap();

            // INTERMEDIATE_LEVELS plain `type: task` levels, each an
            // ordinary parent-chained child (`source_type = "task"`).
            for _ in 0..INTERMEDIATE_LEVELS {
                node = JobRepo::create_with_parent(
                    &pool,
                    "default",
                    "hook-target",
                    "distributed",
                    None,
                    "task",
                    None,
                    Some(node),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            }

            current = node;
        }

        let final_job = JobRepo::get(&pool, current).await.unwrap().unwrap();
        let depth = hook_chain_depth(&pool, &final_job).await;
        assert!(
            depth >= MAX_HOOK_CHAIN_DEPTH,
            "expected the walk to see all {HOOK_LINKS} hook links (>= {MAX_HOOK_CHAIN_DEPTH}), \
             got {depth} — a chain with {INTERMEDIATE_LEVELS} intermediate task levels per hook \
             link must not defeat the hop budget"
        );
    }

    /// A hook job's ancestry continues from `source_job_id`, not from the
    /// string in `source_id`. The two point at different jobs here: the walk
    /// must reach the hook link behind `source_job_id` (depth 2), not stop at
    /// the plain job `source_id` names (depth 1).
    #[tokio::test(flavor = "multi_thread")]
    async fn hook_chain_depth_follows_source_job_id_not_source_id() {
        let pool = stroem_test_support::test_pool().await;

        let create = |source_type: &'static str,
                      source_id: Option<String>,
                      source_job_id: Option<uuid::Uuid>| {
            let pool = pool.clone();
            async move {
                JobRepo::create_with_parent(
                    &pool,
                    "default",
                    "t",
                    "distributed",
                    None,
                    source_type,
                    source_id.as_deref(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    source_job_id,
                    None,
                )
                .await
                .unwrap()
            }
        };

        let genesis = create("api", None, None).await;
        let first_hook = create("hook", Some(genesis.to_string()), Some(genesis)).await;
        let unrelated = create("api", None, None).await;
        let second_hook = create("hook", Some(unrelated.to_string()), Some(first_hook)).await;

        let job = JobRepo::get(&pool, second_hook).await.unwrap().unwrap();
        assert_eq!(hook_chain_depth(&pool, &job).await, 2);
    }

    /// During a rolling deploy a pre-048 server writes hook rows with the
    /// firing job only in `source_id`. The walk must continue through such a
    /// row instead of ending there, or a chain crossing the deploy counts only
    /// the links after it and overruns `MAX_HOOK_CHAIN_DEPTH`. Chain here:
    /// typed hook -> legacy hook (NULL `source_job_id`) -> typed hook = 3.
    #[tokio::test(flavor = "multi_thread")]
    async fn hook_chain_depth_continues_through_pre_048_hook_rows() {
        let pool = stroem_test_support::test_pool().await;

        let create = |source_type: &'static str,
                      source_id: Option<String>,
                      source_job_id: Option<uuid::Uuid>| {
            let pool = pool.clone();
            async move {
                JobRepo::create_with_parent(
                    &pool,
                    "default",
                    "t",
                    "distributed",
                    None,
                    source_type,
                    source_id.as_deref(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    source_job_id,
                    None,
                )
                .await
                .unwrap()
            }
        };

        let genesis = create("api", None, None).await;
        let first = create("hook", Some(genesis.to_string()), Some(genesis)).await;
        let legacy = create("hook", Some(format!("{first}/notify")), None).await;
        let last = create("hook", Some(legacy.to_string()), Some(legacy)).await;

        let job = JobRepo::get(&pool, last).await.unwrap().unwrap();
        assert_eq!(hook_chain_depth(&pool, &job).await, 3);
    }

    // ─── Error scrubbing tests (defence in depth: Tera 1→2 upgrade) ──────────

    #[test]
    fn hook_error_text_is_scrubbed_with_workspace_secrets() {
        let mut cfg = stroem_common::models::workflow::WorkspaceConfig::new();
        cfg.secrets
            .insert("T".into(), serde_json::json!("hook-raw-canary"));
        let text = super::scrub_hook_error("failed: hook-raw-canary", &cfg);
        assert!(!text.contains("hook-raw-canary"), "{text}");
    }

    /// Spec 2026-10-06 § 3.4 (Codex review, Low): a REAL hook-input render
    /// failure, driven through `fire_single_hook`'s error path for both
    /// callers (`fire_hooks_of_kind` → `on_error`, `fire_suspended_hooks` →
    /// `on_suspended`), lands in the SOURCE job's `_server` log without the
    /// secret or its upper-cased form — Tera's raw text carries the latter
    /// (asserted first), so the absence is not vacuous.
    #[tokio::test(flavor = "multi_thread")]
    async fn failing_hook_input_render_logs_a_value_free_line_to_the_source_job() {
        const SECRET: &str = "hook-input-canary";
        const TPL: &str = "{{ secret.X | upper | int }}";
        let upper = SECRET.to_uppercase();
        assert!(
            crate::test_support::tera_raw_detail_contains(
                TPL,
                &json!({"secret": {"X": SECRET}}),
                &upper
            ),
            "fixture must leak through Tera's raw text, else this test is vacuous"
        );

        let cfg: WorkspaceConfig = serde_yaml::from_str(
            r#"
secrets:
  X: hook-input-canary
actions:
  notify:
    type: script
    script: echo hi
"#,
        )
        .unwrap();
        let hook = HookDef {
            git_ref: None,
            action: "notify".to_string(),
            input: HashMap::from([("msg".to_string(), json!(TPL))]),
        };
        let mut task = make_task_def(vec![], vec![hook.clone()], vec![]);
        task.on_suspended = vec![hook];

        let temp_dir = tempfile::TempDir::new().unwrap();
        let pool = stroem_test_support::test_pool().await;
        let mgr = crate::workspace::WorkspaceManager::from_config("default", cfg.clone());
        let app_state = crate::state::test_app_state_with_pool(pool, mgr, temp_dir.path());
        let s = app_state.settlement();

        let mut job = stroem_db::JobRow::test_default();
        job.status = "failed".to_string();
        fire_hooks_of_kind(&s, &cfg, &job, &task, HookKind::Error).await;
        fire_suspended_hooks(&s, &cfg, &job, &task, "approve", "message").await;

        let log = std::fs::read_to_string(temp_dir.path().join(format!("{}.jsonl", job.job_id)))
            .expect("the source job has a log");
        let lines: Vec<String> = log
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|l| l["step"] == "_server")
            .map(|l| l["line"].as_str().unwrap().to_string())
            .collect();
        for prefix in [
            "[hooks] Failed to fire hook on_error[0] for action 'notify': ",
            "[hooks] Failed to fire on_suspended hook[0] for action 'notify': ",
        ] {
            let line = lines
                .iter()
                .find(|l| l.starts_with(prefix))
                .unwrap_or_else(|| panic!("no `{prefix}` line in {lines:?}"));
            assert!(
                line.contains("Failed to render hook input templates"),
                "{line}"
            );
            assert!(line.contains("filter `int` failed"), "{line}");
            assert!(!line.contains(SECRET), "{line}");
            assert!(!line.contains(&upper), "{line}");
        }
        assert!(!log.contains(SECRET) && !log.contains(&upper), "{log}");
    }

    // ─── H2 regression: instrument spans must not Debug-print the JobRow ─────

    /// A `tracing_subscriber::Layer` that records every field on every new
    /// span as a `"name=value"` string, so a test can assert none of them
    /// contain a sentinel that should have been kept out of the span.
    struct FieldCapture {
        fields: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl<S> tracing_subscriber::layer::Layer<S> for FieldCapture
    where
        S: tracing::Subscriber,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor<'a>(&'a mut Vec<String>);
            impl tracing::field::Visit for Visitor<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push(format!("{}={:?}", field.name(), value));
                }
            }
            let mut fields = self.fields.lock().unwrap();
            attrs.record(&mut Visitor(&mut fields));
        }
    }

    const SENTINEL: &str = "SENTINEL-DO-NOT-LOG";

    /// Regression for H2: `fire_hooks_of_kind`'s and `fire_suspended_hooks`'s
    /// `#[tracing::instrument]` spans used to omit `job: &JobRow` from
    /// `skip(...)`. `JobRow` derives plain `Debug`, which prints `input` /
    /// `output` / `raw_input` unredacted — for a cross-workspace `type: task`
    /// step those can carry the owner's resolved connection values and
    /// secrets, so the old span recorded them into server logs at info
    /// level. Both spans now `skip` the row and record only
    /// `job_id`/`workspace`/`task` as explicit fields; this installs a test
    /// subscriber that captures every span field verbatim and asserts a
    /// sentinel placed in `job.input` never appears in any of them.
    #[tokio::test(flavor = "current_thread")]
    async fn hook_spans_do_not_record_job_row_debug() {
        use tracing_subscriber::layer::SubscriberExt;

        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let layer = FieldCapture {
            fields: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let temp_dir = tempfile::TempDir::new().unwrap();
        let mgr =
            crate::workspace::WorkspaceManager::from_config("default", WorkspaceConfig::new());
        let app_state = crate::state::test_app_state_with_workspaces(mgr, temp_dir.path());
        let s = app_state.settlement();

        let mut job = stroem_db::JobRow::test_default();
        job.input = Some(json!({"field": SENTINEL}));
        job.output = Some(json!({"field": SENTINEL}));
        job.raw_input = Some(json!({"field": SENTINEL}));

        let ws_config = WorkspaceConfig::new();
        // No task-level or workspace-level hooks configured for any kind, so
        // both functions return right after the span is entered — before
        // touching `s.pool` (which is a never-connected lazy pool here).
        let task = make_task_def(vec![], vec![], vec![]);

        fire_hooks_of_kind(&s, &ws_config, &job, &task, HookKind::Success).await;
        fire_suspended_hooks(&s, &ws_config, &job, &task, "approve", "message").await;

        let fields = captured.lock().unwrap();
        assert!(
            !fields.is_empty(),
            "expected at least one span to have been recorded"
        );
        for f in fields.iter() {
            assert!(
                !f.contains(SENTINEL),
                "sentinel leaked into an instrument span field: {f}"
            );
        }
    }
}
