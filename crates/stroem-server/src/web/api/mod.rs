pub mod auth;
pub mod jobs;
pub mod middleware;
pub mod oidc;
pub mod tasks;
pub mod workspaces;
pub mod ws;

use crate::state::AppState;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::{routing::get, routing::post, Json, Router};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Serialize)]
struct OidcProviderInfo {
    id: String,
    display_name: String,
}

/// GET /api/config -- public endpoint returning server configuration for the UI
async fn get_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let oidc_providers: Vec<OidcProviderInfo> = state
        .oidc_providers
        .iter()
        .map(|(id, p)| OidcProviderInfo {
            id: id.clone(),
            display_name: p.display_name.clone(),
        })
        .collect();

    // Internal auth is available when auth is enabled AND either:
    // - no providers are configured (backward compat: password login is the default)
    // - an explicit "internal" provider is configured
    let has_internal_auth = state
        .config
        .auth
        .as_ref()
        .map(|a| {
            a.providers.is_empty()
                || a.providers.values().any(|p| p.provider_type == "internal")
        })
        .unwrap_or(false);

    Json(json!({
        "auth_required": state.config.auth.is_some(),
        "oidc_providers": oidc_providers,
        "has_internal_auth": has_internal_auth,
    }))
}

pub fn build_api_routes(state: Arc<AppState>) -> Router {
    Router::new()
        // Public config endpoint
        .route("/config", get(get_config))
        // Workspace listing
        .route("/workspaces", get(workspaces::list_workspaces))
        // Workspace-scoped task routes
        .route("/workspaces/{ws}/tasks", get(tasks::list_tasks))
        .route("/workspaces/{ws}/tasks/{name}", get(tasks::get_task))
        .route(
            "/workspaces/{ws}/tasks/{name}/execute",
            post(tasks::execute_task),
        )
        // Job routes (jobs have workspace stored in DB)
        .route("/jobs", get(jobs::list_jobs))
        .route("/jobs/{id}", get(jobs::get_job))
        .route("/jobs/{id}/logs", get(jobs::get_job_logs))
        .route("/jobs/{id}/steps/{step}/logs", get(jobs::get_step_logs))
        // WebSocket log streaming
        .route("/jobs/{id}/logs/stream", get(ws::job_log_stream))
        // Auth routes
        .route("/auth/login", post(auth::login))
        .route("/auth/refresh", post(auth::refresh))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        // OIDC routes
        .route("/auth/oidc/{provider}", get(oidc::oidc_start))
        .route("/auth/oidc/{provider}/callback", get(oidc::oidc_callback))
        .with_state(state)
}
