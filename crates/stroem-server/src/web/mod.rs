pub mod api;
pub mod error;
pub mod health;
pub mod hooks;
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

    let health_route = Router::new()
        .route("/healthz", get(health::healthz))
        .with_state(state.clone());

    let mut router = Router::new()
        .merge(health_route)
        .nest("/api", api::build_api_routes(state.clone()))
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
    }

    router
        .fallback(static_handler)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
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
