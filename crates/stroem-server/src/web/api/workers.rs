use crate::state::AppState;
use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use stroem_db::WorkerRepo;

#[derive(Debug, Deserialize)]
pub struct ListWorkersQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    50
}

/// GET /api/workers - List registered workers
#[tracing::instrument(skip(state))]
pub async fn list_workers(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListWorkersQuery>,
) -> impl IntoResponse {
    match WorkerRepo::list(&state.pool, query.limit, query.offset).await {
        Ok(workers) => {
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

            Json(workers_json).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to list workers: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list workers: {}", e)})),
            )
                .into_response()
        }
    }
}
