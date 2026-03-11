use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
use crate::job_creator::create_job_for_task;
use crate::state::AppState;
use crate::web::api::get_workspace_or_error;
use crate::web::api::middleware::AuthUser;
use crate::web::api::triggers::TriggerInfo;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use stroem_common::template::PRIMITIVE_TYPES;

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

#[derive(Debug, Deserialize)]
pub struct ExecuteTaskRequest {
    #[serde(default)]
    pub input: HashMap<String, serde_json::Value>,
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
) -> impl IntoResponse {
    // Resolve ACL context once if auth is present and ACL is configured.
    let acl_ctx = if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = match auth.user_id() {
                Ok(id) => id,
                Err(resp) => return resp,
            };
            match load_user_acl_context(&state.pool, user_id, auth.is_admin()).await {
                Ok(ctx) => Some(ctx),
                Err(e) => {
                    tracing::error!("Failed to load ACL context: {:#}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "Internal server error"})),
                    )
                        .into_response();
                }
            }
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
    Json(tasks).into_response()
}

/// GET /api/workspaces/:ws/tasks - List all tasks from a workspace
#[tracing::instrument(skip(state, auth_user))]
pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(ws): Path<String>,
) -> impl IntoResponse {
    let workspace = match get_workspace_or_error(&state, &ws).await {
        Ok(w) => w,
        Err(resp) => return resp,
    };

    // Resolve ACL context once if auth is present and ACL is configured.
    let acl_ctx = if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = match auth.user_id() {
                Ok(id) => id,
                Err(resp) => return resp,
            };
            match load_user_acl_context(&state.pool, user_id, auth.is_admin()).await {
                Ok(ctx) => Some(ctx),
                Err(e) => {
                    tracing::error!("Failed to load ACL context: {:#}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "Internal server error"})),
                    )
                        .into_response();
                }
            }
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

    Json(tasks).into_response()
}

/// GET /api/workspaces/:ws/tasks/:name - Get task detail with action info
#[tracing::instrument(skip(state, auth_user))]
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((ws, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let workspace = match get_workspace_or_error(&state, &ws).await {
        Ok(w) => w,
        Err(resp) => return resp,
    };

    let task = match workspace.tasks.get(&name) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Task not found"})),
            )
                .into_response()
        }
    };

    // ACL check: Deny -> 404 (task not found), View -> can_execute=false, Run -> can_execute=true
    let can_execute = if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = match auth.user_id() {
                Ok(id) => id,
                Err(resp) => return resp,
            };
            let (is_admin, groups) =
                match load_user_acl_context(&state.pool, user_id, auth.is_admin()).await {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        tracing::error!("Failed to load ACL context: {:#}", e);
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": "Internal server error"})),
                        )
                            .into_response();
                    }
                };
            let task_path = make_task_path(task.folder.as_deref(), &name);
            let perm = state
                .acl
                .evaluate(&ws, &task_path, &auth.claims.email, &groups, is_admin);
            match perm {
                TaskPermission::Deny => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": "Task not found"})),
                    )
                        .into_response()
                }
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

    Json(detail).into_response()
}

/// POST /api/workspaces/:ws/tasks/:name/execute - Trigger task execution
#[tracing::instrument(skip(state, auth_user, req))]
pub async fn execute_task(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((ws, name)): Path<(String, String)>,
    Json(req): Json<ExecuteTaskRequest>,
) -> impl IntoResponse {
    // 1. Enforce auth: when auth is enabled, require a valid token
    let (source_type, source_id) = match (state.config.auth.is_some(), &auth_user) {
        (false, _) => ("api", None),
        (true, Some(user)) => ("user", Some(user.claims.email.clone())),
        (true, None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response()
        }
    };

    let workspace = match get_workspace_or_error(&state, &ws).await {
        Ok(w) => w,
        Err(resp) => return resp,
    };

    // 2. Verify task exists in workspace
    let task = match workspace.tasks.get(&name) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Task not found"})),
            )
                .into_response()
        }
    };

    // 3. ACL check: Deny -> 404, View -> 403, Run -> proceed
    if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = match auth.user_id() {
                Ok(id) => id,
                Err(resp) => return resp,
            };
            let (is_admin, groups) =
                match load_user_acl_context(&state.pool, user_id, auth.is_admin()).await {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        tracing::error!("Failed to load ACL context: {:#}", e);
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": "Internal server error"})),
                        )
                            .into_response();
                    }
                };
            let task_path = make_task_path(task.folder.as_deref(), &name);
            let perm = state
                .acl
                .evaluate(&ws, &task_path, &auth.claims.email, &groups, is_admin);
            match perm {
                TaskPermission::Deny => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": "Task not found"})),
                    )
                        .into_response()
                }
                TaskPermission::View => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": "View-only access"})),
                    )
                        .into_response()
                }
                TaskPermission::Run => {} // allowed
            }
        }
    }

    let input_value = serde_json::to_value(&req.input).unwrap_or_default();

    // 4. Create job + steps via shared function
    let job_id = match create_job_for_task(
        &state.pool,
        &workspace,
        &ws,
        &name,
        input_value,
        source_type,
        source_id.as_deref(),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Failed to create job: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to create job: {}", e)})),
            )
                .into_response();
        }
    };

    // 5. Return job_id
    Json(ExecuteTaskResponse {
        job_id: job_id.to_string(),
    })
    .into_response()
}
