use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::error::AppError;
use crate::workspace::ReloadApiError;
use anyhow::Context;
use axum::{extract::Path, extract::State, response::IntoResponse, Json};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// Minimum interval between forced refreshes of the same workspace from the
/// public API. The watcher polls every 30–60s anyway, so 5s adds no value
/// loss while cutting the DoS amplification surface.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(5);

/// GET /api/workspaces - List all workspaces
#[tracing::instrument(skip(state, auth_user))]
pub async fn list_workspaces(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
) -> Result<impl IntoResponse, AppError> {
    let infos = state.workspaces.list_workspace_info().await;

    // When ACL is configured, filter to workspaces where user has at least one non-Deny task
    if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = auth.user_id()?;
            let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
                .await
                .context("load ACL context")?;

            if is_admin {
                return Ok(Json(infos));
            }

            // Check which workspaces have at least one visible task
            let mut visible_workspaces: HashSet<String> = HashSet::new();
            for (ws_name, ws_config) in state.workspaces.get_all_configs().await {
                for (task_name, task_def) in &ws_config.tasks {
                    let task_path = make_task_path(task_def.folder.as_deref(), task_name);
                    let perm = state.acl.evaluate(
                        &ws_name,
                        &task_path,
                        &auth.claims.email,
                        &groups,
                        false,
                    );
                    if !matches!(perm, TaskPermission::Deny) {
                        visible_workspaces.insert(ws_name.clone());
                        break;
                    }
                }
            }

            let filtered: Vec<_> = infos
                .into_iter()
                .filter(|i| visible_workspaces.contains(&i.name))
                .collect();
            return Ok(Json(filtered));
        }
    }

    Ok(Json(infos))
}

/// POST /api/workspaces/{ws}/refresh — Force-reload a workspace from its source.
///
/// Triggers an immediate git fetch (or folder re-hash) and YAML re-parse,
/// bypassing the normal poll interval. Use this from CI pipelines to ensure
/// the workspace is up-to-date before triggering a task.
///
/// **Authorization**: requires either admin or at least View permission on
/// one task in the workspace (matches the visibility rule used by
/// `GET /api/workspaces`). Callers who cannot see the workspace receive 404
/// — the same response as a non-existent workspace, to avoid leaking
/// existence + current revision via enumeration.
///
/// **Rate limit**: serialized per-workspace and rejected with 429 +
/// `Retry-After` if invoked within [`REFRESH_COOLDOWN`] of the last
/// completed refresh, or if another refresh is already in flight.
#[tracing::instrument(skip(state, auth_user))]
pub async fn refresh_workspace(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(ws): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    // ACL check: required BEFORE existence check so an unauthorized caller
    // cannot distinguish "doesn't exist" from "exists but you can't see it".
    // Both cases collapse to 404.
    if !caller_can_see_workspace(&state, auth_user.as_ref(), &ws).await? {
        return Err(AppError::NotFound(format!("Workspace '{}' not found", ws)));
    }

    match state.workspaces.reload_for_api(&ws, REFRESH_COOLDOWN).await {
        Ok(()) => {
            let revision = state.workspaces.get_revision(&ws);
            tracing::info!(workspace = %ws, revision = ?revision, "Workspace refreshed via API");
            Ok(Json(json!({
                "workspace": ws,
                "revision": revision,
                "refreshed": true,
            })))
        }
        Err(ReloadApiError::NotFound) => {
            Err(AppError::NotFound(format!("Workspace '{}' not found", ws)))
        }
        Err(ReloadApiError::Cooldown { retry_after_secs }) => Err(AppError::TooManyRequests {
            message: format!("Workspace refresh rate-limited; retry after {retry_after_secs}s"),
            retry_after_secs,
        }),
        Err(ReloadApiError::Failed(e)) => {
            Err(AppError::Internal(e.context("Failed to reload workspace")))
        }
    }
}

/// Returns true when the caller is permitted to see — and therefore act on
/// — the named workspace. Unauthenticated requests (auth disabled) and
/// admins always return true. Returns true when ACL is unconfigured (matches
/// `list_workspaces` posture).
async fn caller_can_see_workspace(
    state: &Arc<AppState>,
    auth_user: Option<&AuthUser>,
    ws: &str,
) -> Result<bool, AppError> {
    // Workspace must actually exist before we evaluate ACL — otherwise we'd
    // leak existence by returning 404 only for non-existent names.
    // Combined with the "no permission → 404" branch below, both classes
    // produce identical responses.
    if !state.workspaces.has_workspace(ws) {
        return Ok(false);
    }

    let Some(auth) = auth_user else {
        // Auth disabled → everything is open (matches the project's
        // "Auth is optional" posture).
        return Ok(true);
    };
    if !state.acl.is_configured() {
        return Ok(true);
    }

    let user_id = auth.user_id()?;
    let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
        .await
        .context("load ACL context")?;
    if is_admin {
        return Ok(true);
    }

    let Some(ws_config) = state.workspaces.get_config(ws).await else {
        // Workspace has a load error → no tasks visible by ACL anyway.
        return Ok(false);
    };

    for (task_name, task_def) in &ws_config.tasks {
        let task_path = make_task_path(task_def.folder.as_deref(), task_name);
        let perm = state
            .acl
            .evaluate(ws, &task_path, &auth.claims.email, &groups, false);
        if !matches!(perm, TaskPermission::Deny) {
            return Ok(true);
        }
    }
    Ok(false)
}
