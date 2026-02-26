use crate::job_creator::create_job_for_task;
use crate::state::AppState;
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
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct TaskListItem {
    pub name: String,
    pub mode: String,
    pub workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub has_triggers: bool,
}

#[derive(Debug, Serialize)]
pub struct TaskDetail {
    pub name: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub input: HashMap<String, serde_json::Value>,
    pub flow: HashMap<String, serde_json::Value>,
    pub triggers: Vec<TriggerInfo>,
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

/// GET /api/workspaces/:ws/tasks - List all tasks from a workspace
#[tracing::instrument(skip(state))]
pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> impl IntoResponse {
    let workspace = match state.get_workspace(&ws).await {
        Some(w) => w,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("Workspace '{}' not found", ws)})),
            )
                .into_response()
        }
    };

    let tasks: Vec<TaskListItem> = workspace
        .tasks
        .iter()
        .map(|(name, task)| {
            let has_triggers = workspace
                .triggers
                .values()
                .any(|t| t.enabled() && t.task() == *name);
            TaskListItem {
                name: name.clone(),
                mode: task.mode.clone(),
                workspace: ws.clone(),
                folder: task.folder.clone(),
                has_triggers,
            }
        })
        .collect();

    Json(tasks).into_response()
}

/// GET /api/workspaces/:ws/tasks/:name - Get task detail with action info
#[tracing::instrument(skip(state))]
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path((ws, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let workspace = match state.get_workspace(&ws).await {
        Some(w) => w,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("Workspace '{}' not found", ws)})),
            )
                .into_response()
        }
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

    let triggers: Vec<TriggerInfo> = workspace
        .triggers
        .iter()
        .filter(|(_, t)| t.task() == name)
        .map(|(trig_name, trigger)| TriggerInfo::from_def(trig_name, trigger, 5))
        .collect();

    let detail = TaskDetail {
        name: name.clone(),
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
    let (source_type, source_id) = match (state.config.auth.is_some(), auth_user) {
        (false, _) => ("api", None),
        (true, Some(user)) => ("user", Some(user.0.email)),
        (true, None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Authentication required"})),
            )
                .into_response()
        }
    };

    let workspace = match state.get_workspace(&ws).await {
        Some(w) => w,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("Workspace '{}' not found", ws)})),
            )
                .into_response()
        }
    };

    // 2. Verify task exists in workspace
    if !workspace.tasks.contains_key(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Task not found"})),
        )
            .into_response();
    }

    let input_value = serde_json::to_value(&req.input).unwrap_or_default();

    // 3. Create job + steps via shared function
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

    // 4. Return job_id
    Json(ExecuteTaskResponse {
        job_id: job_id.to_string(),
    })
    .into_response()
}
