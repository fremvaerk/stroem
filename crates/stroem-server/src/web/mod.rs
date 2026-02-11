pub mod api;
pub mod worker_api;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

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
        .layer(cors)
        .layer(TraceLayer::new_for_http())
}
