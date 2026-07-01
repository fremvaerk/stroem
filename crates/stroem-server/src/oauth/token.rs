//! OAuth 2.1 Token Endpoint (`POST /oauth/token`).
//!
//! Two grants:
//! * `authorization_code` — exchanges a PKCE-verified code for an access
//!   token (JWT, audience-bound) and a refresh token (opaque, DB-backed).
//! * `refresh_token` — rotates a refresh token and mints a new access token.
//!
//! Tokens this endpoint issues are intended for `/mcp` only — the `aud`
//! claim is set to the canonical resource URL and the MCP middleware
//! enforces that on every request (`mcp/auth.rs`).

use crate::auth::hash_token;
use crate::oauth::metadata::canonical_issuer;
use crate::oauth::pkce;
use crate::state::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Form;
use axum::Json;
use jsonwebtoken::{EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use stroem_common::models::auth::Claims;
use stroem_db::{OAuthAuthorizationCodeRepo, OAuthClientRepo, OAuthRefreshTokenRepo};

/// OAuth 2.1 §5.2 standard error codes (the ones we actually emit).
const ERR_INVALID_REQUEST: &str = "invalid_request";
const ERR_INVALID_CLIENT: &str = "invalid_client";
const ERR_INVALID_GRANT: &str = "invalid_grant";
const ERR_UNSUPPORTED_GRANT_TYPE: &str = "unsupported_grant_type";
const ERR_SERVER_ERROR: &str = "server_error";

/// Access tokens live 1 hour (OAuth convention). Refresh tokens live 30 days,
/// matching the existing login refresh-token TTL.
const ACCESS_TOKEN_TTL_SECS: i64 = 3600;
const REFRESH_TOKEN_TTL_SECS: i64 = 30 * 24 * 3600;

#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,

    // authorization_code grant fields:
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub code_verifier: Option<String>,

    // refresh_token grant fields:
    #[serde(default)]
    pub refresh_token: Option<String>,

    // Common:
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    // `scope` and `resource` are accepted but ignored — for the code grant
    // they come from the stored auth code, and for refresh they come from
    // the stored refresh token. Serde silently drops them on deserialize.
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub refresh_token: String,
    pub scope: String,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    error_description: String,
}

fn err(
    status: StatusCode,
    code: &'static str,
    desc: impl Into<String>,
) -> axum::response::Response {
    (
        status,
        Json(ErrorBody {
            error: code,
            error_description: desc.into(),
        }),
    )
        .into_response()
}

/// `POST /oauth/token` entry point. Dispatches on `grant_type`.
#[tracing::instrument(skip(state, headers, req))]
pub async fn token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(req): Form<TokenRequest>,
) -> axum::response::Response {
    match req.grant_type.as_str() {
        "authorization_code" => exchange_authorization_code(&state, &headers, req).await,
        "refresh_token" => refresh_grant(&state, &headers, req).await,
        other => err(
            StatusCode::BAD_REQUEST,
            ERR_UNSUPPORTED_GRANT_TYPE,
            format!("Unsupported grant_type: {other}"),
        ),
    }
}

