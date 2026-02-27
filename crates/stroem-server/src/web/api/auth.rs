use crate::auth::{
    create_access_token, generate_refresh_token, hash_refresh_token, verify_password,
};
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use stroem_db::{RefreshTokenRepo, UserRepo};

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct LogoutRequest {
    pub refresh_token: String,
}

/// Create an access token + refresh token pair and persist the refresh token.
///
/// Returns a ready-to-send [`TokenResponse`] on success, or a pre-built error
/// [`Response`] on failure so callers can directly `return` it.
#[allow(clippy::result_large_err)]
pub async fn issue_token_pair(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    email: &str,
    jwt_secret: &str,
) -> Result<TokenResponse, Response> {
    let access_token = match create_access_token(&user_id.to_string(), email, jwt_secret) {
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

    Ok(TokenResponse {
        access_token,
        refresh_token: raw_refresh,
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
        &auth_config.jwt_secret,
    )
    .await
    {
        Ok(tokens) => Json(tokens).into_response(),
        Err(resp) => resp,
    }
}

/// POST /api/auth/refresh
#[tracing::instrument(skip(state, req))]
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefreshRequest>,
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

    let token_hash = hash_refresh_token(&req.refresh_token);

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
        &auth_config.jwt_secret,
    )
    .await
    {
        Ok(tokens) => Json(tokens).into_response(),
        Err(resp) => resp,
    }
}

/// POST /api/auth/logout
#[tracing::instrument(skip(state, req))]
pub async fn logout(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LogoutRequest>,
) -> impl IntoResponse {
    let token_hash = hash_refresh_token(&req.refresh_token);

    if let Err(e) = RefreshTokenRepo::delete(&state.pool, &token_hash).await {
        tracing::error!("Failed to delete refresh token: {:#}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Internal server error"})),
        )
            .into_response();
    }

    Json(json!({"status": "ok"})).into_response()
}

/// GET /api/auth/me
#[tracing::instrument(skip(state))]
pub async fn me(State(state): State<Arc<AppState>>, auth: AuthUser) -> impl IntoResponse {
    let user_id = match auth.user_id() {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    match UserRepo::get_by_id(&state.pool, user_id).await {
        Ok(Some(user)) => Json(json!({
            "user_id": user.user_id,
            "name": user.name,
            "email": user.email,
            "created_at": user.created_at,
        }))
        .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "User not found"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to get user: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response()
        }
    }
}
