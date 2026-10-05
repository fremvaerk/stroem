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
use std::collections::HashMap;
use std::sync::Arc;
use stroem_db::{JobRepo, JobStepRepo, WorkerRepo};

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
                "capabilities": w.capabilities,
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
                        "capabilities": worker.capabilities,
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
                            let live_folder = all_configs
                                .iter()
                                .find(|(ws_name, _)| ws_name == &s.workspace)
                                .and_then(|(_, ws)| {
                                    ws.tasks.get(&s.task_name).and_then(|t| t.folder.clone())
                                });
                            // Spec § 7.8: a pinned job's own folder, never the live one.
                            let folder = crate::acl::acl_folder(
                                s.git_ref.as_deref(),
                                s.task_folder.as_deref(),
                                live_folder.as_deref(),
                            );
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

    // Per-job redaction of `error_message` (spec § 7.4): one per distinct job
    // that has an error to show. A job whose set cannot be built (a pin not
    // loadable, transiently or for good, or the row gone) has its rows' error
    // masked whole — fail closed per row, not per page. One memo for the
    // request: the short-circuit probe, each closure (by job id) and each
    // pin's values are computed once, however many rows share them.
    let mut redactions: HashMap<uuid::Uuid, Option<crate::redaction::JobRedaction>> =
        HashMap::new();
    let mut memo = crate::redaction::RedactionMemo::default();
    for s in &steps {
        if s.error_message.is_none() || redactions.contains_key(&s.job_id) {
            continue;
        }
        let set = match (
            JobRepo::get(&state.pool, s.job_id).await,
            JobStepRepo::get_steps_for_job(&state.pool, s.job_id).await,
        ) {
            (Ok(Some(job)), Ok(job_steps)) => {
                crate::redaction::job_redaction_memo(&state, &job, &job_steps, &mut memo)
                    .await
                    .ok()
            }
            _ => None,
        };
        redactions.insert(s.job_id, set);
    }

    let steps_json: Vec<serde_json::Value> = steps
        .iter()
        .map(|s| {
            let error_message =
                s.error_message
                    .as_deref()
                    .map(|m| match redactions.get(&s.job_id) {
                        Some(Some(redaction)) => redaction.apply_str(m),
                        _ => crate::workspace_set::REDACTED.to_string(),
                    });
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
                "error_message": error_message,
            })
        })
        .collect();

    Ok(Json(json!({
        "worker_id": worker.worker_id,
        "name": worker.name,
        "status": worker.status,
        "capabilities": worker.capabilities,
        "tags": worker.tags,
        "version": worker.version,
        "last_heartbeat": worker.last_heartbeat,
        "registered_at": worker.registered_at,
        "steps": { "items": steps_json, "total": total },
    })))
}
