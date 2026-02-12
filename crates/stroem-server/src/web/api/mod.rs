pub mod auth;
pub mod jobs;
pub mod middleware;
pub mod tasks;
pub mod ws;

use crate::state::AppState;
use axum::{routing::get, routing::post, Router};
use std::sync::Arc;

pub fn build_api_routes(state: Arc<AppState>) -> Router {
    Router::new()
        // Task routes
        .route("/tasks", get(tasks::list_tasks))
        .route("/tasks/{name}", get(tasks::get_task))
        .route("/tasks/{name}/execute", post(tasks::execute_task))
        // Job routes
        .route("/jobs", get(jobs::list_jobs))
        .route("/jobs/{id}", get(jobs::get_job))
        .route("/jobs/{id}/logs", get(jobs::get_job_logs))
        // WebSocket log streaming
        .route("/jobs/{id}/logs/stream", get(ws::job_log_stream))
        // Auth routes
        .route("/auth/login", post(auth::login))
        .route("/auth/refresh", post(auth::refresh))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .with_state(state)
}
