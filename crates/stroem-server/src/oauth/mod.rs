//! OAuth 2.1 endpoints for MCP authentication.
//!
//! Per the MCP authorization spec (revision 2025-06-18), Strøm acts as both
//! the OAuth 2.1 Authorization Server and the Resource Server for `/mcp`.
//! This module owns the discovery surface (RFC 9728 protected-resource
//! metadata, RFC 8414 authorization-server metadata) and — in later phases —
//! the interactive authorize/token endpoints and Dynamic Client Registration.
//!
//! Phase 1 (this commit): metadata + 501 stubs for the interactive endpoints,
//! plus a canonical issuer helper reused by the MCP `WWW-Authenticate` header.

mod authorize;
mod metadata;
mod pkce;
mod register;
mod token;

use crate::state::AppState;
use crate::web::api::ClientIpExtractor;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tower_governor::{errors::GovernorError, governor::GovernorConfigBuilder, GovernorLayer};

/// Build the OAuth router. Mounted at the application root (not under `/api`)
/// because the `.well-known` paths are spec-fixed and the `/oauth/*` paths
/// are conventional OAuth surface that lives outside the Strøm REST API.
pub fn build_oauth_routes(state: Arc<AppState>) -> Router {
    // /oauth/register is public (anyone can POST to register an MCP client).
    // Rate-limit it the same way /api/auth/login is — one token every 12s,
    // burst 5 — so an unbounded script can't fill the `oauth_client` table.
    // The module-level comment in `register.rs` promises this; that promise
    // wasn't kept previously, this layer makes it real.
    let governor_cfg = GovernorConfigBuilder::default()
        .key_extractor(ClientIpExtractor)
        .per_second(12)
        .burst_size(5)
        .finish()
        .expect("governor config: per_second and burst_size must be > 0");
    let dcr_rate_limit = GovernorLayer::new(governor_cfg).error_handler(|e| {
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
    });

    let register_routes = Router::new()
        .route("/oauth/register", post(register::register))
        .layer(dcr_rate_limit);

    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(metadata::protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(metadata::authorization_server_metadata),
        )
        .route("/oauth/authorize", get(authorize::authorize))
        .route("/oauth/token", post(token::token))
        .merge(register_routes)
        .with_state(state)
}

pub use metadata::{canonical_issuer, MCP_SCOPE};
