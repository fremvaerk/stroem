use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
use crate::config::JobDefaults;
use crate::job_creator::{create_job_for_task_detailed, create_job_for_task_pinned, CreationMode};
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::api::triggers::TriggerInfo;
use crate::web::api::{classify_execute_error, get_workspace_or_error};
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use stroem_common::template::PRIMITIVE_TYPES;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct TaskListItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub mode: String,
    pub workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub has_triggers: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_execute: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct TaskDetail {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub input: HashMap<String, serde_json::Value>,
    pub flow: HashMap<String, serde_json::Value>,
    pub triggers: Vec<TriggerInfo>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub connections: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_execute: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct RecentDuration {
    pub job_id: String,
    pub duration_ms: f64,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct TaskDurationStats {
    pub sample_size: i64,
    pub avg_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub max_ms: Option<f64>,
    /// Newest-first list of recent run durations (used for the sparkline).
    pub recent: Vec<RecentDuration>,
}

#[derive(Debug, Serialize)]
pub struct StepDurationStats {
    pub step_name: String,
    pub sample_size: i64,
    pub avg_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct TaskStatsResponse {
    /// Window the stats were computed over (the request's `limit`, clamped).
    pub window: i64,
    pub task: TaskDurationStats,
    pub steps: Vec<StepDurationStats>,
}

#[derive(Debug, Deserialize)]
pub struct TaskStatsQuery {
    /// Number of most-recent completed runs to aggregate over. Clamped to [1, 500].
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ExecuteTaskRequest {
    #[serde(default)]
    pub input: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub source_job_id: Option<Uuid>,
    /// Fields whose value is replayed from the source job's stored
    /// `raw_input` (spec 2026-10-06-json-input-type D12). Requires
    /// `source_job_id`.
    #[serde(default)]
    pub replay_fields: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ExecuteTaskResponse {
    pub job_id: String,
}

/// GET /api/tasks - List all tasks from all workspaces
#[tracing::instrument(skip(state, auth_user))]
pub async fn list_all_tasks(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
) -> Result<impl IntoResponse, AppError> {
    // Resolve ACL context once if auth is present and ACL is configured.
    let acl_ctx = if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = auth.user_id()?;
            let ctx = load_user_acl_context(&state.pool, user_id, auth.is_admin())
                .await
                .context("load ACL context")?;
            Some(ctx)
        } else {
            None
        }
    } else {
        None
    };

    let mut tasks = Vec::new();
    for (ws_name, workspace) in state.workspaces.get_all_configs().await {
        for (name, task) in &workspace.tasks {
            let can_execute = if let (Some(ref auth), Some((is_admin, ref groups))) =
                (&auth_user, &acl_ctx)
            {
                let task_path = make_task_path(task.folder.as_deref(), name);
                let perm =
                    state
                        .acl
                        .evaluate(&ws_name, &task_path, &auth.claims.email, groups, *is_admin);
                match perm {
                    TaskPermission::Deny => continue,
                    TaskPermission::View => Some(false),
                    TaskPermission::Run => Some(true),
                }
            } else {
                None
            };

            let has_triggers = workspace
                .triggers
                .values()
                .any(|t| t.enabled() && t.task() == *name);
            tasks.push(TaskListItem {
                id: name.clone(),
                name: task.name.clone(),
                description: task.description.clone(),
                mode: task.mode.clone(),
                workspace: ws_name.clone(),
                folder: task.folder.clone(),
                has_triggers,
                can_execute,
            });
        }
    }
    Ok(Json(tasks))
}

/// GET /api/workspaces/:ws/tasks - List all tasks from a workspace
#[tracing::instrument(skip(state, auth_user))]
pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(ws): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let workspace = get_workspace_or_error(&state, &ws).await?;

    // Resolve ACL context once if auth is present and ACL is configured.
    let acl_ctx = if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = auth.user_id()?;
            let ctx = load_user_acl_context(&state.pool, user_id, auth.is_admin())
                .await
                .context("load ACL context")?;
            Some(ctx)
        } else {
            None
        }
    } else {
        None
    };

    let mut tasks = Vec::new();
    for (name, task) in &workspace.tasks {
        let can_execute = if let (Some(ref auth), Some((is_admin, ref groups))) =
            (&auth_user, &acl_ctx)
        {
            let task_path = make_task_path(task.folder.as_deref(), name);
            let perm = state
                .acl
                .evaluate(&ws, &task_path, &auth.claims.email, groups, *is_admin);
            match perm {
                TaskPermission::Deny => continue,
                TaskPermission::View => Some(false),
                TaskPermission::Run => Some(true),
            }
        } else {
            None
        };

        let has_triggers = workspace
            .triggers
            .values()
            .any(|t| t.enabled() && t.task() == *name);
        tasks.push(TaskListItem {
            id: name.clone(),
            name: task.name.clone(),
            description: task.description.clone(),
            mode: task.mode.clone(),
            workspace: ws.clone(),
            folder: task.folder.clone(),
            has_triggers,
            can_execute,
        });
    }

    Ok(Json(tasks))
}

