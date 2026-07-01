//! OAuth 2.1 discovery metadata.
//!
//! Implements the two metadata documents an MCP client needs to bootstrap
//! the authorization flow:
//!
//! * **RFC 9728 — OAuth 2.0 Protected Resource Metadata**
//!   `/.well-known/oauth-protected-resource` advertises the resource URL,
//!   which authorization servers can issue tokens for it, and what scopes
//!   exist.
//!
//! * **RFC 8414 — OAuth 2.0 Authorization Server Metadata**
//!   `/.well-known/oauth-authorization-server` advertises the endpoints,
//!   grant types, and PKCE methods of Strøm-as-AS.
//!
//! Both are read by Claude Desktop, Cursor, MCP Inspector, and any other
//! spec-conformant client when they hit `/mcp` and receive a
//! `WWW-Authenticate: Bearer resource_metadata="..."` 401.

use crate::state::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use std::sync::Arc;

/// The single OAuth scope advertised for MCP access.
///
/// Fine-grained authorization is delegated to the existing Strøm ACL: a
/// token bearer's effective permissions still come from their group/user
/// rules. The OAuth layer just decides "may this principal call any MCP
/// tool at all" — the answer is yes iff they hold a token with this scope.
pub const MCP_SCOPE: &str = "mcp";

/// Return the canonical issuer URL for OAuth metadata.
///
/// Preference order:
/// 1. `auth.base_url` from the server config — explicit, production posture.
/// 2. `X-Forwarded-*` headers — for setups behind a TLS-terminating proxy.
/// 3. The request `Host` header with a best-guess scheme.
///
/// Operators SHOULD set `auth.base_url` explicitly; the fallback exists so
/// local-dev and single-host deployments work without extra config.
pub fn canonical_issuer(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(base) = state
        .config
        .auth
        .as_ref()
        .and_then(|a| a.base_url.as_deref())
    {
        return base.trim_end_matches('/').to_string();
    }

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");

    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
                "http".to_string()
            } else {
                "https".to_string()
            }
        });

    format!("{scheme}://{host}")
}

/// RFC 9728 §3 Protected Resource Metadata.
#[derive(Serialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<String>,
    bearer_methods_supported: Vec<String>,
    resource_name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_documentation: Option<String>,
}

pub async fn protected_resource_metadata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let issuer = canonical_issuer(&state, &headers);
    let meta = ProtectedResourceMetadata {
        resource: format!("{issuer}/mcp"),
        authorization_servers: vec![issuer.clone()],
        scopes_supported: vec![MCP_SCOPE.to_string()],
        bearer_methods_supported: vec!["header".to_string()],
        resource_name: "Strøm MCP",
        resource_documentation: Some(format!("{issuer}/docs/guides/mcp/")),
    };
    Json(meta)
}

/// RFC 8414 §2 Authorization Server Metadata.
#[derive(Serialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
    scopes_supported: Vec<&'static str>,
    response_types_supported: Vec<&'static str>,
    grant_types_supported: Vec<&'static str>,
    code_challenge_methods_supported: Vec<&'static str>,
    token_endpoint_auth_methods_supported: Vec<&'static str>,
}

pub async fn authorization_server_metadata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let issuer = canonical_issuer(&state, &headers);
    let meta = AuthorizationServerMetadata {
        issuer: issuer.clone(),
        authorization_endpoint: format!("{issuer}/oauth/authorize"),
        token_endpoint: format!("{issuer}/oauth/token"),
        registration_endpoint: format!("{issuer}/oauth/register"),
        scopes_supported: vec![MCP_SCOPE],
        response_types_supported: vec!["code"],
        grant_types_supported: vec!["authorization_code", "refresh_token"],
        // OAuth 2.1 mandates PKCE; S256 is the only method clients can rely on.
        code_challenge_methods_supported: vec!["S256"],
        // `none` is required for public clients (Claude Desktop, MCP Inspector);
        // `client_secret_post` is for confidential clients that opt into a
        // secret at registration time.
        token_endpoint_auth_methods_supported: vec!["none", "client_secret_post"],
    };
    Json(meta)
}
