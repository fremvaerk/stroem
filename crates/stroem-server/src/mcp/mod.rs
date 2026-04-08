pub(crate) mod auth;
mod handler;
mod tools;

use auth::McpAuthContext;
use axum::body::Body;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use handler::StromMcpHandler;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

tokio::task_local! {
    static MCP_AUTH: Option<McpAuthContext>;
}

/// Auth middleware for MCP requests.
///
/// Authenticates the request using the same logic as the REST API (JWT or API key).
/// When auth is not configured, passes through with `None` context.
/// When auth is configured and token is missing/invalid, returns 401.
async fn mcp_auth_middleware(
    state: Arc<AppState>,
    req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    let (parts, body) = req.into_parts();
    match auth::authenticate(&state, &parts).await {
        Ok(auth_ctx) => {
            let req = axum::http::Request::from_parts(parts, body);
            MCP_AUTH.scope(auth_ctx, next.run(req)).await
        }
        Err(msg) => {
            tracing::warn!(error = %msg, "MCP auth failed");
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": msg})),
            )
                .into_response()
        }
    }
}

/// Build the MCP routes to be nested in the Axum router at `/mcp`.
///
/// Returns a `Router` with auth middleware that wraps the StreamableHttpService.
/// The middleware validates auth and stores the context in a task-local,
/// which the handler factory reads when constructing per-request handlers.
pub fn build_mcp_routes(state: Arc<AppState>, ct: CancellationToken) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_sse_retry(None)
        .with_cancellation_token(ct);

    let factory_state = state.clone();
    let mcp_service = StreamableHttpService::new(
        move || {
            let auth = MCP_AUTH.try_with(|a| a.clone()).ok().flatten();
            Ok(StromMcpHandler::new(factory_state.clone(), auth))
        },
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let middleware_state = state.clone();
    Router::new()
        .fallback_service(mcp_service)
        .layer(middleware::from_fn(move |req, next| {
            let st = middleware_state.clone();
            mcp_auth_middleware(st, req, next)
        }))
}