/// GET /api/workspaces/:ws/tasks/:name - Get task detail with action info
#[tracing::instrument(skip(state, auth_user))]
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((ws, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let workspace = get_workspace_or_error(&state, &ws).await?;

    let task = workspace
        .tasks
        .get(&name)
        .ok_or_else(|| AppError::not_found("Task"))?;

    // ACL check: Deny -> 404 (task not found), View -> can_execute=false, Run -> can_execute=true
    let can_execute = if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = auth.user_id()?;
            let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
                .await
                .context("load ACL context")?;
            let task_path = make_task_path(task.folder.as_deref(), &name);
            let perm = state
                .acl
                .evaluate(&ws, &task_path, &auth.claims.email, &groups, is_admin);
            match perm {
                TaskPermission::Deny => return Err(AppError::not_found("Task")),
                TaskPermission::View => Some(false),
                TaskPermission::Run => Some(true),
            }
        } else {
            None
        }
    } else {
        None
    };

    let triggers: Vec<TriggerInfo> = workspace
        .triggers
        .iter()
        .filter(|(_, t)| t.task() == name)
        .map(|(trig_name, trigger)| TriggerInfo::from_def(trig_name, trigger, 5))
        .collect();

    // Build connections map keyed by each input's `type:` AS WRITTEN (the UI
    // looks the list up by the field's own type string). Candidates are every
    // connection in any loaded workspace whose canonical type equals the
    // input's canonical type; foreign ones only when `shared`.
    let ws_set =
        crate::workspace_set::WorkspaceSet::load(&state.workspaces, &ws, Some(workspace.as_ref()))
            .await;
    let mut connections: HashMap<String, Vec<String>> = HashMap::new();
    let connection_types_needed: BTreeSet<&str> = task
        .input
        .values()
        .map(|f| f.field_type.as_str())
        .filter(|t| !PRIMITIVE_TYPES.contains(t))
        .collect();

    for type_as_written in connection_types_needed {
        let Ok(field_ct) =
            stroem_common::template::canonical_type_ref(type_as_written, &ws, &ws_set)
        else {
            continue; // unresolvable type: no dropdown, job creation reports the error
        };
        let mut local: Vec<String> = Vec::new();
        let mut foreign: Vec<String> = Vec::new();
        for (ws_name, cfg) in ws_set.iter_configs() {
            let is_local = ws_name == ws;
            for (conn_name, conn) in &cfg.connections {
                let Some(ref declared) = conn.connection_type else {
                    continue;
                };
                let Ok(conn_ct) =
                    stroem_common::template::canonical_type_ref(declared, ws_name, &ws_set)
                else {
                    continue;
                };
                if conn_ct != field_ct {
                    continue;
                }
                if is_local {
                    local.push(conn_name.clone());
                } else if conn.shared {
                    foreign.push(format!("{}.{}", ws_name, conn_name));
                }
            }
        }
        local.sort();
        foreign.sort();
        local.extend(foreign);
        if !local.is_empty() {
            connections.insert(type_as_written.to_string(), local);
        }
    }

    let detail = TaskDetail {
        id: name.clone(),
        name: task.name.clone(),
        description: task.description.clone(),
        mode: task.mode.clone(),
        folder: task.folder.clone(),
        input: task
            .input
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or_default()))
            .collect(),
        flow: task
            .flow
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or_default()))
            .collect(),
        triggers,
        connections,
        can_execute,
    };

    Ok(Json(detail))
}

