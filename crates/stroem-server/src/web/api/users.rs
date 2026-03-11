use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::api::{default_limit, parse_uuid_param};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use stroem_db::{UserAuthLinkRepo, UserGroupRepo, UserRepo};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ListUsersQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn auth_method(has_password: bool, providers: &[String]) -> serde_json::Value {
    let mut methods = Vec::new();
    if has_password {
        methods.push("password".to_string());
    }
    methods.extend(providers.iter().cloned());
    json!(methods)
}

/// GET /api/users - List users (admin only)
#[tracing::instrument(skip(state, auth))]
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Query(query): Query<ListUsersQuery>,
) -> impl IntoResponse {
    if let Err(resp) = auth.require_admin() {
        return resp;
    }

    let users = match UserRepo::list(&state.pool, query.limit, query.offset).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("Failed to list users: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list users: {}", e)})),
            )
                .into_response();
        }
    };

    let total = match UserRepo::count(&state.pool).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to count users: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to count users: {}", e)})),
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
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list auth links: {}", e)})),
            )
                .into_response();
        }
    };

    // Batch-load groups for all users
    let all_groups = match UserGroupRepo::get_groups_for_users(&state.pool, &user_ids).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("Failed to load user groups: {:#}", e);
            vec![]
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
            let groups: Vec<String> = all_groups
                .iter()
                .filter(|g| g.user_id == u.user_id)
                .map(|g| g.group_name.clone())
                .collect();
            json!({
                "user_id": u.user_id,
                "name": u.name,
                "email": u.email,
                "is_admin": u.is_admin,
                "groups": groups,
                "auth_methods": auth_method(u.password_hash.is_some(), &providers),
                "created_at": u.created_at,
                "last_login_at": u.last_login_at,
            })
        })
        .collect();

    Json(json!({ "items": users_json, "total": total })).into_response()
}

/// GET /api/users/:id - Get user detail (admin only)
#[tracing::instrument(skip(state, auth))]
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(resp) = auth.require_admin() {
        return resp;
    }

    let user_id = match parse_uuid_param(&id, "user") {
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
                .into_response();
        }
        Err(e) => {
            tracing::error!("Failed to get user: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
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
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list auth links: {}", e)})),
            )
                .into_response();
        }
    };

    let providers: Vec<String> = auth_links.iter().map(|l| l.provider_id.clone()).collect();

    let groups: Vec<String> = match UserGroupRepo::get_groups_for_user(&state.pool, user_id).await {
        Ok(g) => g.into_iter().collect(),
        Err(e) => {
            tracing::error!("Failed to load user groups: {:#}", e);
            vec![]
        }
    };

    Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
        "is_admin": user.is_admin,
        "groups": groups,
        "auth_methods": auth_method(user.password_hash.is_some(), &providers),
        "created_at": user.created_at,
        "last_login_at": user.last_login_at,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct SetAdminRequest {
    pub is_admin: bool,
}

/// PUT /api/users/:id/admin - Set admin flag (admin only)
#[tracing::instrument(skip(state, auth, req))]
pub async fn set_user_admin(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(req): Json<SetAdminRequest>,
) -> impl IntoResponse {
    if let Err(resp) = auth.require_admin() {
        return resp;
    }

    let user_id = match parse_uuid_param(&id, "user") {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    // Prevent admin from revoking their own admin status
    if !req.is_admin {
        if let Ok(auth_uid) = auth.user_id() {
            if auth_uid == user_id {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "Cannot revoke your own admin status"})),
                )
                    .into_response();
            }
        }
    }

    // Verify user exists
    match UserRepo::get_by_id(&state.pool, user_id).await {
        Ok(Some(_)) => {}
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
    }

    match UserRepo::set_admin(&state.pool, user_id, req.is_admin).await {
        Ok(()) => Json(json!({"status": "ok"})).into_response(),
        Err(e) => {
            tracing::error!("Failed to set admin: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response()
        }
    }
}

/// GET /api/users/:id/groups - Get user groups (admin only)
#[tracing::instrument(skip(state, auth))]
pub async fn get_user_groups(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(resp) = auth.require_admin() {
        return resp;
    }

    let user_id = match parse_uuid_param(&id, "user") {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    match UserGroupRepo::get_groups_for_user(&state.pool, user_id).await {
        Ok(groups) => {
            let groups_vec: Vec<String> = groups.into_iter().collect();
            Json(json!({"groups": groups_vec})).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to get user groups: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SetGroupsRequest {
    pub groups: Vec<String>,
}

/// PUT /api/users/:id/groups - Set user groups (admin only)
#[tracing::instrument(skip(state, auth, req))]
pub async fn set_user_groups(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(req): Json<SetGroupsRequest>,
) -> impl IntoResponse {
    if let Err(resp) = auth.require_admin() {
        return resp;
    }

    let user_id = match parse_uuid_param(&id, "user") {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    // Validate group names
    for group in &req.groups {
        if group.is_empty() || group.len() > 64 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Group name must be 1-64 characters: '{}'", group)})),
            )
                .into_response();
        }
        if !group
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Group name contains invalid characters (only alphanumeric, _, - allowed): '{}'", group)})),
            )
                .into_response();
        }
    }

    // Verify user exists
    match UserRepo::get_by_id(&state.pool, user_id).await {
        Ok(Some(_)) => {}
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
    }

    match UserGroupRepo::set_groups(&state.pool, user_id, &req.groups).await {
        Ok(()) => Json(json!({"status": "ok"})).into_response(),
        Err(e) => {
            tracing::error!("Failed to set user groups: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response()
        }
    }
}

/// GET /api/groups - List all distinct group names (admin only)
#[tracing::instrument(skip(state, auth))]
pub async fn list_groups(State(state): State<Arc<AppState>>, auth: AuthUser) -> impl IntoResponse {
    if let Err(resp) = auth.require_admin() {
        return resp;
    }

    match UserGroupRepo::list_groups(&state.pool).await {
        Ok(groups) => Json(json!({"groups": groups})).into_response(),
        Err(e) => {
            tracing::error!("Failed to list groups: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Internal server error"})),
            )
                .into_response()
        }
    }
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
