//! Worker-side artifact upload endpoint.
//!
//! `POST /worker/jobs/{job_id}/steps/{step_name}/artifacts/{name}` accepts the
//! file body verbatim and:
//!   1. Enforces the per-file size cap.
//!   2. Enforces the per-job size cap (existing + this upload, minus any
//!      same-named artifact being replaced).
//!   3. Resolves the workspace from the `job` row (FK validation).
//!   4. Writes the bytes to the configured artifact blob backend.
//!   5. Upserts the row in `job_artifact` (replace-by-name semantics).

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use std::sync::Arc;
use stroem_db::repos::job_artifact::{JobArtifactRepo, NewArtifactRow};
use uuid::Uuid;

use crate::state::AppState;
use crate::web::error::AppError;

#[derive(serde::Serialize)]
pub struct UploadResponse {
    pub id: Uuid,
    pub name: String,
    pub size_bytes: i64,
    pub content_type: String,
}

#[tracing::instrument(skip(state, body))]
pub async fn upload_artifact(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name, name)): Path<(Uuid, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let cfg = &state.artifact_config;

    // Per-file size limit
    if body.len() as u64 > cfg.max_file_bytes {
        return Err(AppError::PayloadTooLarge(format!(
            "file '{name}' is {} bytes, exceeds per-file limit of {}",
            body.len(),
            cfg.max_file_bytes
        )));
    }

    // Per-job size limit (counts existing + this upload)
    let repo = JobArtifactRepo::new(state.pool.clone());
    let existing = repo.total_size_for_job(job_id).await? as u64;
    // If we're replacing the same-named artifact, subtract its size from the cap math.
    let replacing = repo
        .get_by_name(job_id, &name)
        .await?
        .map(|r| r.size_bytes as u64)
        .unwrap_or(0);
    let projected = existing
        .saturating_sub(replacing)
        .saturating_add(body.len() as u64);
    if projected > cfg.max_job_bytes {
        return Err(AppError::PayloadTooLarge(format!(
            "adding '{name}' ({}) would push job to {} bytes, exceeds per-job limit of {}",
            body.len(),
            projected,
            cfg.max_job_bytes
        )));
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // Resolve workspace from the job row (validate FK + collect prefix)
    let workspace = sqlx::query_scalar::<_, String>("SELECT workspace FROM job WHERE job_id = $1")
        .bind(job_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {job_id}")))?;

    let blob = state
        .artifact_blob
        .clone()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("artifact storage not configured")))?;
    let key = state.artifact_storage_key(&workspace, job_id, &step_name, &name);
    blob.put(&key, &content_type, body.clone())
        .await
        .map_err(AppError::Internal)?;

    let rec = repo
        .upsert(NewArtifactRow {
            job_id,
            step_name,
            name: name.clone(),
            content_type: content_type.clone(),
            size_bytes: body.len() as i64,
            storage_key: key,
        })
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            id: rec.id,
            name: rec.name,
            size_bytes: rec.size_bytes,
            content_type: rec.content_type,
        }),
    ))
}
