//! OAuth 2.1 Authorization Endpoint (`GET /oauth/authorize`).
//!
//! Validates the incoming PKCE Authorization Code request, then hands off to
//! the React consent page (`/consent`) which prompts the user and POSTs the
//! decision to `/api/oauth/consent` (see `web/api/oauth.rs`). That endpoint
//! mints the authorization code and returns the final redirect URL.
//!
//! We split server-side validation (here) from consent (in the SPA) because
//! the React app already owns the login session — the browser carries the
//! login JWT in `localStorage`, not in an HTTP cookie. Doing consent in the
//! SPA keeps the backend stateless and avoids introducing a session cookie.

use crate::oauth::metadata::canonical_issuer;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use stroem_db::OAuthClientRepo;

/// Application/x-www-form-urlencoded escape, matching the existing `url`
/// crate usage at other call sites (e.g. `mcp/tools.rs`).
fn percent_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>()
}

/// Parameters per OAuth 2.1 §4.1.1 + RFC 7636 (PKCE) + RFC 8707 (resource).
#[derive(Debug, Deserialize)]
pub struct AuthorizeParams {
    /// MUST be `"code"`.
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    /// Opaque CSRF token the client expects echoed back at redirect time.
    /// Optional per spec but every real client sends one.
    #[serde(default)]
    pub state: Option<String>,
    /// PKCE — RFC 7636. OAuth 2.1 makes this mandatory for all clients.
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: Option<String>,
    /// Requested scope. We only support `"mcp"` today.
    #[serde(default)]
    pub scope: Option<String>,
    /// Resource indicator (RFC 8707). MUST match the canonical `/mcp` URL.
    #[serde(default)]
    pub resource: Option<String>,
}

/// Errors that are reported to the **client** via redirect-uri query params
/// per OAuth 2.1 §4.1.2.1. These are only used after we've validated the
/// redirect_uri itself — otherwise an attacker controlling the redirect_uri
/// could harvest error codes.
#[derive(Debug, Clone, Copy)]
enum RedirectError {
    UnsupportedResponseType,
    InvalidRequest,
    InvalidScope,
    InvalidTarget,
}

impl RedirectError {
    fn code(self) -> &'static str {
        match self {
            Self::UnsupportedResponseType => "unsupported_response_type",
            Self::InvalidRequest => "invalid_request",
            Self::InvalidScope => "invalid_scope",
            Self::InvalidTarget => "invalid_target",
        }
    }
}

/// Errors that MUST NOT be returned via the redirect (per OAuth 2.1 §4.1.2.1)
/// because either the client is unknown or the redirect_uri is invalid — in
/// either case we can't trust the URL, so we render an inline error.
fn direct_error(status: StatusCode, code: &str, description: &str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": code,
            "error_description": description,
        })),
    )
        .into_response()
}

fn redirect_with_error(
    redirect_uri: &str,
    err: RedirectError,
    state: Option<&str>,
    description: &str,
) -> Response {
    let mut url = redirect_uri.to_string();
    let sep = if url.contains('?') { '&' } else { '?' };
    url.push(sep);
    url.push_str(&format!(
        "error={}&error_description={}",
        percent_encode(err.code()),
        percent_encode(description),
    ));
    if let Some(s) = state {
        url.push_str(&format!("&state={}", percent_encode(s)));
    }
    Redirect::to(&url).into_response()
}

