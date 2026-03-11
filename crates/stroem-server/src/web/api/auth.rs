use crate::auth::{
    create_access_token, generate_refresh_token, hash_refresh_token, verify_password,
};
use crate::config::AuthConfig;
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use axum::{
    extract::State,
    http::{header, StatusCode},
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
/// Returns [`IssuedTokenPair`] on success, or a pre-built error [`Response`]
/// on failure so callers can directly `return` it.
#[allow(clippy::result_large_err)]
pub async fn issue_token_pair(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    email: &str,
    is_admin: bool,
    jwt_secret: &str,
) -> Result<IssuedTokenPair, Response> {
    let access_token = match create_access_token(&user_id.to_string(), email, is_admin, jwt_secret)
    {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to create access token: {:#}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response());
        }
    };

    let (raw_refresh, refresh_hash) = generate_refresh_token();
    let expires_at = Utc::now() + Duration::days(30);

    if let Err(e) = RefreshTokenRepo::create(pool, &refresh_hash, user_id, expires_at).await {
        tracing::error!("Failed to store refresh token: {:#}", e);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Internal server error"})),
        )
            .into_response());
    }

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
) -> impl IntoResponse {
    let auth_config = match &state.config.auth {
        Some(cfg) => cfg,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Authentication not configured"})),
            )
                .into_response()
        }
    };

    let user = match UserRepo::get_by_email(&state.pool, &req.email).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid email or password"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("DB error during login: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    };

    let password_hash = match &user.password_hash {
        Some(h) => h,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid email or password"})),
            )
                .into_response()
        }
    };

    match verify_password(&req.password, password_hash) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid email or password"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Password verification error: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    }

    if let Err(e) = UserRepo::touch_last_login(&state.pool, user.user_id).await {
        tracing::warn!("Failed to update last_login_at: {:#}", e);
    }

    match issue_token_pair(
        &state.pool,
        user.user_id,
        &user.email,
        user.is_admin,
        &auth_config.jwt_secret,
    )
    .await
    {
        Ok(tokens) => {
            let cookie = refresh_token_cookie(&tokens.raw_refresh_token, 2_592_000, auth_config);
            (
                [(header::SET_COOKIE, cookie)],
                Json(TokenResponse {
                    access_token: tokens.access_token,
                }),
            )
                .into_response()
        }
        Err(resp) => resp,
    }
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
) -> impl IntoResponse {
    let auth_config = match &state.config.auth {
        Some(cfg) => cfg,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Authentication not configured"})),
            )
                .into_response()
        }
    };

    // Prefer cookie; fall back to body token (backward compat for CLI/API clients).
    let raw_token = jar
        .get(REFRESH_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .or_else(|| req.as_ref().and_then(|r| r.refresh_token.clone()));

    let raw_token = match raw_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "No refresh token provided"})),
            )
                .into_response()
        }
    };

    let token_hash = hash_refresh_token(&raw_token);

    let token_row = match RefreshTokenRepo::get_by_hash(&state.pool, &token_hash).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid refresh token"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("DB error during refresh: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    };

    if token_row.expires_at < Utc::now() {
        let _ = RefreshTokenRepo::delete(&state.pool, &token_hash).await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Refresh token expired"})),
        )
            .into_response();
    }

    // Delete old token (rotation)
    if let Err(e) = RefreshTokenRepo::delete(&state.pool, &token_hash).await {
        tracing::error!("Failed to delete old refresh token: {:#}", e);
    }

    let user = match UserRepo::get_by_id(&state.pool, token_row.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "User not found"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("DB error looking up user: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    };

    match issue_token_pair(
        &state.pool,
        user.user_id,
        &user.email,
        user.is_admin,
        &auth_config.jwt_secret,
    )
    .await
    {
        Ok(tokens) => {
            let cookie = refresh_token_cookie(&tokens.raw_refresh_token, 2_592_000, auth_config);
            (
                [(header::SET_COOKIE, cookie)],
                Json(TokenResponse {
                    access_token: tokens.access_token,
                }),
            )
                .into_response()
        }
        Err(resp) => resp,
    }
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
) -> impl IntoResponse {
    let auth_config = state.config.auth.as_ref();

    // Prefer cookie; fall back to body token.
    let raw_token = jar
        .get(REFRESH_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .or_else(|| req.as_ref().and_then(|r| r.refresh_token.clone()));

    if let Some(raw) = raw_token.filter(|t| !t.is_empty()) {
        let token_hash = hash_refresh_token(&raw);
        if let Err(e) = RefreshTokenRepo::delete(&state.pool, &token_hash).await {
            tracing::error!("Failed to delete refresh token: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
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

    (
        [(header::SET_COOKIE, clear_cookie)],
        Json(json!({"status": "ok"})),
    )
        .into_response()
}

/// GET /api/auth/me
#[tracing::instrument(skip(state))]
pub async fn me(State(state): State<Arc<AppState>>, auth: AuthUser) -> impl IntoResponse {
    let user_id = match auth.user_id() {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let user = match UserRepo::get_by_id(&state.pool, user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "User not found"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to get user: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    };

    let groups: Vec<String> =
        match stroem_db::UserGroupRepo::get_groups_for_user(&state.pool, user.user_id).await {
            Ok(g) => g.into_iter().collect(),
            Err(e) => {
                tracing::warn!("Failed to load user groups: {:#}", e);
                vec![]
            }
        };

    Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
        "is_admin": user.is_admin,
        "groups": groups,
        "created_at": user.created_at,
    }))
    .into_response()
}
