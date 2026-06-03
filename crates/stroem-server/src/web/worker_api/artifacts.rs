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

/// Reject names/segments that could escape the artifact sandbox or store opaque
/// bytes (NUL/control chars). The on-disk `LocalBlobArchive` also rejects these
/// via its key validator, but we fail fast at the API boundary with a clear
/// `400` rather than bubbling up a `500` from the blob layer — and S3 keys
/// don't get the same protection, so the handler is the right gate.
fn validate_path_segment(value: &str, field: &str) -> Result<(), AppError> {
    if value.is_empty() {
        return Err(AppError::BadRequest(format!("{field} is empty")));
    }
    if value.len() > 255 {
        return Err(AppError::BadRequest(format!(
            "{field} exceeds 255 characters"
        )));
    }
    if value == "." || value == ".." {
        return Err(AppError::BadRequest(format!("invalid {field}: {value}")));
    }
    for ch in value.chars() {
        if ch == '\0' || ch == '\\' || (ch.is_control() && ch != ' ') {
            return Err(AppError::BadRequest(format!(
                "{field} contains forbidden character"
            )));
        }
    }
    Ok(())
}

fn validate_artifact_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::BadRequest("artifact name is empty".into()));
    }
    if name.len() > 255 {
        return Err(AppError::BadRequest(
            "artifact name exceeds 255 characters".into(),
        ));
    }
    if name.starts_with('/') {
        return Err(AppError::BadRequest("artifact name is absolute".into()));
    }
    if name.starts_with('-') {
        return Err(AppError::BadRequest(
            "artifact name must not start with '-'".into(),
        ));
    }
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(AppError::BadRequest(format!(
                "invalid path segment in artifact name: '{segment}'"
            )));
        }
        if segment.starts_with('-') {
            return Err(AppError::BadRequest(
                "artifact name segment must not start with '-'".into(),
            ));
        }
        for ch in segment.chars() {
            if ch == '\0' || ch == '\\' || (ch.is_control() && ch != ' ') {
                return Err(AppError::BadRequest(
                    "artifact name contains forbidden character".into(),
                ));
            }
        }
    }
    Ok(())
}

#[tracing::instrument(skip(state, body))]
pub async fn upload_artifact(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name, name)): Path<(Uuid, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Reject path-traversal and control-char inputs before doing any DB work
    // or constructing a storage key.
    validate_path_segment(&step_name, "step_name")?;
    validate_artifact_name(&name)?;

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

/// Worker-only cleanup hook used when artifact upload for a step fails partway
/// through. Removes every `job_artifact` row recorded for the step and deletes
/// the matching `{prefix}{ws}/{job_id}/{step}/` blob subtree. Idempotent: re-
/// calling on an already-clean step is a 204 no-op.
#[tracing::instrument(skip(state))]
pub async fn delete_step_artifacts(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(Uuid, String)>,
) -> Result<StatusCode, AppError> {
    validate_path_segment(&step_name, "step_name")?;

    let workspace = sqlx::query_scalar::<_, String>("SELECT workspace FROM job WHERE job_id = $1")
        .bind(job_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {job_id}")))?;

    let blob = state
        .artifact_blob
        .clone()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("artifact storage not configured")))?;
    let prefix = format!(
        "{}{}/{}/{}/",
        state.artifact_config.prefix, workspace, job_id, step_name
    );
    blob.delete_prefix(&prefix)
        .await
        .map_err(AppError::Internal)?;

    JobArtifactRepo::new(state.pool.clone())
        .delete_for_step(job_id, &step_name)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_bad(result: Result<(), AppError>) {
        assert!(
            matches!(result, Err(AppError::BadRequest(_))),
            "expected BadRequest, got {result:?}"
        );
    }

    #[test]
    fn name_validation_rejects_traversal_and_control() {
        assert_bad(validate_artifact_name(""));
        assert_bad(validate_artifact_name("/abs"));
        assert_bad(validate_artifact_name(".."));
        assert_bad(validate_artifact_name("../etc/passwd"));
        assert_bad(validate_artifact_name("foo/../bar"));
        assert_bad(validate_artifact_name("foo/./bar"));
        assert_bad(validate_artifact_name("foo//bar"));
        assert_bad(validate_artifact_name("a\0b"));
        assert_bad(validate_artifact_name("a\x01b"));
        assert_bad(validate_artifact_name("-leading"));
        assert_bad(validate_artifact_name("ok/-leading-segment"));
        assert_bad(validate_artifact_name("win\\style"));
        assert_bad(validate_artifact_name(&"x".repeat(256)));
    }

    #[test]
    fn name_validation_accepts_normal_inputs() {
        validate_artifact_name("report.html").unwrap();
        validate_artifact_name("reports/q1.html").unwrap();
        validate_artifact_name("nested/dir/file_name-1.zip").unwrap();
        validate_artifact_name("ok with space.txt").unwrap();
    }

    #[test]
    fn step_validation_rejects_traversal() {
        assert_bad(validate_path_segment("", "step_name"));
        assert_bad(validate_path_segment(".", "step_name"));
        assert_bad(validate_path_segment("..", "step_name"));
        assert_bad(validate_path_segment("evil\0step", "step_name"));
        validate_path_segment("build", "step_name").unwrap();
        validate_path_segment("step-with-dash", "step_name").unwrap();
    }
}
