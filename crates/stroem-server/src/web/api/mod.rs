pub mod api_keys;
pub mod artifacts;
pub mod auth;
pub mod jobs;
pub mod middleware;
#[cfg(feature = "mcp")]
pub mod oauth_consent;
pub mod oidc;
pub mod state_upload;
pub mod tasks;
pub mod triggers;
pub mod users;
pub mod workers;
pub mod workspaces;
pub mod ws;

use crate::state::AppState;
use crate::web::error::AppError;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{routing::delete, routing::get, routing::post, routing::put, Json, Router};
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
pub struct ClientIpExtractor;

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

/// Parse a path parameter as a [`uuid::Uuid`], returning a 400 `AppError` on failure.
///
/// `entity_name` is used in the error message, e.g. `"job"` → `"Invalid job ID"`.
pub fn parse_uuid_param(id: &str, entity_name: &str) -> Result<uuid::Uuid, AppError> {
    id.parse::<uuid::Uuid>()
        .map_err(|_| AppError::BadRequest(format!("Invalid {} ID", entity_name)))
}

/// Default pagination limit used across list endpoints.
pub fn default_limit() -> i64 {
    50
}

/// Resolve a workspace by name from [`AppState`], returning a 404 `AppError` when missing.
pub async fn get_workspace_or_error(
    state: &std::sync::Arc<AppState>,
    ws: &str,
) -> Result<std::sync::Arc<stroem_common::models::workflow::WorkspaceConfig>, AppError> {
    state
        .get_workspace(ws)
        .await
        .ok_or_else(|| AppError::NotFound(format!("Workspace '{}' not found", ws)))
}

#[derive(Serialize)]
struct OidcProviderInfo {
    id: String,
    display_name: String,
}

/// GET /api/config -- public endpoint returning server configuration for the UI
#[tracing::instrument(skip(state))]
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
        "acl_enabled": state.acl.is_configured(),
        "oidc_providers": oidc_providers,
        "has_internal_auth": has_internal_auth,
        "version": env!("CARGO_PKG_VERSION"),
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
        Some(t) => match crate::auth::validate_access_token(t, &auth_config.jwt_secret, None) {
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
    // Rate limit on the API-key routes. The layer covers GET (list), POST
    // (create) AND DELETE on this router, and the UI issues list+create+reload
    // per key operation — so the previous strict (12 s / burst 5 ≈ 5 req/min)
    // budget was exhausted by *legitimate* use: opening Settings and creating a
    // couple of keys already 429s (and flaked the e2e suite, which creates
    // several keys quickly). Loosen to burst 60, one token/s (~60 req/min
    // sustained) — still bounds runaway/abusive creation on this
    // already-authenticated endpoint, but comfortably fits normal UI sessions.
    // Sits inside the protected router so auth is verified before the check.
    let api_key_create = Router::new()
        .route(
            "/auth/api-keys",
            get(api_keys::list_api_keys).post(api_keys::create_api_key),
        )
        .route("/auth/api-keys/{prefix}", delete(api_keys::delete_api_key))
        .layer(auth_rate_limit_layer!(1, 60));

    // OAuth consent endpoint — gated on the `mcp` feature because it only
    // exists to support the OAuth flow that fronts /mcp.
    #[cfg(feature = "mcp")]
    let oauth_consent_routes = Router::new()
        .route("/oauth/consent", post(oauth_consent::consent))
        .route(
            "/oauth/clients/{client_id}",
            get(oauth_consent::describe_client),
        );

    // Routes that require authentication (when auth is enabled).
    let protected = Router::new()
        .route("/workspaces", get(workspaces::list_workspaces))
        .route("/tasks", get(tasks::list_all_tasks))
        .route(
            "/workspaces/{ws}/refresh",
            post(workspaces::refresh_workspace),
        )
        .route("/workspaces/{ws}/tasks", get(tasks::list_tasks))
        .route("/workspaces/{ws}/tasks/{name}", get(tasks::get_task))
        .route(
            "/workspaces/{ws}/tasks/{name}/stats",
            get(tasks::get_task_stats),
        )
        .route("/workspaces/{ws}/triggers", get(triggers::list_triggers))
        .route(
            "/workspaces/{ws}/tasks/{name}/execute",
            post(tasks::execute_task),
        )
        // Per-route DefaultBodyLimit mirrors the pattern in worker_api/mod.rs:92-98.
        .route(
            "/workspaces/{ws}/tasks/{name}/state",
            post(state_upload::upload_task_state)
                .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024)),
        )
        .route(
            "/workspaces/{ws}/state",
            post(state_upload::upload_global_state)
                .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024)),
        )
        .route("/users", get(users::list_users).post(users::create_user))
        .route("/users/{id}", get(users::get_user))
        .route("/users/{id}/admin", put(users::set_user_admin))
        .route(
            "/users/{id}/groups",
            get(users::get_user_groups).put(users::set_user_groups),
        )
        .route("/groups", get(users::list_groups))
        .route("/workers", get(workers::list_workers))
        .route("/workers/{id}", get(workers::get_worker))
        .route("/stats", get(jobs::get_stats))
        .route("/jobs", get(jobs::list_jobs))
        .route("/jobs/{id}", get(jobs::get_job))
        .route("/jobs/{id}/cancel", post(jobs::cancel_job))
        .route("/jobs/{id}/restart", post(jobs::restart_job))
        .route("/jobs/{id}/steps/{step}/approve", post(jobs::approve_step))
        .route("/jobs/{id}/logs", get(jobs::get_job_logs))
        .route("/jobs/{id}/steps/{step}/logs", get(jobs::get_step_logs))
        .route("/jobs/{id}/artifacts", get(artifacts::list_artifacts))
        .route(
            "/jobs/{id}/artifacts/{name}",
            get(artifacts::download_artifact),
        )
        .merge(api_key_create);

    #[cfg(feature = "mcp")]
    let protected = protected.merge(oauth_consent_routes);

    let protected = protected.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        require_auth,
    ));

    // Login rate limit: 20 req/min per IP (one token every 3 s, burst 10).
    let login_routes = Router::new()
        .route("/auth/login", post(auth::login))
        .layer(auth_rate_limit_layer!(3, 10));

    // Refresh rate limit: 30 req/min per IP (one token every 2 s, burst 15).
    let refresh_routes = Router::new()
        .route("/auth/refresh", post(auth::refresh))
        .layer(auth_rate_limit_layer!(2, 15));

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

