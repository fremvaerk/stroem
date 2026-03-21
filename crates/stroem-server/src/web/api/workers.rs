use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
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
use stroem_db::{JobStepRepo, WorkerRepo};

#[derive(Debug, Deserialize)]
pub struct ListWorkersQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// GET /api/workers - List registered workers
#[tracing::instrument(skip(state))]
pub async fn list_workers(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListWorkersQuery>,
) -> Result<impl IntoResponse, AppError> {
    let workers = WorkerRepo::list(&state.pool, query.limit, query.offset)
        .await
        .context("list workers")?;
    let total = WorkerRepo::count(&state.pool)
        .await
        .context("count workers")?;

    let workers_json: Vec<serde_json::Value> = workers
        .iter()
        .map(|w| {
            json!({
                "worker_id": w.worker_id,
                "name": w.name,
                "status": w.status,
                "tags": w.tags,
                "version": w.version,
                "last_heartbeat": w.last_heartbeat,
                "registered_at": w.registered_at,
            })
        })
        .collect();

    Ok(Json(json!({ "items": workers_json, "total": total })))
}

/// GET /api/workers/:id - Get worker detail with recent steps
#[tracing::instrument(skip(state))]
pub async fn get_worker(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let worker_id = parse_uuid_param(&id, "worker")?;

    let worker = WorkerRepo::get(&state.pool, worker_id)
        .await
        .context("get worker")?
        .ok_or_else(|| AppError::not_found("Worker"))?;

    let steps = JobStepRepo::list_by_worker(&state.pool, worker_id, 50, 0)
        .await
        .context("list steps for worker")?;

    // ACL filter: remove steps for tasks the user can't see
    let steps: Vec<_> = if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = match auth.user_id() {
                Ok(id) => id,
                Err(_) => {
                    // Couldn't parse user_id — return worker info with no steps for safety
                    return Ok(Json(json!({
                        "worker_id": worker.worker_id,
                        "name": worker.name,
                        "status": worker.status,
                        "tags": worker.tags,
                        "version": worker.version,
                        "last_heartbeat": worker.last_heartbeat,
                        "registered_at": worker.registered_at,
                        "steps": { "items": [], "total": 0i64 },
                    })));
                }
            };
            match load_user_acl_context(&state.pool, user_id, auth.is_admin()).await {
                Ok((true, _)) => steps, // admin sees all
                Ok((false, groups)) => {
                    let all_configs = state.workspaces.get_all_configs().await;
                    steps
                        .into_iter()
                        .filter(|s| {
                            let folder = all_configs
                                .iter()
                                .find(|(ws_name, _)| ws_name == &s.workspace)
                                .and_then(|(_, ws)| {
                                    ws.tasks.get(&s.task_name).and_then(|t| t.folder.clone())
                                });
                            let task_path = make_task_path(folder.as_deref(), &s.task_name);
                            let perm = state.acl.evaluate(
                                &s.workspace,
                                &task_path,
                                &auth.claims.email,
                                &groups,
                                false,
                            );
                            !matches!(perm, TaskPermission::Deny)
                        })
                        .collect()
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to load ACL context for worker detail");
                    vec![]
                }
            }
        } else {
            steps
        }
    } else {
        steps
    };

    let total = steps.len() as i64;

    let steps_json: Vec<serde_json::Value> = steps
        .iter()
        .map(|s| {
            json!({
                "job_id": s.job_id,
                "workspace": s.workspace,
                "task_name": s.task_name,
                "job_status": s.job_status,
                "step_name": s.step_name,
                "action_type": s.action_type,
                "status": s.status,
                "started_at": s.started_at,
                "completed_at": s.completed_at,
                "error_message": s.error_message,
            })
        })
        .collect();

    Ok(Json(json!({
        "worker_id": worker.worker_id,
        "name": worker.name,
        "status": worker.status,
        "tags": worker.tags,
        "version": worker.version,
        "last_heartbeat": worker.last_heartbeat,
        "registered_at": worker.registered_at,
        "steps": { "items": steps_json, "total": total },
    })))
}
