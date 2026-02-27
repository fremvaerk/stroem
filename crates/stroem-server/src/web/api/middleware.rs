use crate::auth::{hash_api_key, validate_access_token};
use crate::state::AppState;
use axum::{
    extract::{FromRequestParts, OptionalFromRequestParts},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use stroem_common::models::auth::Claims;
use stroem_db::{ApiKeyRepo, UserRepo};

/// Extractor that validates a JWT Bearer token or API key and provides the claims.
/// Use `Option<AuthUser>` for optional auth — returns `None` when auth is
/// not configured or token is missing/invalid.
/// Use `AuthUser` directly for required auth (rejects with 401).
#[derive(Debug)]
pub struct AuthUser {
    pub claims: Claims,
    /// True when the request was authenticated via an API key (not a JWT).
    pub is_api_key: bool,
}

impl AuthUser {
    /// Parse the `sub` claim as a [`uuid::Uuid`].
    ///
    /// Returns an `Err` with a pre-built 500 response when the claim is malformed.
    /// Callers should early-return the error response directly:
    ///
    /// ```ignore
    /// let user_id = match auth.user_id() {
    ///     Ok(id) => id,
    ///     Err(resp) => return resp,
    /// };
    /// ```
    #[allow(clippy::result_large_err)]
    pub fn user_id(&self) -> Result<uuid::Uuid, Response> {
        self.claims.sub.parse::<uuid::Uuid>().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Invalid user ID in token"})),
            )
                .into_response()
        })
    }
}

/// Validate an API key token and return Claims if valid.
pub(super) async fn validate_api_key(
    token: &str,
    state: &Arc<AppState>,
) -> Result<Claims, Response> {
    let key_hash = hash_api_key(token);

    let key_row = match ApiKeyRepo::get_by_hash(&state.pool, &key_hash).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid API key"})),
            )
                .into_response())
        }
        Err(e) => {
            tracing::error!("DB error validating API key: {:#}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response());
        }
    };

    // Check expiry
    if let Some(expires_at) = key_row.expires_at {
        if expires_at < chrono::Utc::now() {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "API key expired"})),
            )
                .into_response());
        }
    }

    // Load the user
    let user = match UserRepo::get_by_id(&state.pool, key_row.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "API key user not found"})),
            )
                .into_response())
        }
        Err(e) => {
            tracing::error!("DB error loading API key user: {:#}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response());
        }
    };

    // Update last_used_at in the background (don't block the request)
    let pool = state.pool.clone();
    let hash = key_hash.clone();
    tokio::spawn(async move {
        if let Err(e) = ApiKeyRepo::touch_last_used(&pool, &hash).await {
            tracing::warn!("Failed to update API key last_used_at: {:#}", e);
        }
    });

    Ok(Claims {
        sub: user.user_id.to_string(),
        email: user.email,
        iat: chrono::Utc::now().timestamp(),
        exp: chrono::Utc::now().timestamp() + 3600, // synthetic expiry for Claims struct
    })
}

impl FromRequestParts<Arc<AppState>> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let auth_config = match &state.config.auth {
            Some(cfg) => cfg,
            None => {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "Authentication not configured"})),
                )
                    .into_response())
            }
        };

        let auth_header = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());

        let token = match auth_header {
            Some(val) => match val.strip_prefix("Bearer ") {
                Some(t) => t,
                None => {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "Invalid authorization header format"})),
                    )
                        .into_response())
                }
            },
            None => {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "Missing authorization header"})),
                )
                    .into_response())
            }
        };

        // API key path: starts with "strm_"
        if token.starts_with("strm_") {
            return validate_api_key(token, state).await.map(|claims| AuthUser {
                claims,
                is_api_key: true,
            });
        }

        // JWT path
        match validate_access_token(token, &auth_config.jwt_secret) {
            Ok(claims) => Ok(AuthUser {
                claims,
                is_api_key: false,
            }),
            Err(_) => Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid or expired token"})),
            )
                .into_response()),
        }
    }
}

impl OptionalFromRequestParts<Arc<AppState>> for AuthUser {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Option<Self>, Self::Rejection> {
        let auth_config = match &state.config.auth {
            Some(cfg) => cfg,
            None => return Ok(None),
        };

        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        match token {
            Some(t) if t.starts_with("strm_") => match validate_api_key(t, state).await {
                Ok(claims) => Ok(Some(AuthUser {
                    claims,
                    is_api_key: true,
                })),
                Err(_) => Ok(None),
            },
            Some(t) => match validate_access_token(t, &auth_config.jwt_secret) {
                Ok(claims) => Ok(Some(AuthUser {
                    claims,
                    is_api_key: false,
                })),
                Err(_) => Ok(None),
            },
            None => Ok(None),
        }
    }
}
