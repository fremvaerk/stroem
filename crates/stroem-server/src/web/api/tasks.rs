use crate::auth::validate_access_token;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_db::{JobRepo, JobStepRepo, NewJobStep};

#[derive(Debug, Serialize)]
pub struct TaskListItem {
    pub name: String,
    pub mode: String,
    pub workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TaskDetail {
    pub name: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub input: HashMap<String, serde_json::Value>,
    pub flow: HashMap<String, serde_json::Value>,
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
        .map(|(name, task)| TaskListItem {
            name: name.clone(),
            mode: task.mode.clone(),
            workspace: ws.clone(),
            folder: task.folder.clone(),
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
    };

    Json(detail).into_response()
}

/// POST /api/workspaces/:ws/tasks/:name/execute - Trigger task execution
#[tracing::instrument(skip(state, headers))]
pub async fn execute_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((ws, name)): Path<(String, String)>,
    Json(req): Json<ExecuteTaskRequest>,
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

    // 1. Look up task in workspace
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

    // 2. Determine source based on authentication
    let (source_type, source_id) = {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        match token {
            Some(t) if state.config.auth.is_some() => {
                let secret = &state.config.auth.as_ref().unwrap().jwt_secret;
                match validate_access_token(t, secret) {
                    Ok(claims) => ("user", Some(claims.email)),
                    Err(_) => ("api", None),
                }
            }
            _ => ("api", None),
        }
    };
    let job_id = match JobRepo::create(
        &state.pool,
        &ws,
        &name,
        &task.mode,
        Some(serde_json::to_value(&req.input).unwrap_or_default()),
        source_type,
        source_id.as_deref(),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Failed to create job: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to create job: {}", e)})),
            )
                .into_response();
        }
    };

    // 3. Create job steps
    let mut new_steps = Vec::new();

    for (step_name, flow_step) in &task.flow {
        // Resolve action
        let action = match workspace.actions.get(&flow_step.action) {
            Some(a) => a,
            None => {
                tracing::error!("Action '{}' not found", flow_step.action);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("Action '{}' not found", flow_step.action)})),
                )
                    .into_response();
            }
        };

        // Determine initial status: ready if no dependencies, pending otherwise
        let status = if flow_step.depends_on.is_empty() {
            "ready"
        } else {
            "pending"
        };

        // Create action_spec from action definition
        let action_spec = serde_json::to_value(action).ok();

        let new_step = NewJobStep {
            job_id,
            step_name: step_name.clone(),
            action_name: flow_step.action.clone(),
            action_type: action.action_type.clone(),
            action_image: action.image.clone(),
            action_spec,
            input: Some(serde_json::to_value(&flow_step.input).unwrap_or_default()),
            status: status.to_string(),
        };

        new_steps.push(new_step);
    }

    // Insert all steps
    if let Err(e) = JobStepRepo::create_steps(&state.pool, &new_steps).await {
        tracing::error!("Failed to create job steps: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to create job steps: {}", e)})),
        )
            .into_response();
    }

    tracing::info!("Created job {} with {} steps", job_id, new_steps.len());

    // 4. Return job_id
    Json(ExecuteTaskResponse {
        job_id: job_id.to_string(),
    })
    .into_response()
}
