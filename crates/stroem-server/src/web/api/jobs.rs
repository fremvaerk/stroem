use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ListJobsQuery {
    pub workspace: Option<String>,
    pub task_name: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    50
}

/// GET /api/jobs - List jobs
#[tracing::instrument(skip(state))]
pub async fn list_jobs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListJobsQuery>,
) -> impl IntoResponse {
    if query.task_name.is_some() && query.workspace.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "task_name filter requires workspace"})),
        )
            .into_response();
    }

    let result = match (query.workspace.as_deref(), query.task_name.as_deref()) {
        (Some(ws), Some(task)) => {
            JobRepo::list_by_task(&state.pool, ws, task, query.limit, query.offset).await
        }
        _ => {
            JobRepo::list(
                &state.pool,
                query.workspace.as_deref(),
                query.limit,
                query.offset,
            )
            .await
        }
    };

    match result {
        Ok(jobs) => {
            let jobs_json: Vec<serde_json::Value> = jobs
                .iter()
                .map(|job| {
                    json!({
                        "job_id": job.job_id,
                        "workspace": job.workspace,
                        "task_name": job.task_name,
                        "mode": job.mode,
                        "status": job.status,
                        "source_type": job.source_type,
                        "source_id": job.source_id,
                        "worker_id": job.worker_id,
                        "created_at": job.created_at,
                        "started_at": job.started_at,
                        "completed_at": job.completed_at,
                    })
                })
                .collect();

            Json(jobs_json).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to list jobs: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list jobs: {}", e)})),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JobDetailResponse {
    pub job_id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub mode: String,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub status: String,
    pub source_type: String,
    pub source_id: Option<String>,
    pub worker_id: Option<Uuid>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub steps: Vec<serde_json::Value>,
}

/// GET /api/jobs/:id - Get job detail with steps
#[tracing::instrument(skip(state))]
pub async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid job ID"})),
            )
                .into_response()
        }
    };

    // Get job
    let job = match JobRepo::get(&state.pool, job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Job not found"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to get job: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get job: {}", e)})),
            )
                .into_response();
        }
    };

    // Get steps
    let steps = match JobStepRepo::get_steps_for_job(&state.pool, job_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to get job steps: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get job steps: {}", e)})),
            )
                .into_response();
        }
    };

    let mut steps_json: Vec<serde_json::Value> = steps
        .iter()
        .map(|step| {
            json!({
                "step_name": step.step_name,
                "action_name": step.action_name,
                "action_type": step.action_type,
                "action_image": step.action_image,
                "runner": step.runner,
                "input": step.input,
                "output": step.output,
                "status": step.status,
                "worker_id": step.worker_id,
                "started_at": step.started_at,
                "completed_at": step.completed_at,
                "error_message": step.error_message,
            })
        })
        .collect();

    // Sort steps by topological order (dependency-first) using the task flow
    if let Some(workspace) = state.get_workspace(&job.workspace).await {
        if let Some(task) = workspace.tasks.get(&job.task_name) {
            let mut in_deg: HashMap<&str, usize> = task
                .flow
                .iter()
                .map(|(name, fs)| (name.as_str(), fs.depends_on.len()))
                .collect();

            let mut queue: Vec<&str> = in_deg
                .iter()
                .filter(|(_, &d)| d == 0)
                .map(|(&n, _)| n)
                .collect();
            queue.sort();

            let mut topo_order: Vec<&str> = Vec::new();
            while let Some(node) = queue.first().copied() {
                queue.remove(0);
                topo_order.push(node);
                for (name, fs) in &task.flow {
                    if fs.depends_on.iter().any(|d| d == node) {
                        if let Some(deg) = in_deg.get_mut(name.as_str()) {
                            *deg -= 1;
                            if *deg == 0 {
                                queue.push(name.as_str());
                                queue.sort();
                            }
                        }
                    }
                }
            }

            let pos: HashMap<&str, usize> = topo_order
                .iter()
                .enumerate()
                .map(|(i, &n)| (n, i))
                .collect();
            steps_json.sort_by_key(|s| {
                s["step_name"]
                    .as_str()
                    .and_then(|n| pos.get(n).copied())
                    .unwrap_or(usize::MAX)
            });
        }
    }

    let response = JobDetailResponse {
        job_id: job.job_id,
        workspace: job.workspace,
        task_name: job.task_name,
        mode: job.mode,
        input: job.input,
        output: job.output,
        status: job.status,
        source_type: job.source_type,
        source_id: job.source_id,
        worker_id: job.worker_id,
        created_at: job.created_at.to_rfc3339(),
        started_at: job.started_at.map(|dt| dt.to_rfc3339()),
        completed_at: job.completed_at.map(|dt| dt.to_rfc3339()),
        steps: steps_json,
    };

    Json(response).into_response()
}

/// GET /api/jobs/:id/steps/:step/logs - Get per-step logs
#[tracing::instrument(skip(state))]
pub async fn get_step_logs(
    State(state): State<Arc<AppState>>,
    Path((id, step_name)): Path<(String, String)>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid job ID"})),
            )
                .into_response()
        }
    };

    match state.log_storage.get_step_log(job_id, &step_name).await {
        Ok(logs) => Json(json!({"logs": logs})).into_response(),
        Err(e) => {
            tracing::error!("Failed to get step logs: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get step logs: {}", e)})),
            )
                .into_response()
        }
    }
}

/// GET /api/jobs/:id/logs - Get log file contents
#[tracing::instrument(skip(state))]
pub async fn get_job_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid job ID"})),
            )
                .into_response()
        }
    };

    match state.log_storage.get_log(job_id).await {
        Ok(logs) => Json(json!({"logs": logs})).into_response(),
        Err(e) => {
            tracing::error!("Failed to get logs: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get logs: {}", e)})),
            )
                .into_response()
        }
    }
}
