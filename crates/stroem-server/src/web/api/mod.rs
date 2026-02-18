pub mod auth;
pub mod jobs;
pub mod middleware;
pub mod oidc;
pub mod tasks;
pub mod workers;
pub mod workspaces;
pub mod ws;

use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
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
            a.providers.is_empty() || a.providers.values().any(|p| p.provider_type == "internal")
        })
        .unwrap_or(false);

    Json(json!({
        "auth_required": state.config.auth.is_some(),
        "oidc_providers": oidc_providers,
        "has_internal_auth": has_internal_auth,
    }))
}

/// Middleware that rejects unauthenticated requests when auth is enabled.
/// When auth is not configured, all requests pass through.
async fn require_auth(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let auth_config = match &state.config.auth {
        Some(cfg) => cfg,
        None => return next.run(req).await,
    };

    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match token {
        Some(t) => match crate::auth::validate_access_token(t, &auth_config.jwt_secret) {
            Ok(_) => next.run(req).await,
            Err(_) => (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid or expired token"})),
            )
                .into_response(),
        },
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Authentication required"})),
        )
            .into_response(),
    }
}

pub fn build_api_routes(state: Arc<AppState>) -> Router {
    // Routes that require authentication (when auth is enabled)
    let protected = Router::new()
        .route("/workspaces", get(workspaces::list_workspaces))
        .route("/workspaces/{ws}/tasks", get(tasks::list_tasks))
        .route("/workspaces/{ws}/tasks/{name}", get(tasks::get_task))
        .route(
            "/workspaces/{ws}/tasks/{name}/execute",
            post(tasks::execute_task),
        )
        .route("/workers", get(workers::list_workers))
        .route("/jobs", get(jobs::list_jobs))
        .route("/jobs/{id}", get(jobs::get_job))
        .route("/jobs/{id}/logs", get(jobs::get_job_logs))
        .route("/jobs/{id}/steps/{step}/logs", get(jobs::get_step_logs))
        .route("/jobs/{id}/logs/stream", get(ws::job_log_stream))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    // Public routes (no auth required)
    let public = Router::new()
        .route("/config", get(get_config))
        .route("/auth/login", post(auth::login))
        .route("/auth/refresh", post(auth::refresh))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/auth/oidc/{provider}", get(oidc::oidc_start))
        .route("/auth/oidc/{provider}/callback", get(oidc::oidc_callback));

    Router::new()
        .merge(protected)
        .merge(public)
        .with_state(state)
}
