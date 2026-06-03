//! Worker-side artifact upload endpoint.
//!
//! `POST /worker/jobs/{job_id}/steps/{step_name}/artifacts/{name}` accepts the
//! file body verbatim and:
//!   1. Enforces the per-file size cap.
//!   2. Opens a transaction and takes a row-level lock on the parent `job`
//!      row (`SELECT … FOR UPDATE`) to serialise concurrent uploads.
//!   3. Rejects the upload if the job is in a terminal state
//!      (`completed`/`failed`/`cancelled`) with `409 Conflict`.
//!   4. Enforces the per-job size cap inside the locked transaction so two
//!      concurrent uploads can't both squeeze past the cap (TOCTOU).
//!   5. Writes the bytes to the configured artifact blob backend.
//!   6. Upserts the row in `job_artifact` (replace-by-name semantics) inside
//!      the same transaction; commit failure or upsert failure best-effort
//!      cleans up the staged blob.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use std::sync::Arc;
use stroem_db::repos::job_artifact::JobArtifactRepo;
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
    // Reject leading/trailing whitespace so " build" and "build " can't
    // collide with "build" once a downstream consumer trims, and so they
    // can't smuggle ambiguous lookups through path matching.
    if value.trim() != value {
        return Err(AppError::BadRequest(format!(
            "{field} has leading or trailing whitespace"
        )));
    }
    for ch in value.chars() {
        if ch == '\0' || ch == '\\' || ch == '/' || (ch.is_control() && ch != ' ') {
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

    // Per-file size limit — cheap, do it before opening any tx.
    if body.len() as u64 > cfg.max_file_bytes {
        return Err(AppError::PayloadTooLarge(format!(
            "file '{name}' is {} bytes, exceeds per-file limit of {}",
            body.len(),
            cfg.max_file_bytes
        )));
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let blob = state
        .artifact_blob
        .clone()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("artifact storage not configured")))?;

    // Per-job cap is enforced under a row-level lock on the parent job so two
    // concurrent uploads can't both observe a stale total and both squeeze
    // through the cap check (TOCTOU). The whole sequence
    //   lock job → read existing total → read replacing size → blob.put → upsert
    // runs inside one transaction; the blob.put happens AFTER the cap check
    // (so we don't stage bytes we'd reject) but before the upsert (so a
    // failed write doesn't leave an orphan row). If the upsert or the final
    // commit fails we best-effort delete the blob to avoid leaking storage.
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    // SELECT … FOR UPDATE serialises all uploads for this job_id. Also acts
    // as our FK check + status gate. If the job is already terminal we
    // refuse before doing any blob I/O.
    let job_row = sqlx::query_as::<_, (String, String)>(
        "SELECT workspace, status FROM job WHERE job_id = $1 FOR UPDATE",
    )
    .bind(job_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("job {job_id}")))?;
    let (workspace, status) = job_row;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        return Err(AppError::Conflict(format!(
            "job {job_id} is {status}; artifact uploads are no longer accepted"
        )));
    }

    // Cap math under the lock.
    let existing: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM job_artifact WHERE job_id = $1",
    )
    .bind(job_id)
    .fetch_one(&mut *tx)
    .await?;
    let replacing: Option<i64> =
        sqlx::query_scalar("SELECT size_bytes FROM job_artifact WHERE job_id = $1 AND name = $2")
            .bind(job_id)
            .bind(&name)
            .fetch_optional(&mut *tx)
            .await?;
    let projected = (existing as u64)
        .saturating_sub(replacing.unwrap_or(0) as u64)
        .saturating_add(body.len() as u64);
    if projected > cfg.max_job_bytes {
        return Err(AppError::PayloadTooLarge(format!(
            "adding '{name}' ({}) would push job to {} bytes, exceeds per-job limit of {}",
            body.len(),
            projected,
            cfg.max_job_bytes
        )));
    }

    // Cap holds: stage the blob. If the tx later fails to commit we delete
    // the blob (best-effort) so we don't leak storage.
    let key = state.artifact_storage_key(&workspace, job_id, &step_name, &name);
    blob.put(&key, &content_type, body.clone())
        .await
        .map_err(AppError::Internal)?;

    // Upsert row inside the same tx as the lock + cap check.
    let id = Uuid::new_v4();
    let rec = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            String,
            String,
            String,
            i64,
            String,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        r#"
        INSERT INTO job_artifact
            (id, job_id, step_name, name, content_type, size_bytes, storage_key)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (job_id, name) DO UPDATE
            SET step_name    = EXCLUDED.step_name,
                content_type = EXCLUDED.content_type,
                size_bytes   = EXCLUDED.size_bytes,
                storage_key  = EXCLUDED.storage_key,
                created_at   = NOW()
        RETURNING id, job_id, step_name, name, content_type, size_bytes, storage_key, created_at
        "#,
    )
    .bind(id)
    .bind(job_id)
    .bind(&step_name)
    .bind(&name)
    .bind(&content_type)
    .bind(body.len() as i64)
    .bind(&key)
    .fetch_one(&mut *tx)
    .await;

    let rec = match rec {
        Ok(r) => r,
        Err(e) => {
            // Best-effort blob cleanup so we don't leak orphaned bytes when
            // the row write fails after the put succeeded.
            let _ = blob.delete(&key).await;
            return Err(AppError::Internal(e.into()));
        }
    };

    if let Err(e) = tx.commit().await {
        let _ = blob.delete(&key).await;
        return Err(AppError::Internal(e.into()));
    }

    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            id: rec.0,
            name: rec.3,
            size_bytes: rec.5,
            content_type: rec.4,
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

    let job_row = sqlx::query_as::<_, (String, String)>(
        "SELECT workspace, status FROM job WHERE job_id = $1",
    )
    .bind(job_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("job {job_id}")))?;
    let (workspace, status) = job_row;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        return Err(AppError::Conflict(format!(
            "job {job_id} is {status}; artifact deletions are no longer accepted"
        )));
    }

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
        assert_bad(validate_path_segment("../escape", "step_name"));
        assert_bad(validate_path_segment("nested/step", "step_name"));
        assert_bad(validate_path_segment("evil\0step", "step_name"));
        assert_bad(validate_path_segment("win\\style", "step_name"));
        validate_path_segment("build", "step_name").unwrap();
        validate_path_segment("step-with-dash", "step_name").unwrap();
    }

    #[test]
    fn step_validation_rejects_leading_or_trailing_whitespace() {
        // Smuggling a leading space lets " build" look distinct from
        // "build" until something downstream trims, at which point you
        // get ambiguous lookups. We reject before any of that can happen.
        assert_bad(validate_path_segment(" build", "step_name"));
        assert_bad(validate_path_segment("build ", "step_name"));
        assert_bad(validate_path_segment("  build  ", "step_name"));
        assert_bad(validate_path_segment("\tbuild", "step_name"));
        assert_bad(validate_path_segment("build\t", "step_name"));
        // Internal space is still fine.
        validate_path_segment("build step", "step_name").unwrap();
    }
}
