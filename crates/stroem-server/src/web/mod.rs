pub mod api;
pub mod worker_api;

use crate::state::AppState;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use rust_embed::Embed;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

#[derive(Embed)]
#[folder = "static/"]
struct StaticFiles;

pub fn build_router(state: AppState) -> Router {
    let state = Arc::new(state);

    // Build CORS layer
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .nest("/api", api::build_api_routes(state.clone()))
        .nest(
            "/worker",
            worker_api::build_worker_api_routes(state.clone()),
        )
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
