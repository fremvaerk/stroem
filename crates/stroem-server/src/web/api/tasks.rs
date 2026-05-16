use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
use crate::config::JobDefaults;
use crate::job_creator::create_job_for_task;
use crate::state::AppState;
use crate::web::api::get_workspace_or_error;
use crate::web::api::middleware::AuthUser;
use crate::web::api::triggers::TriggerInfo;
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

    // Build connections map: for each non-primitive input type, collect matching connection names
    let mut connections: HashMap<String, Vec<String>> = HashMap::new();
    let connection_types_needed: BTreeSet<&str> = task
        .input
        .values()
        .map(|f| f.field_type.as_str())
        .filter(|t| !PRIMITIVE_TYPES.contains(t))
        .collect();

    for conn_type in connection_types_needed {
        let mut names: Vec<String> = workspace
            .connections
            .iter()
            .filter(|(_, conn)| conn.connection_type.as_deref() == Some(conn_type))
            .map(|(conn_name, _)| conn_name.clone())
            .collect();
        if !names.is_empty() {
            names.sort();
            connections.insert(conn_type.to_string(), names);
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

    let workspace = get_workspace_or_error(&state, &ws).await?;

    // 2. Verify task exists in workspace
    let task = workspace
        .tasks
        .get(&name)
        .ok_or_else(|| AppError::not_found("Task"))?;

    // 3. ACL check: Deny -> 404, View -> 403, Run -> proceed
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
            match perm {
                TaskPermission::Deny => return Err(AppError::not_found("Task")),
                TaskPermission::View => {
                    return Err(AppError::Forbidden("View-only access".into()));
                }
                TaskPermission::Run => {} // allowed
            }
        }
    }

    // 4. Re-run validation: source_job_id must reference a job in this workspace
    //    that the user is allowed to view. Authorization mirrors GET /api/jobs/{id}.
    let mut effective_source_type = source_type;
    if let Some(src_id) = req.source_job_id {
        let source_job = stroem_db::JobRepo::get(&state.pool, src_id)
            .await
            .context("load source job for re-run")?
            .ok_or_else(|| AppError::BadRequest(format!("Source job {} not found", src_id)))?;
        if source_job.workspace != ws {
            return Err(AppError::BadRequest(
                "Source job belongs to a different workspace".into(),
            ));
        }
        if source_job.raw_input.is_none() {
            return Err(AppError::BadRequest(
                "Source job predates Re-run prefill (no raw_input)".into(),
            ));
        }
        // Authorization: user must have at least View on the source job's task path.
        let perm = crate::web::api::jobs::check_job_acl(
            &state,
            &auth_user,
            &source_job.workspace,
            &source_job.task_name,
        )
        .await?;
        if matches!(perm, TaskPermission::Deny) {
            return Err(AppError::Forbidden(
                "Not authorized to read source job".into(),
            ));
        }
        effective_source_type = "rerun";
    }

    let input_value = serde_json::to_value(&req.input).unwrap_or_default();

    // 5. Create job + steps via shared function
    let revision = state.workspaces.get_revision(&ws);
    let job_id = create_job_for_task(
        &state.pool,
        &workspace,
        &ws,
        &name,
        input_value,
        effective_source_type,
        source_id.as_deref(),
        revision.as_deref(),
        req.source_job_id,
        state.config.agents.as_ref(),
        JobDefaults::from(state.config.as_ref()),
    )
    .await
    .map_err(|e| {
        let msg = e.to_string();
        // Surface validation errors as 400; keep infrastructure errors as 500.
        if msg.contains("not found")
            || msg.contains("required")
            || msg.contains("invalid")
            || msg.contains("validation")
        {
            AppError::BadRequest(msg)
        } else {
            AppError::Internal(e)
        }
    })?;

    // 6. Fire on_suspended hooks for any root-level approval steps that were
    //    suspended during job creation (FIX 2).
    crate::job_creator::fire_initial_suspended_hooks(&state, &workspace, &ws, &name, job_id).await;

    // 7. Return job_id
    Ok(Json(ExecuteTaskResponse {
        job_id: job_id.to_string(),
    }))
}
