use crate::auth::{
    create_access_token, generate_refresh_token, hash_refresh_token, verify_password,
};
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use stroem_db::{RefreshTokenRepo, UserRepo};
use uuid::Uuid;

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
            tracing::error!("DB error during login: {}", e);
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
            tracing::error!("Password verification error: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    }

    let access_token = match create_access_token(
        &user.user_id.to_string(),
        &user.email,
        &auth_config.jwt_secret,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to create access token: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    };

    let (raw_refresh, refresh_hash) = generate_refresh_token();
    let expires_at = Utc::now() + Duration::days(30);

    if let Err(e) =
        RefreshTokenRepo::create(&state.pool, &refresh_hash, user.user_id, expires_at).await
    {
        tracing::error!("Failed to store refresh token: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Internal server error"})),
        )
            .into_response();
    }

    Json(TokenResponse {
        access_token,
        refresh_token: raw_refresh,
    })
    .into_response()
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
            tracing::error!("DB error during refresh: {}", e);
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
        tracing::error!("Failed to delete old refresh token: {}", e);
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
            tracing::error!("DB error looking up user: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    };

    let access_token = match create_access_token(
        &user.user_id.to_string(),
        &user.email,
        &auth_config.jwt_secret,
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to create access token: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response();
        }
    };

    let (raw_refresh, refresh_hash) = generate_refresh_token();
    let expires_at = Utc::now() + Duration::days(30);

    if let Err(e) =
        RefreshTokenRepo::create(&state.pool, &refresh_hash, user.user_id, expires_at).await
    {
        tracing::error!("Failed to store new refresh token: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Internal server error"})),
        )
            .into_response();
    }

    Json(TokenResponse {
        access_token,
        refresh_token: raw_refresh,
    })
    .into_response()
}

/// POST /api/auth/logout
#[tracing::instrument(skip(state, req))]
pub async fn logout(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LogoutRequest>,
) -> impl IntoResponse {
    let token_hash = hash_refresh_token(&req.refresh_token);

    if let Err(e) = RefreshTokenRepo::delete(&state.pool, &token_hash).await {
        tracing::error!("Failed to delete refresh token: {}", e);
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
    let user_id: Uuid = match auth.0.sub.parse() {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Invalid user ID in token"})),
            )
                .into_response()
        }
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
            tracing::error!("Failed to get user: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response()
        }
    }
}
