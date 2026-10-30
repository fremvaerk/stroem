pub mod api;
pub mod error;
pub mod health;
pub mod hooks;
pub mod metrics;
pub mod worker_api;

use crate::state::AppState;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

#[derive(Embed)]
#[folder = "static/"]
struct StaticFiles;

pub fn build_router(state: AppState, cancel_token: CancellationToken) -> Router {
    let state = Arc::new(state);

    // Build CORS layer: restrict to base_url origin when auth is configured,
    // otherwise allow any origin (dev mode / embedded UI).
    let cors = match state
        .config
        .auth
        .as_ref()
        .and_then(|a| a.base_url.as_deref())
    {
        Some(url) => match url.parse::<header::HeaderValue>() {
            Ok(origin) => CorsLayer::new()
                .allow_origin(origin)
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
                .allow_credentials(true),
            Err(e) => {
                tracing::warn!(
                    base_url = url,
                    error = %e,
                    "auth.base_url could not be parsed as a valid CORS origin, falling back to allow-any"
                );
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any)
            }
        },
        None => CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any),
    };

    // /livez — unauthenticated liveness probe (background loops; no DB, no task names).
    // /healthz — unauthenticated readiness probe (DB only; no leader/task fields).
    // /healthz/detail — worker-token auth required; full HA diagnostics.
    let health_route = Router::new()
        .route("/livez", get(health::livez))
        .route("/healthz", get(health::healthz))
        .with_state(state.clone());

    let health_detail_route = Router::new()
        .route("/healthz/detail", get(health::healthz_detail))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            worker_api::auth_middleware,
        ))
        .with_state(state.clone());

    // /metrics — Prometheus scrape endpoint.
    // Auth: gated by worker_token unless `metrics.public: true`.
    let metrics_public = state.config.metrics.as_ref().is_some_and(|m| m.public);
    let mut metrics_route = Router::new().route("/metrics", get(metrics::metrics_handler));
    if !metrics_public {
        metrics_route = metrics_route.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            metrics::metrics_auth_middleware,
        ));
    }
    let metrics_route = metrics_route.with_state(state.clone());

    let mut router = Router::new()
        .merge(health_route)
        .merge(health_detail_route)
        .merge(metrics_route)
        .nest(
            "/api",
            api::build_api_routes(state.clone()).layer(axum::middleware::from_fn(
                crate::metrics::track_http_metrics,
            )),
        )
        .nest(
            "/worker",
            worker_api::build_worker_api_routes(state.clone()),
        )
        .nest("/hooks", hooks::build_hooks_routes(state.clone()));

    #[cfg(feature = "mcp")]
    if state.config.mcp.as_ref().is_some_and(|m| m.enabled) {
        let mcp_routes = crate::mcp::build_mcp_routes(state.clone(), cancel_token);
        router = router.nest("/mcp", mcp_routes);
        tracing::info!("MCP endpoint enabled at /mcp");

        // Spec-compliant OAuth 2.1 discovery is only meaningful when auth is
        // actually enforced. Without `auth`, the MCP middleware passes every
        // request through, so there's no 401 for clients to react to and no
        // reason to advertise an authorization server.
        if state.config.auth.is_some() {
            router = router.merge(crate::oauth::build_oauth_routes(state.clone()));
            tracing::info!("OAuth 2.1 metadata endpoints enabled for MCP");
        }
    }

    router
        .fallback(static_handler)
        .layer(cors)
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span))
}

/// The request span: `DefaultMakeSpan`'s fields and level, with the URI
/// through [`span_uri`].
fn make_request_span(req: &Request<Body>) -> tracing::Span {
    tracing::debug_span!(
        "request",
        method = %req.method(),
        uri = %span_uri(req.uri()),
        version = ?req.version(),
    )
}

/// The request URI as the request span records it. The segment after
/// `/api/uploads/` is an upload URL's single-use token, a credential
/// (CLAUDE.md § Secrets in logs): it is recorded as `{token}`. Every other
/// URI is recorded as is.
pub(crate) fn span_uri(uri: &axum::http::Uri) -> String {
    const UPLOADS: &str = "/api/uploads/";
    let Some(rest) = uri.path().strip_prefix(UPLOADS) else {
        return uri.to_string();
    };
    let after_token = &rest[rest.find('/').unwrap_or(rest.len())..];
    let mut out = String::new();
    if let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) {
        out.push_str(scheme);
        out.push_str("://");
        out.push_str(authority.as_str());
    }
    out.push_str(UPLOADS);
    out.push_str("{token}");
    out.push_str(after_token);
    if let Some(query) = uri.query() {
        out.push('?');
        out.push_str(query);
    }
    out
}

async fn static_handler(req: Request<Body>) -> Response {
    let path = req.uri().path().trim_start_matches('/');

    // Try to serve exact file
    if let Some(file) = StaticFiles::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime.as_ref())],
            file.data,
        )
            .into_response();
    }

    // SPA fallback: serve index.html for non-API, non-worker routes
    match StaticFiles::get("index.html") {
        Some(file) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html")],
            file.data,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

#[cfg(test)]
mod tests;