/// Handle `GET /oauth/authorize`.
///
/// On success: redirects to the React consent page with the validated params
/// in the query string. The SPA reads them, displays consent UI, then calls
/// `POST /api/oauth/consent` (Phase 2 cont.) to mint the actual code.
///
/// On error: either renders an inline JSON error (when the client / redirect
/// can't be trusted) or redirects to the client with `error=...` params.
#[tracing::instrument(skip(state, headers))]
pub async fn authorize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    let client = match OAuthClientRepo::get(&state.pool, &params.client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return direct_error(
                StatusCode::BAD_REQUEST,
                "invalid_client",
                "Unknown client_id",
            );
        }
        Err(e) => {
            tracing::error!(error = ?e, "DB error loading oauth_client");
            return direct_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Failed to load client",
            );
        }
    };

    if let Some(expires) = client.expires_at {
        if expires < chrono::Utc::now() {
            return direct_error(
                StatusCode::BAD_REQUEST,
                "invalid_client",
                "Client registration has expired",
            );
        }
    }

    if !client
        .redirect_uris_vec()
        .iter()
        .any(|u| u == &params.redirect_uri)
    {
        // OAuth 2.1 §4.1.2.1: if redirect_uri is invalid, do NOT redirect.
        return direct_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri is not registered for this client",
        );
    }

    // Past this point, redirect_uri is trusted — report further errors via
    // the redirect-with-error helper.
    let state_param = params.state.as_deref();

    if params.response_type != "code" {
        return redirect_with_error(
            &params.redirect_uri,
            RedirectError::UnsupportedResponseType,
            state_param,
            "Only response_type=code is supported",
        );
    }

    // OAuth 2.1 mandates PKCE. We refuse anything but S256 (the only secure
    // method) — `plain` was removed by OAuth 2.1.
    let method = params.code_challenge_method.as_deref().unwrap_or("plain");
    if method != "S256" {
        return redirect_with_error(
            &params.redirect_uri,
            RedirectError::InvalidRequest,
            state_param,
            "code_challenge_method must be S256",
        );
    }
    if params.code_challenge.is_empty() {
        return redirect_with_error(
            &params.redirect_uri,
            RedirectError::InvalidRequest,
            state_param,
            "code_challenge is required",
        );
    }

    let requested_scope = params.scope.as_deref().unwrap_or(crate::oauth::MCP_SCOPE);
    if requested_scope != crate::oauth::MCP_SCOPE {
        return redirect_with_error(
            &params.redirect_uri,
            RedirectError::InvalidScope,
            state_param,
            "Only the `mcp` scope is supported",
        );
    }

    // RFC 8707 resource indicator: must match the canonical /mcp URL so
    // the token we mint is audience-bound to *this* server's MCP endpoint
    // (and not replayable against any other resource that trusts the same
    // issuer).
    let issuer = canonical_issuer(&state, &headers);
    let expected_resource = format!("{issuer}/mcp");
    let resource = params.resource.as_deref().unwrap_or(&expected_resource);
    if resource != expected_resource {
        return redirect_with_error(
            &params.redirect_uri,
            RedirectError::InvalidTarget,
            state_param,
            "resource must equal this server's /mcp URL",
        );
    }

    // All validated. Hand off to the SPA consent page. The SPA preserves the
    // full query string (including code_challenge), shows the consent UI,
    // then calls POST /api/oauth/consent with the same parameters.
    //
    // We forward the params verbatim rather than re-encoding to keep the SPA
    // a thin shell: it doesn't need to know which fields are spec-mandated.
    let consent_url = build_consent_url(&params, resource);
    Redirect::to(&consent_url).into_response()
}

/// Render the SPA consent URL preserving every authorize param so the SPA
/// can echo them back when calling `/api/oauth/consent`.
fn build_consent_url(params: &AuthorizeParams, resource: &str) -> String {
    let mut url = String::from("/consent?");
    let mut sep = "";
    let push = |url: &mut String, sep: &mut &'static str, k: &str, v: &str| {
        url.push_str(sep);
        url.push_str(k);
        url.push('=');
        url.push_str(&percent_encode(v));
        *sep = "&";
    };
    push(&mut url, &mut sep, "response_type", &params.response_type);
    push(&mut url, &mut sep, "client_id", &params.client_id);
    push(&mut url, &mut sep, "redirect_uri", &params.redirect_uri);
    push(&mut url, &mut sep, "code_challenge", &params.code_challenge);
    push(&mut url, &mut sep, "code_challenge_method", "S256");
    push(&mut url, &mut sep, "scope", crate::oauth::MCP_SCOPE);
    push(&mut url, &mut sep, "resource", resource);
    if let Some(s) = params.state.as_deref() {
        push(&mut url, &mut sep, "state", s);
    }
    url
}