/// GET /api/workspaces/:ws/tasks/:name/stats - Duration percentiles over recent completed runs
#[tracing::instrument(skip(state, auth_user))]
pub async fn get_task_stats(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((ws, name)): Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<TaskStatsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let workspace = get_workspace_or_error(&state, &ws).await?;

    let task = workspace
        .tasks
        .get(&name)
        .ok_or_else(|| AppError::not_found("Task"))?;

    // ACL: stats are read-only — View permission is sufficient. Deny -> 404.
    if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = auth.user_id()?;
            let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
                .await
                .context("load ACL context")?;
            let task_path = make_task_path(task.folder.as_deref(), &name);
            let perm = state
                .acl
                .evaluate(&ws, &task_path, &auth.claims.email, &groups, is_admin);
            if matches!(perm, TaskPermission::Deny) {
                return Err(AppError::not_found("Task"));
            }
        }
    }

    let limit = query.limit.unwrap_or(50).clamp(1, 500);

    let (task_stats, recent_rows, step_stats) = tokio::try_join!(
        stroem_db::JobRepo::get_task_duration_stats(&state.pool, &ws, &name, limit),
        stroem_db::JobRepo::get_recent_durations(&state.pool, &ws, &name, limit),
        stroem_db::JobStepRepo::get_step_duration_stats_for_task(&state.pool, &ws, &name, limit),
    )
    .with_context(|| format!("get_task_stats {ws}/{name}"))?;

    let recent = recent_rows
        .into_iter()
        .map(|r| RecentDuration {
            job_id: r.job_id.to_string(),
            duration_ms: r.duration_ms,
            completed_at: r.completed_at,
        })
        .collect();

    let response = TaskStatsResponse {
        window: limit,
        task: TaskDurationStats {
            sample_size: task_stats.sample_size,
            avg_ms: task_stats.avg_ms,
            p50_ms: task_stats.p50_ms,
            p95_ms: task_stats.p95_ms,
            min_ms: task_stats.min_ms,
            max_ms: task_stats.max_ms,
            recent,
        },
        steps: step_stats
            .into_iter()
            .map(|s| StepDurationStats {
                step_name: s.step_name,
                sample_size: s.sample_size,
                avg_ms: s.avg_ms,
                p50_ms: s.p50_ms,
                p95_ms: s.p95_ms,
                min_ms: s.min_ms,
                max_ms: s.max_ms,
            })
            .collect(),
    };

    Ok(Json(response))
}

/// POST /api/workspaces/:ws/tasks/:name/execute - Trigger task execution
#[tracing::instrument(skip(state, auth_user, req))]
pub async fn execute_task(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((ws, name)): Path<(String, String)>,
    Json(req): Json<ExecuteTaskRequest>,
) -> Result<impl IntoResponse, AppError> {
    // 1. Enforce auth: when auth is enabled, require a valid token
    let (source_type, source_id) = match (state.config.auth.is_some(), &auth_user) {
        (false, _) => ("api", None),
        (true, Some(user)) => ("user", Some(user.claims.email.clone())),
        (true, None) => {
            return Err(AppError::Unauthorized("Authentication required".into()));
        }
    };

    if !req.replay_fields.is_empty() && req.source_job_id.is_none() {
        return Err(AppError::BadRequest(
            "replay_fields requires source_job_id".into(),
        ));
    }

    // 1b. Re-run source: its job row is read once. The existence + ACL check
    //     below runs FIRST, before the pinned/unpinned branch and before any
    //     destination task lookup; a missing source and a denied one answer
    //     the same 404 `Source job not found` (spec 2026-10-06-json-input-type,
    //     revision 10). A PINNED source then takes its own path, since its
    //     task may exist only at the source's ref.
    let source_row = match req.source_job_id {
        Some(src_id) => stroem_db::JobRepo::get(&state.pool, src_id)
            .await
            .context("load source job for re-run")?,
        None => None,
    };
    // The source check runs BEFORE the pinned/unpinned branch and before any
    // destination lookup: a missing source and a source the caller may not
    // read (pinned or not) answer identically for every destination.
    if req.source_job_id.is_some() {
        let source_job = source_row.as_ref().ok_or_else(source_job_not_found)?;
        let perm = crate::web::api::jobs::check_job_acl(&state, &auth_user, source_job).await?;
        if matches!(perm, TaskPermission::Deny) {
            return Err(source_job_not_found());
        }
    }
    if let Some(source_job) = source_row.as_ref().filter(|j| j.git_ref.is_some()) {
        return execute_pinned_rerun(
            &state,
            &auth_user,
            &ws,
            &name,
            source_id.as_deref(),
            &req,
            source_job,
        )
        .await;
    }

    let workspace = get_workspace_or_error(&state, &ws).await?;

    // 2. Verify task exists in workspace
    let task = workspace
        .tasks
        .get(&name)
        .ok_or_else(|| AppError::not_found("Task"))?;

    // 3. ACL check: Deny -> 404, View -> 403, Run -> proceed
    require_task_run(&state, &auth_user, &ws, &name, task.folder.as_deref()).await?;

    // 4. Re-run validation: source_job_id must reference a job in this workspace
    //    that the user is allowed to view. Authorization mirrors GET /api/jobs/{id}.
    let mut effective_source_type = source_type;
    if req.source_job_id.is_some() {
        let source_job = source_row.ok_or_else(source_job_not_found)?;
        check_rerun_source(&source_job, &ws)?;
        // Same task as the source, on both paths (spec D12): a re-run copies
        // stored values only between runs of one task.
        if source_job.task_name != name {
            return Err(AppError::BadRequest(format!(
                "Source job {} is a run of task '{}', not '{}'",
                source_job.job_id, source_job.task_name, name
            )));
        }
        effective_source_type = "rerun";
    }

    let input_value = serde_json::to_value(&req.input).unwrap_or_default();

    // 5. Create job + steps via shared function
    let revision = state.workspaces.get_revision(&ws);
    let created = match req.source_job_id {
        Some(src_id) => {
            crate::job_creator::create_job_for_rerun(
                &state.workspaces,
                &state.pool,
                &workspace,
                &ws,
                &name,
                input_value,
                effective_source_type,
                source_id.as_deref(),
                revision.as_deref(),
                src_id,
                &req.replay_fields,
                state.config.agents.as_ref(),
                JobDefaults::from(state.config.as_ref()),
            )
            .await
        }
        None => {
            create_job_for_task_detailed(
                &state.workspaces,
                &state.pool,
                &workspace,
                &ws,
                &name,
                input_value,
                effective_source_type,
                source_id.as_deref(),
                revision.as_deref(),
                None,
                state.config.agents.as_ref(),
                JobDefaults::from(state.config.as_ref()),
            )
            .await
        }
    }
    .map_err(classify_execute_error)?;
    let job_id = created.job_id;

    // 6. Fire on_suspended hooks for any root-level approval steps that were
    //    suspended during job creation (FIX 2).
    crate::settlement::dispatch::fire_initial_suspended_hooks(&state, job_id).await;
    state.settlement().job_created(created).await;

    // 7. Return job_id
    Ok(Json(ExecuteTaskResponse {
        job_id: job_id.to_string(),
    }))
}

