use crate::auth::{
    create_access_token, generate_refresh_token, hash_refresh_token, verify_password,
};
use crate::config::AuthConfig;
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    Json,
};
use axum_extra::extract::CookieJar;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use stroem_db::{RefreshTokenRepo, UserRepo};

const REFRESH_COOKIE_NAME: &str = "stroem_refresh";

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    /// Accepted for backward compatibility (CLI / API clients that cannot use cookies).
    pub refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LogoutRequest {
    /// Accepted for backward compatibility (CLI / API clients that cannot use cookies).
    pub refresh_token: Option<String>,
}

/// Build the `Set-Cookie` header value for the refresh token cookie.
///
/// Pass `max_age_secs = 0` and an empty `token` to produce a clearing cookie.
pub fn refresh_token_cookie(token: &str, max_age_secs: i64, auth_config: &AuthConfig) -> String {
    let secure = if auth_config
        .base_url
        .as_deref()
        .is_some_and(|u| u.starts_with("https://"))
    {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/api/auth; Max-Age={}{}",
        REFRESH_COOKIE_NAME, token, max_age_secs, secure
    )
}

/// Issued token pair returned from [`issue_token_pair`].
///
/// Contains the raw access token (for the JSON response) and the raw refresh
/// token (for the `Set-Cookie` header).  The refresh token is **not** included
/// in the JSON response body; callers must set it as an HttpOnly cookie.
pub struct IssuedTokenPair {
    pub access_token: String,
    pub raw_refresh_token: String,
}

/// Create an access token + refresh token pair and persist the refresh token.
///
/// Returns [`IssuedTokenPair`] on success, or an [`AppError`] on failure.
pub async fn issue_token_pair(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    email: &str,
    is_admin: bool,
    jwt_secret: &str,
) -> Result<IssuedTokenPair, AppError> {
    let access_token = create_access_token(&user_id.to_string(), email, is_admin, jwt_secret)
        .context("create access token")?;

    let (raw_refresh, refresh_hash) = generate_refresh_token();
    let expires_at = Utc::now() + Duration::days(30);

    RefreshTokenRepo::create(pool, &refresh_hash, user_id, expires_at)
        .await
        .context("store refresh token")?;

    Ok(IssuedTokenPair {
        access_token,
        raw_refresh_token: raw_refresh,
    })
}

/// POST /api/auth/login
#[tracing::instrument(skip(state, req))]
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Response, AppError> {
    let auth_config = state
        .config
        .auth
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("Authentication not configured".into()))?;

    let user = UserRepo::get_by_email(&state.pool, &req.email)
        .await
        .context("DB error during login")?
        .ok_or_else(|| AppError::Unauthorized("Invalid email or password".into()))?;

    let password_hash = user
        .password_hash
        .as_ref()
        .ok_or_else(|| AppError::Unauthorized("Invalid email or password".into()))?;

    let password_ok =
        verify_password(&req.password, password_hash).context("password verification")?;

    if !password_ok {
        return Err(AppError::Unauthorized("Invalid email or password".into()));
    }

    if let Err(e) = UserRepo::touch_last_login(&state.pool, user.user_id).await {
        tracing::warn!("Failed to update last_login_at: {:#}", e);
    }

    let tokens = issue_token_pair(
        &state.pool,
        user.user_id,
        &user.email,
        user.is_admin,
        &auth_config.jwt_secret,
    )
    .await?;

    let cookie = refresh_token_cookie(&tokens.raw_refresh_token, 2_592_000, auth_config);
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(TokenResponse {
            access_token: tokens.access_token,
        }),
    )
        .into_response())
}