/// Classify a `create_job_for_task` failure as a 400 (author mistake) or a 500
/// (server/infra condition).
///
/// Two tiers of phrase matching, because trusting every layer of the context
/// chain equally is unsafe once wrapped infra errors (sqlx, I/O, ...) are in
/// the mix:
///
/// - **Precise phrases** (`is not shared`, `unknown workspace`, `has no
///   connection`) are specific enough to `stroem_common::template`'s
///   cross-workspace error text that they are safe to match anywhere in the
///   FULL context chain (`{:#}`) — `create_job_for_task` wraps the
///   author-facing phrase several `.context()` layers deep (e.g. "step
///   '...': failed to resolve connection inputs" -> "Input field '...'
///   references connection '...'" -> the actual cause).
/// - **Legacy broad phrases** (`not found`, `does not exist`, `resolve
///   connection`, `has no action`, `required`, `invalid`, `validation`,
///   `merge input defaults`) are common enough that an inner infra-layer
///   message could contain one by coincidence (e.g. a Postgres error's own
///   "relation ... does not exist"), so they are matched on the OUTERMOST
///   message only
///   (`e.to_string()`, which only renders the top context layer this
///   function's caller controls).
///
/// A configured-but-unavailable workspace (`"is not available"`, from
/// `Lookup::Unavailable` in `stroem_common::template`) is a transient server
/// condition, not an author mistake, and always stays a 500 (checked first,
/// anywhere in the chain) even though its message also contains substrings
/// like "workspace" that could otherwise look user-facing.
pub(crate) fn classify_execute_error(e: anyhow::Error) -> AppError {
    let chain = format!("{:#}", e);
    if chain.contains("is not available") {
        return AppError::Internal(e);
    }
    let precise_user_error = chain.contains("is not shared") // cross-workspace connection gate
        || chain.contains("unknown workspace") // qualified ref to a workspace that is not configured
        || chain.contains("has no connection"); // cross-workspace: owner workspace exists, connection doesn't
    if precise_user_error {
        return AppError::BadRequest(chain);
    }
    let outer = e.to_string();
    let legacy_user_error = outer.contains("not found")
        || outer.contains("does not exist") // connection/action missing
        || outer.contains("resolve connection") // resolve_connection_inputs context
        || outer.contains("has no action") // cross-workspace: owner workspace exists, action doesn't
        || outer.contains("required")
        || outer.contains("invalid")
        || outer.contains("validation")
        || outer.contains("merge input defaults"); // task-input default template render failure
    if legacy_user_error {
        AppError::BadRequest(chain)
    } else {
        AppError::Internal(e)
    }
}

#[cfg(test)]
mod classify_execute_error_tests {
    use super::*;

    #[test]
    fn unshared_cross_workspace_connection_is_bad_request() {
        let e = anyhow::anyhow!(
            "connection 'owner.private' exists in workspace 'owner' but is not shared (set `shared: true` on it in workspace 'owner')"
        )
        .context("Input field 'conn' references connection 'owner.private'")
        .context("Failed to resolve connection inputs");

        let err = classify_execute_error(e);
        match err {
            AppError::BadRequest(msg) => assert!(msg.contains("is not shared"), "{msg}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn unavailable_owner_workspace_is_internal() {
        let e = anyhow::anyhow!("connection 'owner.x': workspace 'owner' is not available")
            .context("Failed to resolve connection inputs");

        let err = classify_execute_error(e);
        assert!(
            matches!(err, AppError::Internal(_)),
            "expected Internal, got {err:?}"
        );
    }

    #[test]
    fn legacy_phrase_buried_in_an_infra_layer_is_internal() {
        // A Postgres-style inner error happens to contain "does not exist",
        // but only at an inner layer, not the outermost context this
        // function's caller actually attaches. Must not be misread as an
        // author mistake.
        let e =
            anyhow::anyhow!("relation \"job_step\" does not exist").context("Failed to create job");

        let err = classify_execute_error(e);
        assert!(
            matches!(err, AppError::Internal(_)),
            "expected Internal, got {err:?}"
        );
    }

    #[test]
    fn bad_default_template_is_bad_request() {
        let e = anyhow::anyhow!(
            "Failed to render default template for input field 'x': Variable `secret.nope` not found"
        )
        .context("Failed to merge input defaults");

        let err = classify_execute_error(e);
        match err {
            AppError::BadRequest(msg) => {
                assert!(msg.contains("Failed to merge input defaults"), "{msg}")
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn precise_phrase_buried_deep_in_chain_is_bad_request() {
        let e = anyhow::anyhow!(
            "connection 'owner.private' exists in workspace 'owner' but is not shared (set `shared: true` on it in workspace 'owner')"
        )
        .context("Input field 'conn' references connection 'owner.private'")
        .context("Failed to resolve connection inputs");

        let err = classify_execute_error(e);
        match err {
            AppError::BadRequest(msg) => assert!(msg.contains("is not shared"), "{msg}"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }
}
