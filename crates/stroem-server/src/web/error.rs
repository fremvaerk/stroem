use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Centralized API error type. Internal details are logged via tracing
/// and never sent to the client.
#[derive(Debug)]
pub enum AppError {
    /// 400 Bad Request
    BadRequest(String),
    /// 401 Unauthorized
    Unauthorized(String),
    /// 403 Forbidden
    Forbidden(String),
    /// 404 Not Found
    NotFound(String),
    /// 409 Conflict
    Conflict(String),
    /// 500 Internal Server Error — wraps anyhow::Error.
    /// Full error logged server-side; client sees "Internal server error".
    Internal(anyhow::Error),
}

impl AppError {
    /// Convenience constructor for a standard "X not found" 404 response.
    pub fn not_found(entity: &str) -> Self {
        Self::NotFound(format!("{} not found", entity))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            Self::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
            Self::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            Self::Conflict(msg) => (StatusCode::CONFLICT, msg),
            Self::Internal(err) => {
                tracing::error!("Internal error: {:#}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".into(),
                )
            }
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        Self::Internal(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn test_bad_request_returns_400() {
        let err = AppError::BadRequest("invalid input".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "invalid input");
    }

    #[tokio::test]
    async fn test_unauthorized_returns_401() {
        let err = AppError::Unauthorized("missing token".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "missing token");
    }

    #[tokio::test]
    async fn test_forbidden_returns_403() {
        let err = AppError::Forbidden("Admin access required".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Admin access required");
    }

    #[tokio::test]
    async fn test_not_found_returns_404() {
        let err = AppError::NotFound("resource missing".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "resource missing");
    }

    #[tokio::test]
    async fn test_conflict_returns_409() {
        let err = AppError::Conflict("already exists".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "already exists");
    }

    #[tokio::test]
    async fn test_internal_sanitizes_message() {
        let err = AppError::Internal(anyhow::anyhow!("connection refused at 10.0.0.1:5432"));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Internal server error");
        // Internal details must NOT appear in the response
        let body_str = String::from_utf8_lossy(&body);
        assert!(!body_str.contains("connection refused"));
        assert!(!body_str.contains("10.0.0.1"));
    }

    #[tokio::test]
    async fn test_not_found_helper() {
        let err = AppError::not_found("Job");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Job not found");
    }

    #[tokio::test]
    async fn test_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("some internal failure");
        let err: AppError = anyhow_err.into();
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Internal server error");
    }

    #[tokio::test]
    async fn test_from_sqlx_error() {
        // Use a decode error (constructable without a real DB connection)
        let sqlx_err = sqlx::Error::RowNotFound;
        let err: AppError = sqlx_err.into();
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Internal server error");
    }

    #[tokio::test]
    async fn test_response_json_shape() {
        // Verify the JSON shape is always {"error": "..."} — no extra fields
        let err = AppError::BadRequest("test".into());
        let response = err.into_response();
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 1, "response must have exactly one key");
        assert!(obj.contains_key("error"));
    }
}