/// POST /api/auth/refresh
///
/// The refresh token is read from the `stroem_refresh` HttpOnly cookie.
/// For backward compatibility, the token may also be supplied in the JSON
/// body (`refresh_token` field) — this path is retained for CLI / API clients
/// that cannot use browser cookies.
#[tracing::instrument(skip(state, jar, req))]
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    req: Option<Json<RefreshRequest>>,
) -> Result<Response, AppError> {
    let auth_config = state
        .config
        .auth
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("Authentication not configured".into()))?;

    // Prefer cookie; fall back to body token (backward compat for CLI/API clients).
    let raw_token = jar
        .get(REFRESH_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .or_else(|| req.as_ref().and_then(|r| r.refresh_token.clone()));

    let raw_token = match raw_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return Err(AppError::Unauthorized("No refresh token provided".into()));
        }
    };

    let token_hash = hash_refresh_token(&raw_token);

    let token_row = RefreshTokenRepo::get_by_hash(&state.pool, &token_hash)
        .await
        .context("DB error during refresh")?
        .ok_or_else(|| AppError::Unauthorized("Invalid refresh token".into()))?;

    if token_row.expires_at < Utc::now() {
        let _ = RefreshTokenRepo::delete(&state.pool, &token_hash).await;
        return Err(AppError::Unauthorized("Refresh token expired".into()));
    }

    // Delete old token (rotation)
    if let Err(e) = RefreshTokenRepo::delete(&state.pool, &token_hash).await {
        tracing::error!("Failed to delete old refresh token: {:#}", e);
    }

    let user = UserRepo::get_by_id(&state.pool, token_row.user_id)
        .await
        .context("DB error looking up user")?
        .ok_or_else(|| AppError::Unauthorized("User not found".into()))?;

    let tokens = issue_token_pair(
        &state.pool,
        user.user_id,
        &user.email,
        user.is_admin,
        &auth_config.jwt_secret,
    )
    .await?;

    let cookie = refresh_token_cookie(&tokens.raw_refresh_token, 2_592_000, auth_config);
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(TokenResponse {
            access_token: tokens.access_token,
        }),
    )
        .into_response())
}

/// POST /api/auth/logout
///
/// The refresh token is read from the `stroem_refresh` HttpOnly cookie.
/// For backward compatibility, the token may also be supplied in the JSON
/// body (`refresh_token` field).  The cookie is always cleared in the response
/// regardless of how the token was supplied.
#[tracing::instrument(skip(state, jar, req))]
pub async fn logout(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    req: Option<Json<LogoutRequest>>,
) -> Result<Response, AppError> {
    let auth_config = state.config.auth.as_ref();

    // Prefer cookie; fall back to body token.
    let raw_token = jar
        .get(REFRESH_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .or_else(|| req.as_ref().and_then(|r| r.refresh_token.clone()));

    if let Some(raw) = raw_token.filter(|t| !t.is_empty()) {
        let token_hash = hash_refresh_token(&raw);
        RefreshTokenRepo::delete(&state.pool, &token_hash)
            .await
            .context("delete refresh token")?;
    }

    // Clear the cookie in the response (Max-Age=0), respecting the Secure flag.
    let clear_cookie = auth_config
        .map(|cfg| refresh_token_cookie("", 0, cfg))
        .unwrap_or_else(|| {
            format!(
                "{}=; HttpOnly; SameSite=Strict; Path=/api/auth; Max-Age=0",
                REFRESH_COOKIE_NAME
            )
        });

    Ok((
        [(header::SET_COOKIE, clear_cookie)],
        Json(json!({"status": "ok"})),
    )
        .into_response())
}

/// GET /api/auth/me
#[tracing::instrument(skip(state))]
pub async fn me(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<impl IntoResponse, AppError> {
    let user_id = auth.user_id()?;

    let user = UserRepo::get_by_id(&state.pool, user_id)
        .await
        .context("get user")?
        .ok_or_else(|| AppError::not_found("User"))?;

    let groups: Vec<String> =
        match stroem_db::UserGroupRepo::get_groups_for_user(&state.pool, user.user_id).await {
            Ok(g) => g.into_iter().collect(),
            Err(e) => {
                tracing::warn!("Failed to load user groups: {:#}", e);
                vec![]
            }
        };

    Ok(Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
        "is_admin": user.is_admin,
        "groups": groups,
        "created_at": user.created_at,
    })))
}
