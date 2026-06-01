//! User-facing artifact endpoints.
//!
//! - `GET /api/jobs/{id}/artifacts` — list artifacts for a job (ACL: View)
//! - `GET /api/jobs/{id}/artifacts/{name}` — download a single artifact (ACL: View)
//!
//! Inline content disposition is used only for an allow-list of "safe" content
//! types (images, PDF, plain text/markdown). Everything else — including HTML,
//! SVG and JS — is forced to `attachment` so a malicious upload cannot execute
//! in the browser context of an authenticated user. `X-Content-Type-Options:
//! nosniff` is always set.

use anyhow::Context;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderValue},
    response::Response,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::Arc;
use stroem_db::repos::job_artifact::JobArtifactRepo;
use stroem_db::JobRepo;

use crate::acl::TaskPermission;
use crate::state::AppState;
use crate::web::api::jobs::check_job_acl;
use crate::web::api::middleware::AuthUser;
use crate::web::api::parse_uuid_param;
use crate::web::error::AppError;

#[derive(Debug, Serialize)]
pub struct ArtifactListItem {
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub step_name: String,
    pub created_at: DateTime<Utc>,
    pub url: String,
}

/// Content types that are safe to render inline in the browser.
///
/// HTML, SVG and JavaScript are deliberately excluded: even when uploaded by a
/// trusted task they could execute in the user's authenticated session.
const SAFE_INLINE: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "application/pdf",
    "text/plain",
    "text/markdown",
];

fn disposition_for(content_type: &str, name: &str) -> String {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let safe = SAFE_INLINE.contains(&ct.as_str());
    let kind = if safe { "inline" } else { "attachment" };
    format!("{}; filename=\"{}\"", kind, sanitize_filename(name))
}

fn sanitize_filename(name: &str) -> String {
    name.replace(['"', '\r', '\n'], "_")
}

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>()
}

/// GET /api/jobs/{id}/artifacts — list artifacts for a job.
#[tracing::instrument(skip(state))]
pub async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ArtifactListItem>>, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;

    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    if matches!(perm, TaskPermission::Deny) {
        return Err(AppError::not_found("Job"));
    }

    let rows = JobArtifactRepo::new(state.pool.clone())
        .list_for_job(job_id)
        .await
        .context("list artifacts")?;

    Ok(Json(
        rows.into_iter()
            .map(|r| ArtifactListItem {
                url: format!("/api/jobs/{job_id}/artifacts/{}", url_encode(&r.name)),
                name: r.name,
                content_type: r.content_type,
                size_bytes: r.size_bytes,
                step_name: r.step_name,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

/// GET /api/jobs/{id}/artifacts/{name} — download a single artifact.
#[tracing::instrument(skip(state))]
pub async fn download_artifact(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;

    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    if matches!(perm, TaskPermission::Deny) {
        return Err(AppError::not_found("Job"));
    }

    let repo = JobArtifactRepo::new(state.pool.clone());
    let rec = repo
        .get_by_name(job_id, &name)
        .await
        .context("get artifact")?
        .ok_or_else(|| AppError::not_found("Artifact"))?;

    let blob_archive = state
        .artifact_blob
        .clone()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("artifact storage not configured")))?;
    let stored = blob_archive
        .get(&rec.storage_key)
        .await
        .map_err(AppError::Internal)?
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("blob missing for artifact {}", rec.name))
        })?;

    let mut resp = Response::new(Body::from(stored.bytes));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&rec.content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition_for(&rec.content_type, &rec.name))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(rec.size_bytes));
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_mime_renders_inline() {
        let d = disposition_for("image/png", "x.png");
        assert!(d.starts_with("inline"));
    }

    #[test]
    fn unknown_mime_forces_attachment() {
        let d = disposition_for("application/x-evil", "x.bin");
        assert!(d.starts_with("attachment"));
    }

    #[test]
    fn html_forced_to_attachment() {
        let d = disposition_for("text/html", "evil.html");
        assert!(d.starts_with("attachment"));
    }

    #[test]
    fn content_type_with_charset_still_recognized() {
        let d = disposition_for("text/plain; charset=utf-8", "readme.txt");
        assert!(d.starts_with("inline"));
    }

    #[test]
    fn sanitize_filename_strips_quotes_and_newlines() {
        assert_eq!(sanitize_filename("a\"b\rc\nd"), "a_b_c_d");
    }
}
