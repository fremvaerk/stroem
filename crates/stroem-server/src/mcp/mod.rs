pub(crate) mod auth;
mod handler;
mod tools;

use auth::McpAuthContext;
use axum::body::Body;
use axum::http::{header, StatusCode};
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
            unauthorized_with_metadata(&state, &parts.headers, &msg)
        }
    }
}

/// Build a 401 with the MCP-spec-mandated `WWW-Authenticate` header.
///
/// Per the MCP authorization spec (2025-06-18) and RFC 9728 §5.3, an MCP
/// server's 401 response MUST include a `WWW-Authenticate: Bearer` header
/// whose `resource_metadata` parameter points at the protected-resource
/// metadata document. Spec-conformant clients (Claude Desktop, Cursor, MCP
/// Inspector) follow that pointer, discover the authorization server, and
/// run the OAuth flow without any out-of-band configuration.
///
/// The header escapes any `"` in `msg` defensively even though the auth
/// layer never produces one today — quoted-string syntax (RFC 7235) would
/// otherwise break parsers if a future error message ever contained one.
fn unauthorized_with_metadata(
    state: &std::sync::Arc<AppState>,
    headers: &axum::http::HeaderMap,
    msg: &str,
) -> Response {
    let issuer = crate::oauth::canonical_issuer(state, headers);
    let resource_metadata = format!("{issuer}/.well-known/oauth-protected-resource");
    let safe_msg = msg.replace('"', "'");
    let www_auth = format!(
        r#"Bearer realm="mcp", resource_metadata="{resource_metadata}", error="invalid_token", error_description="{safe_msg}""#
    );

    let body = serde_json::to_vec(&serde_json::json!({"error": msg}))
        .unwrap_or_else(|_| b"{\"error\":\"unauthorized\"}".to_vec());

    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, www_auth)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::UNAUTHORIZED.into_response())
}

/// Build the MCP routes to be nested in the Axum router at `/mcp`.
///
/// Returns a `Router` with auth middleware that wraps the StreamableHttpService.
/// The middleware validates auth and stores the context in a task-local,
/// which the handler factory reads when constructing per-request handlers.
pub fn build_mcp_routes(state: Arc<AppState>, ct: CancellationToken) -> Router {
    // rmcp's Streamable-HTTP transport rejects any request whose `Host` is not
    // in `allowed_hosts` (DNS-rebinding protection), defaulting to loopback
    // only. Behind a reverse proxy at a public host that silently drops every
    // real request *after* auth passes. Allow-list the canonical `base_url`
    // host (plus loopback for local dev / `kubectl port-forward`).
    let allowed_hosts = resolve_allowed_hosts(
        state
            .config
            .auth
            .as_ref()
            .and_then(|a| a.base_url.as_deref()),
        state
            .config
            .mcp
            .as_ref()
            .map(|m| m.allowed_hosts.as_slice())
            .unwrap_or(&[]),
    );
    let mut config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_sse_retry(None)
        .with_cancellation_token(ct)
        .with_allowed_hosts(allowed_hosts);

    // Origin validation stays off by default (non-browser MCP clients like
    // Claude Code send no Origin); enable it only if operators opt in.
    let allowed_origins = state
        .config
        .mcp
        .as_ref()
        .map(|m| m.allowed_origins.clone())
        .unwrap_or_default();
    if !allowed_origins.is_empty() {
        config = config.with_allowed_origins(allowed_origins);
    }

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

/// Resolve the `Host` allow-list for the MCP transport's DNS-rebinding guard.
///
/// Always keeps the loopback authorities (local dev, `kubectl port-forward`),
/// adds the host — and `host:port` when a non-default port is present — parsed
/// from `auth.base_url`, then appends any operator-supplied `extra` entries.
/// Order is preserved and duplicates removed.
fn resolve_allowed_hosts(base_url: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];

    if let Some(base) = base_url {
        if let Ok(parsed) = url::Url::parse(base) {
            if let Some(host) = parsed.host_str() {
                hosts.push(host.to_string());
                if let Some(port) = parsed.port() {
                    hosts.push(format!("{host}:{port}"));
                }
            }
        }
    }

    for h in extra {
        let h = h.trim();
        if !h.is_empty() {
            hosts.push(h.to_string());
        }
    }

    let mut seen = std::collections::HashSet::new();
    hosts.retain(|h| seen.insert(h.clone()));
    hosts
}

#[cfg(test)]
mod tests {
    use super::resolve_allowed_hosts;

    #[test]
    fn keeps_loopback_when_no_base_url() {
        let hosts = resolve_allowed_hosts(None, &[]);
        assert_eq!(hosts, vec!["localhost", "127.0.0.1", "::1"]);
    }

    #[test]
    fn adds_base_url_host_without_default_port() {
        let hosts = resolve_allowed_hosts(Some("https://jobs.allunite.com"), &[]);
        assert!(hosts.contains(&"jobs.allunite.com".to_string()));
        // No explicit port on a default-443 URL, so no `host:port` entry.
        assert!(!hosts.iter().any(|h| h.contains("jobs.allunite.com:")));
    }

    #[test]
    fn adds_host_and_host_port_for_non_default_port() {
        let hosts = resolve_allowed_hosts(Some("http://stroem.internal:8080"), &[]);
        assert!(hosts.contains(&"stroem.internal".to_string()));
        assert!(hosts.contains(&"stroem.internal:8080".to_string()));
    }

    #[test]
    fn appends_extra_hosts_and_dedups() {
        let hosts = resolve_allowed_hosts(
            Some("https://jobs.allunite.com"),
            &[
                "alias.allunite.com".to_string(),
                "  ".to_string(),                // blank ignored
                "jobs.allunite.com".to_string(), // duplicate collapsed
            ],
        );
        assert!(hosts.contains(&"alias.allunite.com".to_string()));
        assert_eq!(
            hosts.iter().filter(|h| *h == "jobs.allunite.com").count(),
            1,
            "duplicate host must be collapsed"
        );
        assert!(!hosts.iter().any(|h| h.trim().is_empty()));
    }

    #[test]
    fn ignores_unparseable_base_url() {
        // Must not panic and must still return the loopback defaults.
        let hosts = resolve_allowed_hosts(Some("not a url"), &[]);
        assert!(hosts.contains(&"localhost".to_string()));
    }
}
