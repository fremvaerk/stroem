use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::error::AppError;
use anyhow::Context;
use axum::{extract::Path, extract::State, response::IntoResponse, Json};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;

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
#[tracing::instrument(skip(state))]
pub async fn refresh_workspace(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if !state.workspaces.has_workspace(&ws) {
        return Err(AppError::NotFound(format!("Workspace '{}' not found", ws)));
    }

    state
        .workspaces
        .reload(&ws)
        .await
        .context("Failed to reload workspace")?;

    let revision = state.workspaces.get_revision(&ws);
    tracing::info!(workspace = %ws, revision = ?revision, "Workspace refreshed via API");

    Ok(Json(json!({
        "workspace": ws,
        "revision": revision,
        "refreshed": true,
    })))
}
