use crate::auth::validate_access_token;
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

/// Extractor that validates a JWT Bearer token and provides the claims.
/// Use `Option<AuthUser>` for optional auth — returns `None` when auth is
/// not configured or token is missing/invalid.
/// Use `AuthUser` directly for required auth (rejects with 401).
#[derive(Debug)]
pub struct AuthUser(pub Claims);

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

        match validate_access_token(token, &auth_config.jwt_secret) {
            Ok(claims) => Ok(AuthUser(claims)),
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
            Some(t) => match validate_access_token(t, &auth_config.jwt_secret) {
                Ok(claims) => Ok(Some(AuthUser(claims))),
                Err(_) => Ok(None),
            },
            None => Ok(None),
        }
    }
}
