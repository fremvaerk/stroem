pub mod event_source;
pub mod jobs;
pub mod rendering;
pub mod workspace;

use crate::state::AppState;
use crate::web::error::AppError;
use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::header,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Middleware to check worker authentication token
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
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
                return AppError::Unauthorized("Invalid authorization header format".into())
                    .into_response();
            }
        }
        None => {
            return AppError::Unauthorized("Missing authorization header".into()).into_response();
        }
    };

    // Compare with configured worker token (constant-time to prevent timing attacks)
    let token_valid: bool = token
        .as_bytes()
        .ct_eq(state.config.worker_token.as_bytes())
        .into();
    if !token_valid {
        return AppError::Unauthorized("Invalid worker token".into()).into_response();
    }

    next.run(request).await
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
        .route("/jobs/{id}/cancelled", get(jobs::check_cancelled))
        .route("/jobs/{id}/logs", post(jobs::append_log))
        .route("/jobs/{id}/complete", post(jobs::complete_job))
        .route(
            "/jobs/{id}/steps/{step}/task-tool",
            post(jobs::agent_task_tool),
        )
        .route(
            "/jobs/{id}/steps/{step}/suspend",
            post(jobs::agent_suspend_step),
        )
        .route(
            "/jobs/{id}/steps/{step}/agent-state",
            post(jobs::agent_save_state),
        )
        .route(
            "/event-source/emit",
            post(event_source::emit_event).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/workspace/{ws}", get(workspace::download_workspace))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}