/// Authorization Code grant — exchanges a PKCE-verified code for tokens.
///
/// Steps (order matters):
///   1. Validate inputs (code, redirect_uri, code_verifier all present).
///   2. **Load** the code without consuming it (`get_unused`). Validate
///      client/redirect/PKCE against the loaded row. If any check fails,
///      we never touch `used_at` — a racing attacker with wrong creds
///      can't burn the user's legitimate code.
///   3. Authenticate the client (public via PKCE only, or confidential via secret).
///   4. Verify PKCE: `BASE64URL(SHA256(code_verifier)) == stored code_challenge`.
///   5. Atomically consume the code (single-use, OAuth 2.1 §4.1.2). If
///      consume returns "already used" here, an actual replay happened
///      (the user's legit /token request already ran) — return invalid_grant.
///   6. Mint an audience-bound access JWT and a fresh refresh token.
async fn exchange_authorization_code(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    req: TokenRequest,
) -> axum::response::Response {
    let code = match req.code.as_deref() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_REQUEST,
                "code is required",
            )
        }
    };
    let redirect_uri = match req.redirect_uri.as_deref() {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_REQUEST,
                "redirect_uri is required",
            )
        }
    };
    let code_verifier = match req.code_verifier.as_deref() {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_REQUEST,
                "code_verifier is required (PKCE is mandatory in OAuth 2.1)",
            )
        }
    };

    let code_hash = hash_token(&code);
    let row = match OAuthAuthorizationCodeRepo::get_unused(&state.pool, &code_hash).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_GRANT,
                "Unknown, expired, or already-used authorization code",
            )
        }
        Err(e) => {
            tracing::error!(error = ?e, "DB error loading authorization code");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Database error",
            );
        }
    };

    // Validate before consume — failures here MUST leave the row intact
    // so a racing attacker can't burn the user's code.
    if let Err(resp) = authenticate_client(state, &req, &row.client_id).await {
        return resp;
    }
    if row.redirect_uri != redirect_uri {
        return err(
            StatusCode::BAD_REQUEST,
            ERR_INVALID_GRANT,
            "redirect_uri does not match the authorization request",
        );
    }
    if !pkce::verify_s256(&row.code_challenge, &code_verifier) {
        return err(
            StatusCode::BAD_REQUEST,
            ERR_INVALID_GRANT,
            "PKCE verification failed",
        );
    }

    // All checks passed — now consume atomically. If two legitimate
    // /token calls race (rare but possible), only one wins; the other
    // sees None here and returns invalid_grant.
    match OAuthAuthorizationCodeRepo::consume(&state.pool, &code_hash).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_GRANT,
                "Authorization code has already been used",
            )
        }
        Err(e) => {
            tracing::error!(error = ?e, "DB error consuming authorization code");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Database error",
            );
        }
    }

    issue_tokens(
        state,
        headers,
        &row.client_id,
        row.user_id,
        &row.scope,
        &row.resource,
    )
    .await
}

/// Single opaque error for any refresh-grant validity failure (unknown,
/// revoked, expired). OAuth 2.1 §5.2 specifies one error code so an
/// unauthenticated probe can't tell which internal state a token is in —
/// the differential response would otherwise give an attacker a probing
/// oracle for arbitrary refresh tokens.
const REFRESH_INVALID_MSG: &str = "refresh_token is not valid";

