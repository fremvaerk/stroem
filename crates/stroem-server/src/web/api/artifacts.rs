//! User-facing artifact endpoints.
//!
//! - `GET /api/jobs/{id}/artifacts` — list artifacts for a job (ACL: View)
//! - `GET /api/jobs/{id}/artifacts/{name}` — download a single artifact (ACL: View)
//!
//! Inline content disposition is used only for a tight allow-list of "safe"
//! content types (raster images, plain text). Everything else — including
//! HTML, SVG, JavaScript, PDF and Markdown — is forced to `attachment` so a
//! malicious or misrendered upload cannot execute in the browser context of
//! an authenticated user. PDFs are excluded because of historical PDF.js
//! script-execution CVEs; Markdown is excluded because no major browser has
//! a built-in renderer and some intermediaries render it as HTML.
//! `X-Content-Type-Options: nosniff` is always set, and every successful
//! download response carries `Cache-Control: private, max-age=0,
//! must-revalidate` so the browser disk cache cannot replay a download to a
//! subsequent unauthenticated session on the same machine.

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
/// Everything not listed here — including HTML, SVG, JavaScript, PDF and
/// Markdown — is forced to `attachment`. PDFs are excluded because PDF.js (the
/// renderer Chromium/Firefox ship by default) has a history of script-
/// execution CVEs via embedded JS. Markdown is excluded because no major
/// browser renders it natively and some intermediaries (proxies, viewers)
/// will render it as HTML, reintroducing the XSS surface we close above.
const SAFE_INLINE: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "text/plain",
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

/// Sanitize a filename for use in a `Content-Disposition` header.
///
/// Strips characters that would let an attacker break out of the quoted
/// `filename=` value (quotes), inject a second header line (CR/LF), inject a
/// new disposition parameter such as `filename*=` (`;`), introduce a Windows
/// path separator (`\`), or smuggle a control character (anything matching
/// `char::is_control`). Replaced with `_` so the result is always a non-empty
/// safe string.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == '"' || c == ';' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
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
    // Stream the blob through to the client rather than buffering up to 100 MiB
    // per concurrent download. The default `get_stream` impl on `BlobArchive`
    // wraps the buffered backend, so this works for both `LocalBlobArchive`
    // and `S3BlobArchive` without further trait changes.
    let (_stored_ct, stream) = blob_archive
        .get_stream(&rec.storage_key)
        .await
        .map_err(AppError::Internal)?
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("blob missing for artifact {}", rec.name))
        })?;

    let mut resp = Response::new(Body::from_stream(stream));
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
    // Prevent the browser disk cache from replaying an authenticated download
    // to a subsequent unauthenticated session on the same machine.
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
    fn pdf_forced_to_attachment() {
        // PDF.js has a history of script-execution CVEs via embedded JS, so we
        // refuse to render PDFs inline even though the browser would happily
        // do so.
        let d = disposition_for("application/pdf", "report.pdf");
        assert!(d.starts_with("attachment"), "expected attachment, got {d}");
    }

    #[test]
    fn markdown_forced_to_attachment() {
        // No browser renders Markdown natively; some intermediaries render it
        // as HTML, which would reintroduce the XSS surface we close above.
        let d = disposition_for("text/markdown", "notes.md");
        assert!(d.starts_with("attachment"), "expected attachment, got {d}");
    }

    #[test]
    fn svg_forced_to_attachment() {
        // SVG can carry inline <script> and event handlers.
        let d = disposition_for("image/svg+xml", "logo.svg");
        assert!(d.starts_with("attachment"), "expected attachment, got {d}");
    }

    #[test]
    fn javascript_forced_to_attachment() {
        let d = disposition_for("application/javascript", "evil.js");
        assert!(d.starts_with("attachment"), "expected attachment, got {d}");
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

    #[test]
    fn sanitize_filename_strips_semicolon() {
        // `;` would let an attacker append a second Content-Disposition
        // parameter such as `filename*=UTF-8''evil.html`, which RFC 5987
        // says takes precedence over the quoted `filename=` value. Quotes
        // and backslashes are also stripped; the remaining `*=...''` is
        // harmless once it can't break out of the quoted value.
        let out = sanitize_filename("safe.html\"; filename*=UTF-8''evil.html");
        assert!(!out.contains(';'), "semicolon must be stripped: {out}");
        assert!(!out.contains('"'), "quote must be stripped: {out}");
        assert_eq!(out, "safe.html__ filename*=UTF-8''evil.html");
    }

    #[test]
    fn sanitize_filename_strips_backslash() {
        // `\` is the Windows path separator and is also the quoted-string
        // escape character per RFC 7230.
        assert_eq!(sanitize_filename("a\\b\\c.txt"), "a_b_c.txt");
    }

    #[test]
    fn sanitize_filename_strips_all_ascii_control_chars() {
        // Sweep every ASCII control character (U+0000–U+001F, U+007F) and
        // make sure none of them survive into the header value.
        for code in 0u8..=0x1F {
            let input = format!("a{}b", code as char);
            let out = sanitize_filename(&input);
            assert_eq!(out, "a_b", "control char 0x{code:02X} not stripped");
        }
        assert_eq!(sanitize_filename("a\x7Fb"), "a_b");
    }

    #[test]
    fn sanitize_filename_preserves_safe_characters() {
        assert_eq!(
            sanitize_filename("report-2026_q1.tar.gz"),
            "report-2026_q1.tar.gz"
        );
        // Plain space is not a control char and must be preserved.
        assert_eq!(sanitize_filename("hello world.txt"), "hello world.txt");
    }
}
