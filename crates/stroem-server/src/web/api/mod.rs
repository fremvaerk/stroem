pub mod jobs;
pub mod tasks;

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
        .with_state(state)
}
