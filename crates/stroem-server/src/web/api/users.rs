use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::api::{default_limit, parse_uuid_param};
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
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
) -> Result<impl IntoResponse, AppError> {
    auth.require_admin()?;

    let users = UserRepo::list(&state.pool, query.limit, query.offset)
        .await
        .context("list users")?;

    let total = UserRepo::count(&state.pool).await.context("count users")?;

    let user_ids: Vec<Uuid> = users.iter().map(|u| u.user_id).collect();
    let auth_links = UserAuthLinkRepo::list_by_user_ids(&state.pool, &user_ids)
        .await
        .context("list auth links")?;

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

    Ok(Json(json!({ "items": users_json, "total": total })))
}

/// GET /api/users/:id - Get user detail (admin only)
#[tracing::instrument(skip(state, auth))]
pub async fn get_user(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    auth.require_admin()?;

    let user_id = parse_uuid_param(&id, "user")?;

    let user = UserRepo::get_by_id(&state.pool, user_id)
        .await
        .context("get user")?
        .ok_or_else(|| AppError::not_found("User"))?;

    let auth_links = UserAuthLinkRepo::list_by_user_ids(&state.pool, &[user_id])
        .await
        .context("list auth links")?;

    let providers: Vec<String> = auth_links.iter().map(|l| l.provider_id.clone()).collect();

    let groups: Vec<String> = match UserGroupRepo::get_groups_for_user(&state.pool, user_id).await {
        Ok(g) => g.into_iter().collect(),
        Err(e) => {
            tracing::warn!("Failed to load user groups: {:#}", e);
            vec![]
        }
    };

    Ok(Json(json!({
        "user_id": user.user_id,
        "name": user.name,
        "email": user.email,
        "is_admin": user.is_admin,
        "groups": groups,
        "auth_methods": auth_method(user.password_hash.is_some(), &providers),
        "created_at": user.created_at,
        "last_login_at": user.last_login_at,
    })))
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
) -> Result<impl IntoResponse, AppError> {
    auth.require_admin()?;

    let user_id = parse_uuid_param(&id, "user")?;

    // Prevent admin from revoking their own admin status
    if !req.is_admin {
        if let Ok(auth_uid) = auth.user_id() {
            if auth_uid == user_id {
                return Err(AppError::BadRequest(
                    "Cannot revoke your own admin status".into(),
                ));
            }
        }
    }

    // Verify user exists
    UserRepo::get_by_id(&state.pool, user_id)
        .await
        .context("get user")?
        .ok_or_else(|| AppError::not_found("User"))?;

    UserRepo::set_admin(&state.pool, user_id, req.is_admin)
        .await
        .context("set admin")?;

    Ok(Json(json!({"status": "ok"})))
}

/// GET /api/users/:id/groups - Get user groups (admin only)
#[tracing::instrument(skip(state, auth))]
pub async fn get_user_groups(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    auth.require_admin()?;

    let user_id = parse_uuid_param(&id, "user")?;

    let groups = UserGroupRepo::get_groups_for_user(&state.pool, user_id)
        .await
        .context("get user groups")?;

    let groups_vec: Vec<String> = groups.into_iter().collect();
    Ok(Json(json!({"groups": groups_vec})))
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
) -> Result<impl IntoResponse, AppError> {
    auth.require_admin()?;

    let user_id = parse_uuid_param(&id, "user")?;

    // Validate group names
    for group in &req.groups {
        if group.is_empty() || group.len() > 64 {
            return Err(AppError::BadRequest(format!(
                "Group name must be 1-64 characters: '{}'",
                group
            )));
        }
        if !group
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(AppError::BadRequest(format!(
                "Group name contains invalid characters (only alphanumeric, _, - allowed): '{}'",
                group
            )));
        }
    }

    // Verify user exists
    UserRepo::get_by_id(&state.pool, user_id)
        .await
        .context("get user")?
        .ok_or_else(|| AppError::not_found("User"))?;

    UserGroupRepo::set_groups(&state.pool, user_id, &req.groups)
        .await
        .context("set user groups")?;

    Ok(Json(json!({"status": "ok"})))
}

/// GET /api/groups - List all distinct group names (admin only)
#[tracing::instrument(skip(state, auth))]
pub async fn list_groups(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<impl IntoResponse, AppError> {
    auth.require_admin()?;

    let groups = UserGroupRepo::list_groups(&state.pool)
        .await
        .context("list groups")?;

    Ok(Json(json!({"groups": groups})))
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
