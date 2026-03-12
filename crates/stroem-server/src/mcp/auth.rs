use crate::acl::{load_user_acl_context, make_task_path, AllowedScope, TaskPermission};
use crate::auth::{hash_api_key, validate_access_token};
use crate::state::AppState;
use std::sync::Arc;
use stroem_common::models::auth::Claims;

/// Auth context resolved from an MCP request's HTTP headers.
#[derive(Debug, Clone)]
pub struct McpAuthContext {
    pub claims: Claims,
}

/// Attempt to authenticate the MCP request.
///
/// Returns `Ok(Some(ctx))` when auth is configured and token is valid,
/// `Ok(None)` when auth is not configured (open access),
/// `Err(message)` when auth is configured but token is missing/invalid.
#[tracing::instrument(skip(state, parts))]
pub async fn authenticate(
    state: &Arc<AppState>,
    parts: &http::request::Parts,
) -> Result<Option<McpAuthContext>, String> {
    let auth_config = match &state.config.auth {
        Some(cfg) => cfg,
        None => return Ok(None),
    };

    let token = parts
        .headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token = match token {
        Some(t) => t,
        None => return Err("Missing authorization header".to_string()),
    };

    // API key path
    if token.starts_with("strm_") {
        let key_hash = hash_api_key(token);
        let key_row = stroem_db::ApiKeyRepo::get_by_hash(&state.pool, &key_hash)
            .await
            .map_err(|e| format!("DB error: {e}"))?
            .ok_or_else(|| "Invalid API key".to_string())?;

        if let Some(expires_at) = key_row.expires_at {
            if expires_at < chrono::Utc::now() {
                return Err("API key expired".to_string());
            }
        }

        let user = stroem_db::UserRepo::get_by_id(&state.pool, key_row.user_id)
            .await
            .map_err(|e| format!("DB error: {e}"))?
            .ok_or_else(|| "API key user not found".to_string())?;

        // Touch last_used_at in background
        let pool = state.pool.clone();
        let hash = key_hash;
        tokio::spawn(async move {
            let _ = stroem_db::ApiKeyRepo::touch_last_used(&pool, &hash).await;
        });

        let now = chrono::Utc::now().timestamp();
        return Ok(Some(McpAuthContext {
            claims: Claims {
                sub: user.user_id.to_string(),
                email: user.email,
                is_admin: user.is_admin,
                iat: now,
                exp: now + 3600, // Sentinel — API key expiry enforced via DB row above
            },
        }));
    }

    // JWT path
    let claims = validate_access_token(token, &auth_config.jwt_secret)
        .map_err(|_| "Invalid or expired token".to_string())?;

    Ok(Some(McpAuthContext { claims }))
}

/// Resolve the ACL permission for a task, handling the case where auth may not be configured.
#[tracing::instrument(skip(state, auth))]
pub async fn check_task_acl(
    state: &Arc<AppState>,
    auth: &Option<McpAuthContext>,
    workspace: &str,
    task_name: &str,
    folder: Option<&str>,
) -> Result<TaskPermission, String> {
    let auth = match auth {
        Some(a) => a,
        None => return Ok(TaskPermission::Run),
    };

    if !state.acl.is_configured() {
        return Ok(TaskPermission::Run);
    }

    let user_id: uuid::Uuid = auth
        .claims
        .sub
        .parse()
        .map_err(|_| "Invalid user ID in token".to_string())?;

    let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.claims.is_admin)
        .await
        .map_err(|e| format!("Failed to load ACL context: {e}"))?;

    let task_path = make_task_path(folder, task_name);
    Ok(state
        .acl
        .evaluate(workspace, &task_path, &auth.claims.email, &groups, is_admin))
}

/// Workspace, task, permission triple.
pub type AclScope = Vec<(String, String, TaskPermission)>;

/// Build the set of (workspace, task_name, permission) triples allowed for this user.
///
/// Returns `None` when no filtering is needed (admin or no ACL).
#[tracing::instrument(skip(state, auth))]
pub async fn resolve_acl_scope(
    state: &Arc<AppState>,
    auth: &Option<McpAuthContext>,
) -> Result<Option<AclScope>, String> {
    let auth = match auth {
        Some(a) => a,
        None => return Ok(None),
    };

    if !state.acl.is_configured() {
        return Ok(None);
    }

    let user_id: uuid::Uuid = auth
        .claims
        .sub
        .parse()
        .map_err(|_| "Invalid user ID in token".to_string())?;

    let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.claims.is_admin)
        .await
        .map_err(|e| format!("Failed to load ACL context: {e}"))?;

    let mut all_tasks = Vec::new();
    for (ws_name, ws_config) in state.workspaces.get_all_configs().await {
        for (task_name, task_def) in &ws_config.tasks {
            all_tasks.push((ws_name.clone(), task_name.clone(), task_def.folder.clone()));
        }
    }

    match state
        .acl
        .allowed_scope(&all_tasks, &auth.claims.email, &groups, is_admin)
    {
        AllowedScope::All => Ok(None),
        AllowedScope::Filtered(items) => Ok(Some(items)),
    }
}
