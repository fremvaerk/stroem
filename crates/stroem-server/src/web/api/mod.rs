pub mod api_keys;
pub mod auth;
pub mod jobs;
pub mod middleware;
pub mod oidc;
pub mod tasks;
pub mod triggers;
pub mod users;
pub mod workers;
pub mod workspaces;
pub mod ws;

use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{routing::delete, routing::get, routing::post, Json, Router};
use serde::Serialize;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tower_governor::{
    errors::GovernorError, governor::GovernorConfigBuilder, key_extractor::KeyExtractor,
    GovernorLayer,
};

/// Client IP extractor for rate limiting.
///
/// Resolution order: `X-Forwarded-For` → `X-Real-IP` → `Forwarded` →
/// `ConnectInfo<SocketAddr>` → fallback to `0.0.0.0`.
///
/// The fallback to `0.0.0.0` ensures requests that cannot be attributed to a
/// real IP address (e.g. unit-test `oneshot` calls without a TCP socket) are
/// still admitted rather than rejected with an extraction error.  In
/// production this case should not occur because the server uses
/// `into_make_service_with_connect_info`.
#[derive(Clone, Copy, Debug)]
struct ClientIpExtractor;

impl KeyExtractor for ClientIpExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &axum::http::Request<T>) -> Result<Self::Key, GovernorError> {
        let headers = req.headers();

        // X-Forwarded-For (first address in the list)
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').find_map(|p| p.trim().parse::<IpAddr>().ok()))
        {
            return Ok(ip);
        }

        // X-Real-IP
        if let Some(ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<IpAddr>().ok())
        {
            return Ok(ip);
        }

        // ConnectInfo<SocketAddr> extension (populated by into_make_service_with_connect_info)
        if let Some(ip) = req
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.ip())
        {
            return Ok(ip);
        }

        // SocketAddr extension (fallback for other setups)
        if let Some(ip) = req
            .extensions()
            .get::<std::net::SocketAddr>()
            .map(|a| a.ip())
        {
            return Ok(ip);
        }

        // Final fallback: use 0.0.0.0 so the request is not rejected outright.
        // This applies to test harnesses where no real TCP socket is present.
        //
        // WARNING: In production, all unattributed requests share a single rate
        // limit bucket (0.0.0.0). Deploy behind a reverse proxy (nginx, Caddy,
        // cloud LB) that sets X-Forwarded-For or X-Real-IP, or ensure the server
        // is started with `into_make_service_with_connect_info` (the default).
        Ok(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }
}

/// Build a `GovernorLayer` keyed on client IP using [`ClientIpExtractor`].
///
/// `per_second_replenish` is the number of seconds between token refills — e.g.
/// `12` means one token is added every 12 seconds.  `burst` is the maximum
/// number of requests that can be made before throttling kicks in.
///
/// The error handler returns a JSON body so API clients receive a consistent
/// `{"error": "Too many requests", "retry_after_secs": N}` payload instead of
/// the plain-text default.
macro_rules! auth_rate_limit_layer {
    ($per_second:expr, $burst:expr) => {{
        let config = GovernorConfigBuilder::default()
            .key_extractor(ClientIpExtractor)
            .per_second($per_second)
            .burst_size($burst)
            .finish()
            .expect("governor config: per_second and burst_size must be > 0");
        GovernorLayer::new(config).error_handler(|e| {
            let wait = match &e {
                GovernorError::TooManyRequests { wait_time, .. } => *wait_time,
                _ => 0,
            };
            let body = serde_json::to_string(&serde_json::json!({
                "error": "Too many requests",
                "retry_after_secs": wait,
            }))
            .unwrap_or_default();
            axum::http::Response::builder()
                .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .header(axum::http::header::RETRY_AFTER, wait.to_string())
                .body(axum::body::Body::from(body))
                .expect("valid 429 response")
        })
    }};
}

