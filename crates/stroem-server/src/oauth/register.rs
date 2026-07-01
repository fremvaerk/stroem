//! Dynamic Client Registration (RFC 7591).
//!
//! `POST /oauth/register` — public endpoint. Lets MCP clients (Claude
//! Desktop, Cursor, MCP Inspector, …) self-register on first contact
//! instead of requiring an admin to mint credentials by hand. Without DCR
//! the spec-compliant flow is impossible: every new editor would need an
//! out-of-band setup step.
//!
//! Mitigations against abuse (anyone can hit this endpoint):
//! * Each registration expires after `DCR_CLIENT_TTL_DAYS` days. Long-lived
//!   integrations re-register on first use after expiry; the cost is one
//!   automatic POST.
//! * Hard cap on `redirect_uris` length (`MAX_REDIRECT_URIS`).
//! * Hard cap on `client_name` length (`MAX_CLIENT_NAME_LEN`).
//! * Allowed grant types restricted to `authorization_code` + `refresh_token`.
//! * Allowed scope restricted to `mcp`.
//! * Allowed token-endpoint auth methods: `none` (public + PKCE) or
//!   `client_secret_post` (confidential).
//!
//! IP-based rate limiting is layered on top by the existing `auth_rate_limit`
//! governor, mirroring the protection on `/api/auth/login`.

use crate::auth::generate_api_key;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::Duration;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use stroem_db::OAuthClientRepo;
use uuid::Uuid;

const DCR_CLIENT_TTL_DAYS: i64 = 30;
const MAX_REDIRECT_URIS: usize = 5;
const MAX_CLIENT_NAME_LEN: usize = 200;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    /// Human-readable client name shown on the consent screen.
    pub client_name: String,
    /// Redirect URIs the client will use. Exact match at `/authorize` time.
    /// At least one required; bounded by `MAX_REDIRECT_URIS`.
    pub redirect_uris: Vec<String>,
    /// Defaults to `["authorization_code", "refresh_token"]`. Any value
    /// outside that set is rejected — Strøm doesn't speak password,
    /// device-code, or implicit grants.
    #[serde(default)]
    pub grant_types: Option<Vec<String>>,
    /// Defaults to `"mcp"`. Other scopes rejected.
    #[serde(default)]
    pub scope: Option<String>,
    /// `"none"` (default) → public client (PKCE only).
    /// `"client_secret_post"` → confidential client, secret returned in the
    ///   response and required at `/oauth/token`.
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub client_id: String,
    /// Only present for confidential clients. RFC 7591 §3.2.1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Seconds since epoch. RFC 7591 §3.2.1.
    pub client_id_issued_at: i64,
    /// RFC 7591 §3.2.1: `0` means "no expiry". We always return our actual
    /// expiry — clients should re-register on or before this time.
    pub client_secret_expires_at: i64,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub scope: String,
    pub token_endpoint_auth_method: String,
    pub client_name: String,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    error_description: String,
}

fn invalid_metadata(desc: impl Into<String>) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            // RFC 7591 §3.2.2 specifies this exact error code for bad client metadata.
            error: "invalid_client_metadata",
            error_description: desc.into(),
        }),
    )
        .into_response()
}

/// Validate that a redirect URI is well-formed and uses a permitted scheme.
///
/// This is an **allowlist** (the inverse of a denylist) per OAuth 2.1
/// §4.1 + RFC 8252 §7.1:
///
/// * `https://...` — production web clients.
/// * `http://localhost[:port]`, `http://127.0.0.1[:port]`, `http://[::1]`
///   — loopback redirects for native apps that bind a local port.
/// * Private-use URI schemes that look like reverse-DNS (e.g.
///   `com.cursor.app:`, `dev.stroem.cli:`) per RFC 8252 §7.1 — used by
///   mobile / native apps to capture redirects via the OS's app-link
///   registry. We require the scheme to contain at least one dot and a
///   non-empty subdomain to filter out single-word schemes like
///   `intent:`, `chrome:`, `about:`, etc.
///
/// Everything else (including `intent://`, `chrome://`, `javascript:`,
/// `data:`, arbitrary one-word schemes) is rejected.
fn validate_redirect_uri(uri: &str) -> Result<(), String> {
    let parsed = url::Url::parse(uri).map_err(|e| format!("invalid redirect_uri '{uri}': {e}"))?;
    let scheme = parsed.scheme();
    match scheme {
        "https" => Ok(()),
        "http" => {
            let host = parsed.host_str().unwrap_or("");
            if host == "localhost" || host == "127.0.0.1" || host == "[::1]" {
                Ok(())
            } else {
                Err(format!(
                    "http redirect_uri only allowed for localhost/127.0.0.1; got host '{host}'"
                ))
            }
        }
        // Private-use scheme per RFC 8252 §7.1. Reverse-DNS pattern: at
        // least two segments, no leading dot, no empty segments. This
        // rejects single-word schemes (`intent`, `chrome`, `about`, etc.)
        // that an attacker could register via DCR to redirect users into
        // an arbitrary OS handler.
        other => {
            if other.is_empty() {
                return Err("redirect_uri scheme is empty".to_string());
            }
            let segments: Vec<&str> = other.split('.').collect();
            let looks_reverse_dns = segments.len() >= 2 && segments.iter().all(|s| !s.is_empty());
            if looks_reverse_dns {
                Ok(())
            } else {
                Err(format!(
                    "redirect_uri scheme '{other}' not allowed; use https://, \
                     http://localhost, or a reverse-DNS private-use scheme \
                     (e.g. com.example.app:)"
                ))
            }
        }
    }
}

