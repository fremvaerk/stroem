//! `GET /metrics` — Prometheus text-format scrape endpoint.

use crate::state::AppState;
use crate::web::error::AppError;
use axum::extract::{Extension, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Auth middleware specifically for `/metrics`. Mirrors `worker_api::auth_middleware`
/// but returns errors as `text/plain` so Prometheus scrape-failure diagnostics
/// stay coherent with the success Content-Type.
pub(crate) async fn metrics_auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());

    let token = match auth_header.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(t) => t.to_owned(),
        None => {
            return text_plain(
                StatusCode::UNAUTHORIZED,
                "Missing or malformed Authorization header\n",
            );
        }
    };

    let expected = state
        .config
        .metrics
        .as_ref()
        .and_then(|m| m.token.as_deref())
        .unwrap_or(state.config.worker_token.as_str());
    let valid: bool = token.as_bytes().ct_eq(expected.as_bytes()).into();
    if !valid {
        return text_plain(StatusCode::UNAUTHORIZED, "Invalid token\n");
    }

    next.run(request).await
}

fn text_plain(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

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
        [
            (header::CONTENT_TYPE, "text/plain; version=0.0.4"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}
