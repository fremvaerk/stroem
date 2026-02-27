use crate::state::AppState;
use crate::web::api::{default_limit, parse_uuid_param};
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use stroem_db::{JobRepo, WorkerRepo};

#[derive(Debug, Deserialize)]
pub struct ListWorkersQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// GET /api/workers - List registered workers
#[tracing::instrument(skip(state))]
pub async fn list_workers(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListWorkersQuery>,
) -> impl IntoResponse {
    let result = WorkerRepo::list(&state.pool, query.limit, query.offset).await;
    let total = WorkerRepo::count(&state.pool).await;

    match (result, total) {
        (Ok(workers), Ok(total)) => {
            let workers_json: Vec<serde_json::Value> = workers
                .iter()
                .map(|w| {
                    json!({
                        "worker_id": w.worker_id,
                        "name": w.name,
                        "status": w.status,
                        "tags": w.tags,
                        "last_heartbeat": w.last_heartbeat,
                        "registered_at": w.registered_at,
                    })
                })
                .collect();

            Json(json!({ "items": workers_json, "total": total })).into_response()
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!("Failed to list workers: {:#}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list workers: {}", e)})),
            )
                .into_response()
        }
    }
}

/// GET /api/workers/:id - Get worker detail with recent jobs
#[tracing::instrument(skip(state))]
pub async fn get_worker(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let worker_id = match parse_uuid_param(&id, "worker") {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let worker = match WorkerRepo::get(&state.pool, worker_id).await {
        Ok(Some(w)) => w,
        Ok(None) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": "Worker not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("Failed to get worker: {:#}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get worker: {}", e)})),
            )
                .into_response();
        }
    };

    let jobs = match JobRepo::list_by_worker(&state.pool, worker_id, 50, 0).await {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::error!("Failed to list jobs for worker: {:#}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list jobs: {}", e)})),
            )
                .into_response();
        }
    };

    let jobs_json: Vec<serde_json::Value> = jobs
        .iter()
        .map(|j| {
            json!({
                "job_id": j.job_id,
                "workspace": j.workspace,
                "task_name": j.task_name,
                "mode": j.mode,
                "status": j.status,
                "source_type": j.source_type,
                "source_id": j.source_id,
                "worker_id": j.worker_id,
                "created_at": j.created_at,
                "started_at": j.started_at,
                "completed_at": j.completed_at,
            })
        })
        .collect();

    Json(json!({
        "worker_id": worker.worker_id,
        "name": worker.name,
        "status": worker.status,
        "tags": worker.tags,
        "last_heartbeat": worker.last_heartbeat,
        "registered_at": worker.registered_at,
        "jobs": jobs_json,
    }))
    .into_response()
}
