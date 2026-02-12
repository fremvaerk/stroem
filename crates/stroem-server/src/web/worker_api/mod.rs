pub mod jobs;
pub mod workspace;

use crate::state::AppState;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::sync::Arc;

/// Middleware to check worker authentication token
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Extract authorization header
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());

    let token = match auth_header {
        Some(header_value) => {
            // Expected format: "Bearer <token>"
            if let Some(token) = header_value.strip_prefix("Bearer ") {
                token
            } else {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "Invalid authorization header format"})),
                ));
            }
        }
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Missing authorization header"})),
            ))
        }
    };

    // Compare with configured worker token
    if token != state.config.worker_token {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Invalid worker token"})),
        ));
    }

    Ok(next.run(request).await)
}

pub fn build_worker_api_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/register", post(jobs::register_worker))
        .route("/heartbeat", post(jobs::heartbeat))
        .route("/jobs/claim", post(jobs::claim_job))
        .route("/jobs/{id}/steps/{step}/start", post(jobs::start_step))
        .route(
            "/jobs/{id}/steps/{step}/complete",
            post(jobs::complete_step),
        )
        .route("/jobs/{id}/logs", post(jobs::append_log))
        .route("/jobs/{id}/complete", post(jobs::complete_job))
        .route("/workspace/{ws}", get(workspace::download_workspace))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}
