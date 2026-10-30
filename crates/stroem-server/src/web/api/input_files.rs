//! Input files of `type: file` task fields: the upload route for a live task
//! and the user download of a job's file (spec § 4.2, § 7.2).

use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::TryStreamExt;
use serde::Deserialize;
use stroem_db::{JobInputFileRepo, JobRepo};

use crate::acl::TaskPermission;
use crate::input_files::stream::download_body;
use crate::input_files::upload::{store_upload, task_has_file_field, UploadError, UploadTarget};
use crate::state::AppState;
use crate::web::api::artifacts::{disposition_for, sanitize_filename};
use crate::web::api::jobs::check_job_acl;
use crate::web::api::middleware::AuthUser;
use crate::web::api::tasks::require_task_run;
use crate::web::api::{get_workspace_or_error, parse_uuid_param};
use crate::web::error::AppError;

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";
const MAX_CONTENT_TYPE_BYTES: usize = 255;

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    name: Option<String>,
}

impl From<UploadError> for AppError {
    fn from(e: UploadError) -> Self {
        match e {
            UploadError::InvalidName => AppError::BadRequest(e.to_string()),
            UploadError::TooLarge { .. } => AppError::PayloadTooLarge(e.to_string()),
            UploadError::NotConfigured => AppError::ServiceUnavailable(e.to_string()),
            UploadError::Internal(inner) => AppError::Internal(inner),
        }
    }
}

/// The request's `Content-Type` as sent, when it is a usable header value.
fn request_content_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= MAX_CONTENT_TYPE_BYTES)
        .unwrap_or(DEFAULT_CONTENT_TYPE)
        .to_string()
}

/// POST /api/workspaces/{ws}/tasks/{name}/files?name= — upload a file for a
/// `type: file` field of a live task.
#[tracing::instrument(skip(state, auth_user, headers, body))]
pub async fn upload_task_file(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((ws, name)): Path<(String, String)>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    if state.config.auth.is_some() && auth_user.is_none() {
        return Err(AppError::Unauthorized("Authentication required".into()));
    }
    let config = get_workspace_or_error(&state, &ws).await?;
    let task = config
        .tasks
        .get(&name)
        .ok_or_else(|| AppError::not_found("Task"))?;
    require_task_run(&state, &auth_user, &ws, &name, task.folder.as_deref()).await?;
    if !task_has_file_field(task) {
        return Err(AppError::BadRequest("task has no file input".into()));
    }
    let original_name = q
        .name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| AppError::BadRequest("invalid file name".into()))?;
    let user_id = match &auth_user {
        Some(u) => Some(u.user_id()?),
        None => None,
    };
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let content_type = request_content_type(&headers);

    let stream = body
        .into_data_stream()
        .map_err(|e| anyhow::Error::new(e).context("read upload body"));
    let resp = store_upload(
        &state,
        UploadTarget {
            workspace: &ws,
            task_name: &name,
            original_name: &original_name,
            content_type: &content_type,
            user_id,
            origin: "api",
        },
        Box::pin(stream),
        content_length,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(resp)).into_response())
}

/// Percent-encodes `s` for the `filename*=UTF-8''` form (RFC 5987 `attr-char`).
fn rfc5987_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `Content-Disposition` for a user-supplied original name: inline only for
/// the artifact allow-list, an ASCII-only quoted fallback, and the exact
/// name in `filename*`.
fn input_file_disposition(content_type: &str, original_name: &str) -> String {
    let kind = if disposition_for(content_type, "").starts_with("inline") {
        "inline"
    } else {
        "attachment"
    };
    let ascii: String = sanitize_filename(original_name)
        .chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .collect();
    format!(
        "{kind}; filename=\"{ascii}\"; filename*=UTF-8''{}",
        rfc5987_encode(original_name)
    )
}

/// GET /api/jobs/{id}/input-files/{field} — download the file bound to a
/// job's `type: file` field.
#[tracing::instrument(skip(state, auth_user))]
pub async fn download_input_file(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((id, field)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;
    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;
    if matches!(
        check_job_acl(&state, &auth_user, &job).await?,
        TaskPermission::Deny
    ) {
        return Err(AppError::not_found("Job"));
    }
    let rec = JobInputFileRepo::get_for_field(&state.pool, job_id, &field)
        .await
        .context("get job input file")?
        .ok_or_else(|| AppError::not_found("Input file"))?;
    let archive = state
        .input_blob
        .clone()
        .ok_or_else(|| AppError::ServiceUnavailable("file storage is not configured".into()))?;
    let body = download_body(archive.as_ref(), &rec.storage_key)
        .await
        .map_err(AppError::Internal)?
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("blob missing for an input file")))?;

    let mut resp = Response::new(body);
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&rec.content_type)
            .unwrap_or(HeaderValue::from_static(DEFAULT_CONTENT_TYPE)),
    );
    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&input_file_disposition(
            &rec.content_type,
            &rec.original_name,
        ))
        .unwrap_or(HeaderValue::from_static("attachment")),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(rec.size_bytes));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=0, must-revalidate"),
    );
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_is_ascii_safe_and_carries_the_exact_name() {
        let d = input_file_disposition("text/csv", "rapport \"é\";.csv");
        assert!(d.starts_with("attachment; filename=\"rapport _"));
        assert!(d.contains("filename*=UTF-8''rapport%20%22%C3%A9%22%3B.csv"));
        assert!(HeaderValue::from_str(&d).is_ok());
    }

    #[test]
    fn safe_image_renders_inline() {
        assert!(input_file_disposition("image/png", "a.png").starts_with("inline"));
        assert!(input_file_disposition("text/html", "a.html").starts_with("attachment"));
    }

    #[test]
    fn request_content_type_falls_back() {
        let mut h = HeaderMap::new();
        assert_eq!(request_content_type(&h), DEFAULT_CONTENT_TYPE);
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"));
        assert_eq!(request_content_type(&h), "text/csv");
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&"x".repeat(256)).unwrap(),
        );
        assert_eq!(request_content_type(&h), DEFAULT_CONTENT_TYPE);
    }
}
