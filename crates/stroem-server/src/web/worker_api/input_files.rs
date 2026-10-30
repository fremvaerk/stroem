//! Worker download of a job's input file (spec 2026-10-29 § 6.1): used by
//! the worker for local and Docker steps and by the pod init container.

use std::sync::Arc;

use anyhow::Context;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use stroem_db::JobInputFileRepo;
use uuid::Uuid;

use crate::input_files::stream::download_body;
use crate::state::AppState;
use crate::web::error::AppError;

/// 503 + `Retry-After: 5`: the worker retries like a pinned tarball
/// (12 attempts, 5 s apart).
fn unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "5")],
        Json(json!({ "error": message })),
    )
        .into_response()
}

/// GET /worker/jobs/{job_id}/input-files/{file_id} — stream a file attached
/// to the job. 404 unless the file is attached to that job (an unknown or
/// malformed job id included) or its blob is gone; 503 while the store is
/// not configured or cannot be read.
#[tracing::instrument(skip(state))]
pub async fn download_input_file(
    State(state): State<Arc<AppState>>,
    Path((job_id, file_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let not_found = || AppError::not_found("Input file");
    let job_id = Uuid::parse_str(&job_id).map_err(|_| not_found())?;
    let rec = JobInputFileRepo::get_attached(&state.pool, job_id, &file_id)
        .await
        .context("get attached input file")?
        .ok_or_else(not_found)?;
    let Some(archive) = state.input_blob.clone() else {
        return Ok(unavailable("file storage is not configured"));
    };
    let body = match download_body(archive.as_ref(), &rec.storage_key).await {
        Ok(Some(body)) => body,
        Ok(None) => {
            tracing::warn!(%job_id, file_id, "input file blob is missing");
            return Err(not_found());
        }
        Err(e) => {
            tracing::warn!(%job_id, file_id, "reading an input file failed: {:#}", e);
            return Ok(unavailable("file storage is unavailable; retry"));
        }
    };

    let mut resp = Response::new(body);
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(rec.size_bytes));
    Ok(resp)
}