/// Refresh grant — rotates the refresh token and mints a new access token.
///
/// Order (matters for security):
///   1. `client_id` is required. RFC 6749 §6 says the refresh_token grant
///      MUST authenticate the client, and public clients MUST include
///      `client_id`. Requiring it up-front means an unauthenticated
///      probe fails on client-auth before any token-state check runs,
///      closing the response-differential oracle.
///   2. Authenticate the client against the supplied `client_id`.
///   3. Load the refresh token. If unknown, return one opaque error.
///   4. **Client-binding check** (RFC 6749 §6 / OAuth 2.1 §6): the
///      refresh token MUST be bound to the authenticated client. If the
///      stored `row.client_id` doesn't match, refuse — otherwise an
///      authenticated client X could refresh client Y's tokens.
///   5. If the token is revoked, this is a reuse attempt per OAuth 2.1
///      §4.13 — revoke the entire chain for (client_id, user_id), then
///      return invalid_grant. Without this, the legitimate rotated
///      token stays live and the thief still wins.
///   6. Mint the access token (can fail without committing rotation).
///   7. Rotate (atomic CAS). If rotation loses the race, the access
///      token we just minted is harmless (1h TTL, same scope/resource),
///      and the caller can retry the refresh.
async fn refresh_grant(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    req: TokenRequest,
) -> axum::response::Response {
    let refresh = match req.refresh_token.as_deref() {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_REQUEST,
                "refresh_token is required",
            )
        }
    };

    // Require client_id explicitly. Falling back to `row.client_id` when
    // client_id is missing would let a client authenticate as itself and
    // then be handed tokens bound to a different client — a client-binding
    // bypass. RFC 6749 §6 requires clients to authenticate on refresh.
    let claimed_client = match req.client_id.as_deref() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => {
            return err(
                StatusCode::UNAUTHORIZED,
                ERR_INVALID_CLIENT,
                "client_id is required for the refresh_token grant",
            )
        }
    };

    if let Err(resp) = authenticate_client(state, &req, &claimed_client).await {
        return resp;
    }

    let token_hash = hash_token(&refresh);
    let row = match OAuthRefreshTokenRepo::get(&state.pool, &token_hash).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_GRANT,
                REFRESH_INVALID_MSG,
            );
        }
        Err(e) => {
            tracing::error!(error = ?e, "DB error loading refresh token");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Database error",
            );
        }
    };

    // Client-binding enforcement: the authenticated client MUST equal
    // the client the refresh token was originally issued to. Without
    // this, client X (authenticated with its own credentials) could
    // present client Y's refresh token and receive tokens minted in
    // Y's name — a client-binding bypass. Opaque error to avoid
    // exposing which tokens exist for which clients.
    if row.client_id != claimed_client {
        tracing::warn!(
            authenticated_client = %claimed_client,
            token_client = %row.client_id,
            "Refresh token client_id mismatch — refusing cross-client refresh"
        );
        return err(
            StatusCode::BAD_REQUEST,
            ERR_INVALID_GRANT,
            REFRESH_INVALID_MSG,
        );
    }

    // Reuse detection per OAuth 2.1 §4.13 — kill the chain, not just
    // the replayed token. The legitimate client's next refresh fails
    // and re-authorizes; the attacker loses their handle too.
    if row.revoked_at.is_some() {
        tracing::warn!(
            user_id = %row.user_id, client_id = %row.client_id,
            "Refresh token reuse detected — revoking entire token chain"
        );
        if let Err(e) =
            OAuthRefreshTokenRepo::revoke_chain(&state.pool, &row.client_id, row.user_id).await
        {
            // Log but still return invalid_grant — the legitimate
            // client will fail eventually anyway and the attacker
            // already got blocked by the revoked_at check.
            tracing::error!(error = ?e, "Failed to revoke refresh-token chain after reuse");
        }
        return err(
            StatusCode::BAD_REQUEST,
            ERR_INVALID_GRANT,
            REFRESH_INVALID_MSG,
        );
    }

    if row.expires_at < chrono::Utc::now() {
        return err(
            StatusCode::BAD_REQUEST,
            ERR_INVALID_GRANT,
            REFRESH_INVALID_MSG,
        );
    }

    // Mint the access token BEFORE rotating: if mint fails (deleted user,
    // transient encoder error), the old refresh is still valid and the
    // user can retry. The old order committed rotation then failed mint,
    // leaving the user permanently locked out.
    let access_token = match mint_access_token(state, headers, &row).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    // Rotate refresh token atomically.
    let (new_refresh_raw, new_refresh_hash) = crate::auth::generate_refresh_token();
    let new_refresh_expires =
        chrono::Utc::now() + chrono::Duration::seconds(REFRESH_TOKEN_TTL_SECS);
    match OAuthRefreshTokenRepo::rotate(
        &state.pool,
        &token_hash,
        &new_refresh_hash,
        &row.client_id,
        row.user_id,
        &row.scope,
        &row.resource,
        new_refresh_expires,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            // Lost the race — another concurrent refresh rotated first.
            // Don't return the minted access token (the other rotation's
            // caller got their own pair); return invalid_grant.
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_GRANT,
                REFRESH_INVALID_MSG,
            );
        }
        Err(e) => {
            tracing::error!(error = ?e, "DB error rotating refresh token");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Database error",
            );
        }
    }

    Json(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_TTL_SECS,
        refresh_token: new_refresh_raw,
        scope: row.scope,
    })
    .into_response()
}

/// Authenticate the client per its registered method.
///
/// Public clients (PKCE-only, `client_secret_hash IS NULL`): no auth needed
/// at this endpoint — possession of the auth code + matching PKCE is the
/// proof. We just verify the `client_id` matches what was registered.
///
/// Confidential clients: `client_secret` MUST be presented (POST body for
/// `client_secret_post`).
async fn authenticate_client(
    state: &Arc<AppState>,
    req: &TokenRequest,
    expected_client_id: &str,
) -> Result<(), axum::response::Response> {
    let client = match OAuthClientRepo::get(&state.pool, expected_client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                ERR_INVALID_CLIENT,
                "Unknown client",
            ));
        }
        Err(e) => {
            tracing::error!(error = ?e, "DB error during client auth");
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Database error",
            ));
        }
    };

    // Match /oauth/authorize's expiry check: an expired DCR client
    // mustn't be able to redeem outstanding codes or rotate refresh
    // tokens after its 30-day TTL, even if the purge sweeper hasn't
    // deleted the row yet.
    if let Some(expires) = client.expires_at {
        if expires < chrono::Utc::now() {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                ERR_INVALID_CLIENT,
                "Client registration has expired",
            ));
        }
    }

    if let Some(provided_cid) = req.client_id.as_deref() {
        if provided_cid != expected_client_id {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                ERR_INVALID_CLIENT,
                "client_id mismatch",
            ));
        }
    }

    match (&client.client_secret_hash, req.client_secret.as_deref()) {
        (None, _) => Ok(()), // public client
        (Some(_), None) => Err(err(
            StatusCode::UNAUTHORIZED,
            ERR_INVALID_CLIENT,
            "client_secret is required",
        )),
        (Some(stored), Some(provided)) => {
            // Constant-time compare to avoid leaking the hash via timing.
            let provided_hash = hash_token(provided);
            use subtle::ConstantTimeEq;
            if provided_hash.as_bytes().ct_eq(stored.as_bytes()).into() {
                Ok(())
            } else {
                Err(err(
                    StatusCode::UNAUTHORIZED,
                    ERR_INVALID_CLIENT,
                    "Invalid client_secret",
                ))
            }
        }
    }
}

