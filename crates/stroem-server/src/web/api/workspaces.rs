use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use axum::{extract::State, response::IntoResponse, Json};
use std::collections::HashSet;
use std::sync::Arc;

/// GET /api/workspaces - List all workspaces
#[tracing::instrument(skip(state, auth_user))]
pub async fn list_workspaces(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
) -> impl IntoResponse {
    let infos = state.workspaces.list_workspace_info().await;

    // When ACL is configured, filter to workspaces where user has at least one non-Deny task
    if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = match auth.user_id() {
                Ok(id) => id,
                Err(resp) => return resp.into_response(),
            };
            let (is_admin, groups) =
                match load_user_acl_context(&state.pool, user_id, auth.is_admin()).await {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        tracing::error!("Failed to load ACL context: {:#}", e);
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error": "Internal server error"})),
                        )
                            .into_response();
                    }
                };
            if is_admin {
                return Json(infos).into_response();
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
            return Json(filtered).into_response();
        }
    }

    Json(infos).into_response()
}
