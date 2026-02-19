use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use stroem_db::{UserAuthLinkRepo, UserRepo};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ListUsersQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    50
}

fn auth_method(has_password: bool, providers: &[String]) -> serde_json::Value {
    let mut methods = Vec::new();
    if has_password {
        methods.push("password".to_string());
    }
    methods.extend(providers.iter().cloned());
    json!(methods)
}

/// GET /api/users - List users
#[tracing::instrument(skip(state))]
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListUsersQuery>,
) -> impl IntoResponse {
    let users = match UserRepo::list(&state.pool, query.limit, query.offset).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("Failed to list users: {:#}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list users: {}", e)})),
            )
                .into_response();
        }
    };

    let user_ids: Vec<Uuid> = users.iter().map(|u| u.user_id).collect();
    let auth_links = match UserAuthLinkRepo::list_by_user_ids(&state.pool, &user_ids).await {
        Ok(links) => links,
        Err(e) => {
            tracing::error!("Failed to list auth links: {:#}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list auth links: {}", e)})),
            )
                .into_response();
        }
    };

    let users_json: Vec<serde_json::Value> = users
        .iter()
        .map(|u| {
            let providers: Vec<String> = auth_links
                .iter()
                .filter(|l| l.user_id == u.user_id)
                .map(|l| l.provider_id.clone())
                .collect();
            json!({
                "user_id": u.user_id,
                "name": u.name,
                "email": u.email,
                "auth_methods": auth_method(u.password_hash.is_some(), &providers),
                "created_at": u.created_at,
                "last_login_at": u.last_login_at,
            })
        })
        .collect();

    Json(users_json).into_response()
}

/// GET /api/users/:id - Get user detail
#[tracing::instrument(skip(state))]
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let user_id = match id.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid user ID"})),
            )
                .into_response();
        }
    };

    let user = match UserRepo::get_by_id(&state.pool, user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": "User not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("Failed to get user: {:#}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get user: {}", e)})),
            )
                .into_response();
        }
    };

    let auth_links = match UserAuthLinkRepo::list_by_user_ids(&state.pool, &[user_id]).await {
        Ok(links) => links,
        Err(e) => {
            tracing::error!("Failed to list auth links: {:#}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list auth links: {}", e)})),
            )
                .into_response();
        }
    };

    let providers: Vec<String> = auth_links.iter().map(|l| l.provider_id.clone()).collect();

    Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
        "auth_methods": auth_method(user.password_hash.is_some(), &providers),
        "created_at": user.created_at,
        "last_login_at": user.last_login_at,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_auth_method_password_only() {
        let result = auth_method(true, &[]);
        assert_eq!(result, json!(["password"]));
    }

    #[test]
    fn test_auth_method_oidc_only() {
        let result = auth_method(false, &["google".to_string()]);
        assert_eq!(result, json!(["google"]));
    }

    #[test]
    fn test_auth_method_both() {
        let result = auth_method(true, &["google".to_string(), "github".to_string()]);
        assert_eq!(result, json!(["password", "google", "github"]));
    }

    #[test]
    fn test_auth_method_none() {
        let result = auth_method(false, &[]);
        assert_eq!(result, json!([]));
    }
}
