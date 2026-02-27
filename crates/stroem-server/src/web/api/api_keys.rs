use crate::auth::generate_api_key;
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use stroem_db::ApiKeyRepo;

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub expires_in_days: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CreateApiKeyResponse {
    pub key: String,
    pub name: String,
    pub prefix: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiKeyInfo {
    pub prefix: String,
    pub name: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// Reject requests authenticated via API key (require JWT for key management).
fn require_jwt(auth: &AuthUser) -> Option<Response> {
    if auth.is_api_key {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "API key management requires JWT authentication"})),
            )
                .into_response(),
        )
    } else {
        None
    }
}

/// POST /api/auth/api-keys — Create a new API key
#[tracing::instrument(skip(state, auth))]
pub async fn create_api_key(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    if let Some(resp) = require_jwt(&auth) {
        return resp;
    }

    if req.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Name is required"})),
        )
            .into_response();
    }

    if let Some(days) = req.expires_in_days {
        if days <= 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "expires_in_days must be positive"})),
            )
                .into_response();
        }
    }

    let user_id = match auth.user_id() {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let (raw_key, key_hash) = generate_api_key();
    let prefix = raw_key[..12].to_string(); // "strm_a1b2c3d"

    let expires_at = req
        .expires_in_days
        .map(|days| Utc::now() + Duration::days(days));

    if let Err(e) = ApiKeyRepo::create(
        &state.pool,
        &key_hash,
        user_id,
        req.name.trim(),
        &prefix,
        expires_at,
    )
    .await
    {
        tracing::error!("Failed to create API key: {:#}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Internal server error"})),
        )
            .into_response();
    }

    Json(CreateApiKeyResponse {
        key: raw_key,
        name: req.name.trim().to_string(),
        prefix,
        expires_at: expires_at.map(|t| t.to_rfc3339()),
    })
    .into_response()
}

/// GET /api/auth/api-keys — List current user's API keys
#[tracing::instrument(skip(state, auth))]
pub async fn list_api_keys(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> impl IntoResponse {
    if let Some(resp) = require_jwt(&auth) {
        return resp;
    }

    let user_id = match auth.user_id() {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    match ApiKeyRepo::list_by_user(&state.pool, user_id).await {
        Ok(keys) => {
            let infos: Vec<ApiKeyInfo> = keys
                .into_iter()
                .map(|k| ApiKeyInfo {
                    prefix: k.prefix,
                    name: k.name,
                    created_at: k.created_at.to_rfc3339(),
                    expires_at: k.expires_at.map(|t| t.to_rfc3339()),
                    last_used_at: k.last_used_at.map(|t| t.to_rfc3339()),
                })
                .collect();
            Json(infos).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to list API keys: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response()
        }
    }
}

/// DELETE /api/auth/api-keys/{prefix} — Revoke an API key by prefix
#[tracing::instrument(skip(state, auth))]
pub async fn delete_api_key(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(prefix): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = require_jwt(&auth) {
        return resp;
    }

    let user_id = match auth.user_id() {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    match ApiKeyRepo::delete_by_prefix_and_user(&state.pool, &prefix, user_id).await {
        Ok(true) => Json(json!({"status": "ok"})).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "API key not found"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to delete API key: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response()
        }
    }
}