/// Parse a path parameter as a [`uuid::Uuid`], returning a 400 response on failure.
///
/// `entity_name` is used in the error message, e.g. `"job"` → `"Invalid job ID"`.
#[allow(clippy::result_large_err)]
pub fn parse_uuid_param(id: &str, entity_name: &str) -> Result<uuid::Uuid, Response> {
    id.parse::<uuid::Uuid>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Invalid {} ID", entity_name)})),
        )
            .into_response()
    })
}

/// Default pagination limit used across list endpoints.
pub fn default_limit() -> i64 {
    50
}

/// Resolve a workspace by name from [`AppState`], returning a 404 response when missing.
#[allow(clippy::result_large_err)]
pub async fn get_workspace_or_error(
    state: &std::sync::Arc<AppState>,
    ws: &str,
) -> Result<stroem_common::models::workflow::WorkspaceConfig, Response> {
    state.get_workspace(ws).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Workspace '{}' not found", ws)})),
        )
            .into_response()
    })
}

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
/// Accepts both JWT tokens and API keys (prefixed with `strm_`).
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
        Some(t) if t.starts_with("strm_") => {
            // API key path: validate via DB lookup
            match middleware::validate_api_key(t, &state).await {
                Ok(_) => next.run(req).await,
                Err(resp) => resp,
            }
        }
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
    // Rate-limited API key creation (strict: 5 req/min — one token every 12 s, burst 5).
    // Sits inside the protected router so auth is verified before the rate limit check.
    let api_key_create = Router::new()
        .route(
            "/auth/api-keys",
            get(api_keys::list_api_keys).post(api_keys::create_api_key),
        )
        .route("/auth/api-keys/{prefix}", delete(api_keys::delete_api_key))
        .layer(auth_rate_limit_layer!(12, 5));

    // Routes that require authentication (when auth is enabled).
    let protected = Router::new()
        .route("/workspaces", get(workspaces::list_workspaces))
        .route("/workspaces/{ws}/tasks", get(tasks::list_tasks))
        .route("/workspaces/{ws}/tasks/{name}", get(tasks::get_task))
        .route("/workspaces/{ws}/triggers", get(triggers::list_triggers))
        .route(
            "/workspaces/{ws}/tasks/{name}/execute",
            post(tasks::execute_task),
        )
        .route("/users", get(users::list_users))
        .route("/users/{id}", get(users::get_user))
        .route("/workers", get(workers::list_workers))
        .route("/workers/{id}", get(workers::get_worker))
        .route("/jobs", get(jobs::list_jobs))
        .route("/jobs/{id}", get(jobs::get_job))
        .route("/jobs/{id}/cancel", post(jobs::cancel_job))
        .route("/jobs/{id}/logs", get(jobs::get_job_logs))
        .route("/jobs/{id}/steps/{step}/logs", get(jobs::get_step_logs))
        .merge(api_key_create)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));

    // Strict login rate limit: 5 req/min per IP (one token every 12 s, burst 5).
    let login_routes = Router::new()
        .route("/auth/login", post(auth::login))
        .layer(auth_rate_limit_layer!(12, 5));

    // Moderate refresh rate limit: 10 req/min per IP (one token every 6 s, burst 10).
    let refresh_routes = Router::new()
        .route("/auth/refresh", post(auth::refresh))
        .layer(auth_rate_limit_layer!(6, 10));

    // Relaxed limit for logout / me / OIDC: 20 req/min per IP (one token every 3 s, burst 20).
    let general_auth_routes = Router::new()
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/auth/oidc/{provider}", get(oidc::oidc_start))
        .route("/auth/oidc/{provider}/callback", get(oidc::oidc_callback))
        .layer(auth_rate_limit_layer!(3, 20));

    // Public routes (no auth required — includes WS which handles auth internally).
    let public = Router::new()
        .route("/config", get(get_config))
        .route("/jobs/{id}/logs/stream", get(ws::job_log_stream))
        .merge(login_routes)
        .merge(refresh_routes)
        .merge(general_auth_routes);

    Router::new()
        .merge(protected)
        .merge(public)
        .with_state(state)
}
