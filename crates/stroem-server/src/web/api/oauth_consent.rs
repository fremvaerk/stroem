//! `POST /api/oauth/consent` — called by the SPA consent page.
//!
//! Flow:
//!   1. The OAuth client redirects the user's browser to `/oauth/authorize`.
//!   2. `/oauth/authorize` (in `crate::oauth::authorize`) validates the
//!      request and 302-redirects to `/consent?<params>` in the React app.
//!   3. The React app reads the user's login JWT from localStorage, displays
//!      a consent screen, and on "Allow" POSTs to this endpoint with the
//!      same params.
//!   4. We re-validate everything server-side (never trust the SPA echo),
//!      mint a single-use authorization code, persist its hash + PKCE
//!      challenge, and return the final redirect URL the SPA should navigate
//!      the browser to.

use crate::auth::hash_token;
use crate::oauth::MCP_SCOPE;
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::error::AppError;
use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::extract::State;
use axum::Json;
use chrono::Duration;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use stroem_db::{OAuthAuthorizationCodeRepo, OAuthClientRepo};

/// OAuth 2.1 §4.1.2 says authorization codes "SHOULD be short lived (e.g. 10
/// minutes)" — we pick 5 minutes to keep the replay window tight.
const AUTH_CODE_TTL_SECS: i64 = 300;

#[derive(Debug, Deserialize)]
pub struct ConsentRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: Option<String>,
    pub scope: String,
    pub resource: String,
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConsentResponse {
    /// Fully-qualified URL the SPA should navigate the browser to. Includes
    /// the `code` and optional `state` query params per OAuth 2.1 §4.1.2.
    pub redirect_url: String,
}

/// Public read-only "describe client" endpoint used by the SPA before
/// prompting the user. Returns only the display fields plus a single
/// boolean: whether the SPA-supplied `redirect_uri` is in the client's
/// registered list. Critically, **does NOT return the full
/// `redirect_uris` array** — leaking it lets any authenticated user (or
/// stolen low-privilege API key) enumerate every client's registered
/// callbacks for typosquatting and targeted phishing reconnaissance.
#[derive(Debug, Serialize)]
pub struct ClientDescribeResponse {
    pub client_id: String,
    pub client_name: String,
    pub is_dynamic: bool,
    /// True iff the `redirect_uri` query param matched one of the
    /// client's registered URIs. False when missing or mismatched — the
    /// SPA uses this to disable the Allow/Deny buttons and warn the user.
    pub redirect_uri_registered: bool,
}

#[derive(Debug, Deserialize)]
pub struct DescribeClientQuery {
    /// The redirect URI the OAuth client claimed at `/authorize`. We
    /// compare against the stored allowlist server-side and return only
    /// the boolean result — never the full list.
    #[serde(default)]
    pub redirect_uri: Option<String>,
}

#[tracing::instrument(skip(state, query))]
pub async fn describe_client(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(client_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<DescribeClientQuery>,
) -> Result<Json<ClientDescribeResponse>, AppError> {
    let client = OAuthClientRepo::get(&state.pool, &client_id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Unknown client_id".to_string()))?;

    let redirect_uri_registered = query
        .redirect_uri
        .as_deref()
        .map(|candidate| client.redirect_uris_vec().iter().any(|u| u == candidate))
        .unwrap_or(false);

    Ok(Json(ClientDescribeResponse {
        client_id: client.client_id.clone(),
        client_name: client.client_name.clone(),
        is_dynamic: client.is_dynamic,
        redirect_uri_registered,
    }))
}

#[tracing::instrument(skip(state, headers, auth, req))]
pub async fn consent(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    auth: AuthUser,
    Json(req): Json<ConsentRequest>,
) -> Result<Json<ConsentResponse>, AppError> {
    // API-key principals don't have a UI session, so they have no business
    // approving OAuth grants — only interactive JWT logins do.
    if auth.is_api_key {
        return Err(AppError::Forbidden(
            "OAuth consent requires an interactive user session, not an API key".to_string(),
        ));
    }

    // Re-validate everything server-side; the SPA-supplied values must match
    // what `/oauth/authorize` accepted, but the round-trip via the browser
    // is untrusted.
    let client = OAuthClientRepo::get(&state.pool, &req.client_id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB error: {e}")))?
        .ok_or_else(|| AppError::BadRequest("Unknown client_id".to_string()))?;

    if let Some(expires) = client.expires_at {
        if expires < chrono::Utc::now() {
            return Err(AppError::BadRequest(
                "Client registration has expired".to_string(),
            ));
        }
    }

    if !client
        .redirect_uris_vec()
        .iter()
        .any(|u| u == &req.redirect_uri)
    {
        return Err(AppError::BadRequest(
            "redirect_uri is not registered for this client".to_string(),
        ));
    }

    let method = req.code_challenge_method.as_deref().unwrap_or("S256");
    if method != "S256" {
        return Err(AppError::BadRequest(
            "code_challenge_method must be S256".to_string(),
        ));
    }
    if req.code_challenge.is_empty() {
        return Err(AppError::BadRequest(
            "code_challenge is required".to_string(),
        ));
    }
    if req.scope != MCP_SCOPE {
        return Err(AppError::BadRequest(
            "Only the `mcp` scope is supported".to_string(),
        ));
    }
    // RFC 8707 resource indicator: the SPA echo is untrusted — re-derive the
    // canonical resource from `auth.base_url` (or the request, in dev) and
    // require an exact match. Without this, a logged-in user POSTing
    // directly to /api/oauth/consent could mint a code whose token carries
    // an arbitrary `aud`, defeating audience binding for any future
    // audience-bound resource that trusts the same issuer.
    let expected_resource = format!("{}/mcp", crate::oauth::canonical_issuer(&state, &headers));
    if req.resource != expected_resource {
        return Err(AppError::BadRequest(format!(
            "resource must equal {expected_resource}"
        )));
    }

    let user_id = auth.user_id()?;

    // Mint code: 32 random bytes hex-encoded → 64-char opaque token. Hashed
    // at rest so a DB read can't replay it.
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let raw_code: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    let code_hash = hash_token(&raw_code);
    let expires = chrono::Utc::now() + Duration::seconds(AUTH_CODE_TTL_SECS);

    OAuthAuthorizationCodeRepo::create(
        &state.pool,
        &code_hash,
        &req.client_id,
        user_id,
        &req.redirect_uri,
        &req.code_challenge,
        "S256",
        &req.scope,
        &req.resource,
        expires,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to insert code: {e}")))?;

    // Build the redirect URL: <redirect_uri>?code=<raw>&state=<state>.
    let sep = if req.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut redirect_url = format!("{}{}code={}", req.redirect_uri, sep, raw_code);
    if let Some(s) = req.state.as_deref() {
        let encoded: String = url::form_urlencoded::byte_serialize(s.as_bytes()).collect();
        redirect_url.push_str(&format!("&state={}", encoded));
    }

    tracing::info!(
        client_id = %req.client_id,
        user_id = %user_id,
        "OAuth consent granted; authorization code issued"
    );

    Ok(Json(ConsentResponse { redirect_url }))
}