#[tracing::instrument(skip(state, req))]
pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> axum::response::Response {
    if req.client_name.trim().is_empty() {
        return invalid_metadata("client_name must not be empty");
    }
    if req.client_name.len() > MAX_CLIENT_NAME_LEN {
        return invalid_metadata(format!(
            "client_name exceeds maximum length of {MAX_CLIENT_NAME_LEN}"
        ));
    }

    if req.redirect_uris.is_empty() {
        return invalid_metadata("at least one redirect_uri is required");
    }
    if req.redirect_uris.len() > MAX_REDIRECT_URIS {
        return invalid_metadata(format!(
            "redirect_uris exceeds maximum of {MAX_REDIRECT_URIS}"
        ));
    }
    for uri in &req.redirect_uris {
        if let Err(e) = validate_redirect_uri(uri) {
            return invalid_metadata(e);
        }
    }

    // Restrict grant_types to what Strøm actually implements. RFC 7591
    // §3.2.2 says servers MAY reject unsupported grant types here; doing so
    // gives clients a clear early signal.
    let grant_types = req
        .grant_types
        .unwrap_or_else(|| vec!["authorization_code".into(), "refresh_token".into()]);
    for gt in &grant_types {
        if gt != "authorization_code" && gt != "refresh_token" {
            return invalid_metadata(format!(
                "grant_type '{gt}' is not supported (only authorization_code and refresh_token)"
            ));
        }
    }
    if !grant_types.iter().any(|g| g == "authorization_code") {
        return invalid_metadata(
            "authorization_code grant must be requested (it's the only flow we offer)",
        );
    }

    let scope = req
        .scope
        .unwrap_or_else(|| crate::oauth::MCP_SCOPE.to_string());
    if scope != crate::oauth::MCP_SCOPE {
        return invalid_metadata("only the `mcp` scope is supported");
    }

    let auth_method = req
        .token_endpoint_auth_method
        .unwrap_or_else(|| "none".to_string());
    if auth_method != "none" && auth_method != "client_secret_post" {
        return invalid_metadata(format!(
            "token_endpoint_auth_method '{auth_method}' is not supported"
        ));
    }
    let is_confidential = auth_method == "client_secret_post";

    // Generate client_id (UUID-ish) and, if confidential, a client_secret
    // using the same generator as API keys — 32 hex chars with a prefix.
    let client_id = format!("mcp_{}", Uuid::new_v4().simple());
    let (client_secret_raw, client_secret_hash) = if is_confidential {
        let (raw, hash) = generate_api_key();
        (Some(raw), Some(hash))
    } else {
        (None, None)
    };

    let now = chrono::Utc::now();
    let expires_at = now + Duration::days(DCR_CLIENT_TTL_DAYS);

    if let Err(e) = OAuthClientRepo::create(
        &state.pool,
        &client_id,
        client_secret_hash.as_deref(),
        &req.client_name,
        &req.redirect_uris,
        &grant_types,
        &scope,
        true, // is_dynamic
        Some(expires_at),
    )
    .await
    {
        tracing::error!(error = ?e, "Failed to persist DCR client");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                error: "server_error",
                error_description: "Failed to persist client".to_string(),
            }),
        )
            .into_response();
    }

    tracing::info!(
        client_id = %client_id,
        client_name = %req.client_name,
        is_confidential,
        "Registered new DCR client"
    );

    // RFC 7591 says respond with 201 Created on success.
    let body = RegisterResponse {
        client_id,
        client_secret: client_secret_raw,
        client_id_issued_at: now.timestamp(),
        client_secret_expires_at: expires_at.timestamp(),
        redirect_uris: req.redirect_uris,
        grant_types,
        scope,
        token_endpoint_auth_method: auth_method,
        client_name: req.client_name,
    };
    (StatusCode::CREATED, Json(body)).into_response()
}
