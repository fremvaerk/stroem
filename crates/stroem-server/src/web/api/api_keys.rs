use crate::auth::generate_api_key;
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    extract::{Path, State},
    response::IntoResponse,
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
fn require_jwt(auth: &AuthUser) -> Result<(), AppError> {
    if auth.is_api_key {
        Err(AppError::Forbidden(
            "API key management requires JWT authentication".into(),
        ))
    } else {
        Ok(())
    }
}

/// POST /api/auth/api-keys — Create a new API key
#[tracing::instrument(skip(state, auth))]
pub async fn create_api_key(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_jwt(&auth)?;

    if req.name.trim().is_empty() {
        return Err(AppError::BadRequest("Name is required".into()));
    }

    if let Some(days) = req.expires_in_days {
        if days <= 0 {
            return Err(AppError::BadRequest(
                "expires_in_days must be positive".into(),
            ));
        }
    }

    let user_id = auth.user_id()?;

    let (raw_key, key_hash) = generate_api_key();
    let prefix = raw_key[..12].to_string(); // "strm_a1b2c3d"

    let expires_at = req
        .expires_in_days
        .map(|days| Utc::now() + Duration::days(days));

    ApiKeyRepo::create(
        &state.pool,
        &key_hash,
        user_id,
        req.name.trim(),
        &prefix,
        expires_at,
    )
    .await
    .context("create api key")?;

    Ok(Json(CreateApiKeyResponse {
        key: raw_key,
        name: req.name.trim().to_string(),
        prefix,
        expires_at: expires_at.map(|t| t.to_rfc3339()),
    }))
}

/// GET /api/auth/api-keys — List current user's API keys
#[tracing::instrument(skip(state, auth))]
pub async fn list_api_keys(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<impl IntoResponse, AppError> {
    require_jwt(&auth)?;

    let user_id = auth.user_id()?;

    let keys = ApiKeyRepo::list_by_user(&state.pool, user_id)
        .await
        .context("list api keys")?;
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

    Ok(Json(infos))
}

/// DELETE /api/auth/api-keys/{prefix} — Revoke an API key by prefix
#[tracing::instrument(skip(state, auth))]
pub async fn delete_api_key(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(prefix): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    require_jwt(&auth)?;

    let user_id = auth.user_id()?;

    let deleted = ApiKeyRepo::delete_by_prefix_and_user(&state.pool, &prefix, user_id)
        .await
        .context("delete api key")?;

    if deleted {
        Ok(Json(json!({"status": "ok"})))
    } else {
        Err(AppError::not_found("API key"))
    }
}
