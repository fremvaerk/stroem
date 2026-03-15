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
use stroem_db::{JobRepo, WorkerRepo};

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

/// GET /api/workers/:id - Get worker detail with recent jobs
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

    let jobs = JobRepo::list_by_worker(&state.pool, worker_id, 50, 0)
        .await
        .context("list jobs for worker")?;

    // ACL filter: remove jobs for tasks the user can't see
    let jobs: Vec<_> = if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            let user_id = match auth.user_id() {
                Ok(id) => id,
                Err(_) => {
                    // Couldn't parse user_id — return worker info with no jobs for safety
                    return Ok(Json(json!({
                        "worker_id": worker.worker_id,
                        "name": worker.name,
                        "status": worker.status,
                        "tags": worker.tags,
                        "version": worker.version,
                        "last_heartbeat": worker.last_heartbeat,
                        "registered_at": worker.registered_at,
                        "jobs": [],
                    })));
                }
            };
            match load_user_acl_context(&state.pool, user_id, auth.is_admin()).await {
                Ok((true, _)) => jobs, // admin sees all
                Ok((false, groups)) => {
                    let all_configs = state.workspaces.get_all_configs().await;
                    jobs.into_iter()
                        .filter(|j| {
                            let folder = all_configs
                                .iter()
                                .find(|(ws_name, _)| ws_name == &j.workspace)
                                .and_then(|(_, ws)| {
                                    ws.tasks.get(&j.task_name).and_then(|t| t.folder.clone())
                                });
                            let task_path = make_task_path(folder.as_deref(), &j.task_name);
                            let perm = state.acl.evaluate(
                                &j.workspace,
                                &task_path,
                                &auth.claims.email,
                                &groups,
                                false,
                            );
                            !matches!(perm, TaskPermission::Deny)
                        })
                        .collect()
                }
                Err(_) => vec![], // error loading ACL context, show no jobs for safety
            }
        } else {
            jobs
        }
    } else {
        jobs
    };

    let jobs_json: Vec<serde_json::Value> = jobs
        .iter()
        .map(|j| {
            json!({
                "job_id": j.job_id,
                "workspace": j.workspace,
                "task_name": j.task_name,
                "mode": j.mode,
                "status": j.status,
                "source_type": j.source_type,
                "source_id": j.source_id,
                "worker_id": j.worker_id,
                "created_at": j.created_at,
                "started_at": j.started_at,
                "completed_at": j.completed_at,
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
        "jobs": jobs_json,
    })))
}
