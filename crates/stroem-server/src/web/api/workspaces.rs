use crate::state::AppState;
use axum::{extract::State, response::IntoResponse, Json};
use std::sync::Arc;

/// GET /api/workspaces - List all workspaces
#[tracing::instrument(skip(state))]
pub async fn list_workspaces(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let infos = state.workspaces.list_workspace_info().await;
    Json(infos)
}