/// `Run` on the task path `{folder}/{name}` of workspace `ws`, for the
/// execute route and for a pinned Re-run / Restart at the folder the task
/// declares at the re-resolved commit (the new job's `task_folder`). Deny →
/// 404 "Task", View → 403 "View-only access". No user, or no ACL → allowed.
pub(crate) async fn require_task_run(
    state: &AppState,
    auth_user: &Option<AuthUser>,
    ws: &str,
    name: &str,
    folder: Option<&str>,
) -> Result<(), AppError> {
    let Some(auth) = auth_user else {
        return Ok(());
    };
    if !state.acl.is_configured() {
        return Ok(());
    }
    let user_id = auth.user_id()?;
    let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
        .await
        .context("load ACL context")?;
    let task_path = make_task_path(folder, name);
    match state
        .acl
        .evaluate(ws, &task_path, &auth.claims.email, &groups, is_admin)
    {
        TaskPermission::Deny => Err(AppError::not_found("Task")),
        TaskPermission::View => Err(AppError::Forbidden("View-only access".into())),
        TaskPermission::Run => Ok(()),
    }
}

/// The one answer for a re-run source that does not exist AND for one the
/// caller may not read (pinned or not): same status, same body, so neither
/// existence nor pinned status is observable.
fn source_job_not_found() -> AppError {
    AppError::NotFound("Source job not found".into())
}

/// The source checks every re-run makes, pinned or not: same workspace, a
/// top-level job, and a `raw_input` to replay.
fn check_rerun_source(source_job: &stroem_db::JobRow, ws: &str) -> Result<(), AppError> {
    if source_job.workspace != ws {
        return Err(AppError::BadRequest(
            "Source job belongs to a different workspace".into(),
        ));
    }
    // Same top-level-only rule as Restart: a re-run always creates a
    // parentless job, so re-running a `type: task` child or a hook job
    // detaches it from its parent and (for `hook`) escapes the hook
    // recursion guard by relabelling the source type `rerun`.
    if !crate::web::api::jobs::is_top_level_job(source_job) {
        return Err(AppError::BadRequest(
            "Only top-level jobs can be re-run".into(),
        ));
    }
    if source_job.raw_input.is_none() {
        return Err(AppError::BadRequest(
            "Source job predates Re-run prefill (no raw_input)".into(),
        ));
    }
    Ok(())
}