/// Mint an audience-bound access JWT and a fresh refresh token, then return
/// the OAuth-standard JSON token response.
async fn issue_tokens(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    client_id: &str,
    user_id: uuid::Uuid,
    scope: &str,
    resource: &str,
) -> axum::response::Response {
    let user = match stroem_db::UserRepo::get_by_id(&state.pool, user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_GRANT,
                "User no longer exists",
            )
        }
        Err(e) => {
            tracing::error!(error = ?e, "DB error loading user");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Database error",
            );
        }
    };

    let auth_config = match state.config.auth.as_ref() {
        Some(a) => a,
        None => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Auth is not configured on this server",
            )
        }
    };

    let issuer = canonical_issuer(state, headers);
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: user.user_id.to_string(),
        email: user.email,
        // Defense in depth: OAuth tokens never carry admin powers, even
        // for users who have the flag elsewhere. MCP tools authorize via
        // ACL; admin-bearing tokens would let a compromised MCP client
        // reach beyond the per-tool ACL checks (e.g. /api/users/{id}/admin)
        // if any future leak in the audience-binding gate exposed them
        // outside /mcp.
        is_admin: false,
        iat: now,
        exp: now + ACCESS_TOKEN_TTL_SECS,
        aud: Some(resource.to_string()),
        iss: Some(issuer),
        scope: Some(scope.to_string()),
        client_id: Some(client_id.to_string()),
    };

    let access_token = match jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(auth_config.jwt_secret.as_bytes()),
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to mint access token");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Failed to mint access token",
            );
        }
    };

    let (refresh_raw, refresh_hash) = crate::auth::generate_refresh_token();
    let refresh_expires = chrono::Utc::now() + chrono::Duration::seconds(REFRESH_TOKEN_TTL_SECS);
    if let Err(e) = OAuthRefreshTokenRepo::create(
        &state.pool,
        &refresh_hash,
        client_id,
        user_id,
        scope,
        resource,
        refresh_expires,
    )
    .await
    {
        tracing::error!(error = ?e, "Failed to persist refresh token");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            ERR_SERVER_ERROR,
            "Failed to persist refresh token",
        );
    }

    Json(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_TTL_SECS,
        refresh_token: refresh_raw,
        scope: scope.to_string(),
    })
    .into_response()
}

/// Build the access-token-only response for the refresh path.
async fn mint_access_token(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    refresh_row: &stroem_db::OAuthRefreshTokenRow,
) -> Result<String, axum::response::Response> {
    let user = match stroem_db::UserRepo::get_by_id(&state.pool, refresh_row.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                ERR_INVALID_GRANT,
                "User no longer exists",
            ))
        }
        Err(e) => {
            tracing::error!(error = ?e, "DB error loading user");
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Database error",
            ));
        }
    };
    let auth_config = match state.config.auth.as_ref() {
        Some(a) => a,
        None => {
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                ERR_SERVER_ERROR,
                "Auth is not configured on this server",
            ))
        }
    };
    let issuer = canonical_issuer(state, headers);
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: user.user_id.to_string(),
        email: user.email,
        // Defense in depth: OAuth tokens never carry admin powers, even
        // for users who have the flag elsewhere. MCP tools authorize via
        // ACL; admin-bearing tokens would let a compromised MCP client
        // reach beyond the per-tool ACL checks (e.g. /api/users/{id}/admin)
        // if any future leak in the audience-binding gate exposed them
        // outside /mcp.
        is_admin: false,
        iat: now,
        exp: now + ACCESS_TOKEN_TTL_SECS,
        aud: Some(refresh_row.resource.clone()),
        iss: Some(issuer),
        scope: Some(refresh_row.scope.clone()),
        client_id: Some(refresh_row.client_id.clone()),
    };

    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(auth_config.jwt_secret.as_bytes()),
    )
    .map_err(|e| {
        tracing::error!(error = ?e, "Failed to mint access token");
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            ERR_SERVER_ERROR,
            "Failed to mint access token",
        )
    })
}
