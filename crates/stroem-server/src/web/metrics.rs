//! `GET /metrics` — Prometheus text-format scrape endpoint.

use crate::state::AppState;
use crate::web::error::AppError;
use axum::extract::{Extension, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::Arc;

/// Render Prometheus text format. Updates pull-mode gauges first, then
/// renders the registry.
#[tracing::instrument(skip(state, handle))]
pub async fn metrics_handler(
    State(state): State<Arc<AppState>>,
    Extension(handle): Extension<PrometheusHandle>,
) -> Result<Response, AppError> {
    crate::metrics::gather_gauges(&state).await;
    let body = handle.render();
    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response())
}