/// Re-run of a pinned source (spec § 7.3). The ACL check comes FIRST, so a
/// denied caller learns nothing about the source: `Run` on the SOURCE's path,
/// `{task_folder}/{task}` (§ 7.8), because the task may not exist in the live
/// config at all. Then the source checks of the unpinned path and the task
/// name; then the ref is re-resolved (a branch to its current tip), the task
/// looked up at that commit, and `Run` required on the folder it declares
/// there — the new job's `task_folder` — as the unpinned path requires it on
/// the live task's folder.
///
/// Deny → 404, View → 403 (either check), a bad source or a task missing at
/// the commit → 400, `RefNotFound` → 400, `PinUnavailable` → 500.
async fn execute_pinned_rerun(
    state: &AppState,
    auth_user: &Option<AuthUser>,
    ws: &str,
    name: &str,
    source_id: Option<&str>,
    req: &ExecuteTaskRequest,
    source_job: &stroem_db::JobRow,
) -> Result<Json<ExecuteTaskResponse>, AppError> {
    match crate::web::api::jobs::check_job_acl(state, auth_user, source_job).await? {
        TaskPermission::Deny => return Err(source_job_not_found()),
        TaskPermission::View => return Err(AppError::Forbidden("View-only access".into())),
        TaskPermission::Run => {}
    }
    check_rerun_source(source_job, ws)?;
    if source_job.task_name != name {
        return Err(AppError::BadRequest(format!(
            "Source job {} is a run of task '{}', not '{}'",
            source_job.job_id, source_job.task_name, name
        )));
    }

    let source_pin = super::pinned_source::resolve_source_pin(state, source_job)
        .await?
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "re-run source {} has no ref to re-resolve",
                source_job.job_id
            ))
        })?;
    let task = source_pin.task(name)?;
    require_task_run(state, auth_user, ws, name, task.folder.as_deref()).await?;
    let input_value = serde_json::to_value(&req.input).unwrap_or_default();

    let created = create_job_for_task_pinned(
        &state.workspaces,
        &state.pool,
        source_pin.handle.config(),
        ws,
        name,
        input_value,
        "rerun",
        source_id,
        &source_pin.pin.commit,
        &source_pin.pin.git_ref,
        CreationMode::Rerun {
            source_job_id: source_job.job_id,
            replay_fields: &req.replay_fields,
        },
        state.config.agents.as_ref(),
        JobDefaults::from(state.config.as_ref()),
    )
    .await
    .map_err(classify_execute_error)?;
    let job_id = created.job_id;

    crate::settlement::dispatch::fire_initial_suspended_hooks(state, job_id).await;
    state.settlement().job_created(created).await;

    Ok(Json(ExecuteTaskResponse {
        job_id: job_id.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use stroem_common::depends_on::{AnyEntry, DependsOnEntry};
    use stroem_common::models::workflow::FlowStep;

    fn flow_step_with_entries(depends_on: Vec<DependsOnEntry>) -> FlowStep {
        FlowStep {
            action: "shell/bash".to_string(),
            name: None,
            description: None,
            depends_on,
            input: HashMap::new(),
            continue_on_failure: false,
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

    /// `TaskDetail.flow` (see `get_task_detail` above) is built by
    /// `serde_json::to_value`-ing each `FlowStep` directly — this pins that a
    /// grouped `depends_on` entry round-trips as-authored, not flattened to
    /// bare step names.
    #[test]
    fn task_detail_serializes_grouped_dependencies_as_authored() {
        let mut flow = HashMap::new();
        flow.insert(
            "m".to_string(),
            flow_step_with_entries(vec![DependsOnEntry::Any(AnyEntry {
                any: vec![
                    DependsOnEntry::Name("a".to_string()),
                    DependsOnEntry::Name("b".to_string()),
                ],
            })]),
        );

        let flow_json: HashMap<String, serde_json::Value> = flow
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or_default()))
            .collect();

        assert_eq!(flow_json["m"]["depends_on"], json!([{"any": ["a", "b"]}]));
    }

    #[test]
    fn task_detail_serializes_step_accept_entry() {
        let mut flow = HashMap::new();
        flow.insert(
            "notify".to_string(),
            flow_step_with_entries(vec![DependsOnEntry::Step(
                stroem_common::depends_on::StepEntry {
                    step: "build".to_string(),
                    accept: stroem_common::depends_on::AcceptSet::Outcomes(vec![
                        stroem_common::depends_on::Outcome::Failed,
                    ]),
                },
            )]),
        );

        let flow_json: HashMap<String, serde_json::Value> = flow
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or_default()))
            .collect();

        assert_eq!(
            flow_json["notify"]["depends_on"],
            json!([{"step": "build", "accept": ["failed"]}])
        );
    }
}
