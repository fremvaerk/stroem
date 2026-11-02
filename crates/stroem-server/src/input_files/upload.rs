//! Storing one uploaded input file (spec § 4.2): the core shared by the REST
//! upload route and, later, the MCP and upload-URL routes.

use crate::input_files::stream::{put_counted, PutError};
use crate::state::AppState;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_core::stream::BoxStream;
use serde::Serialize;
use stroem_common::file_field::{new_file_id, safe_name, FILE_TYPE};
use stroem_common::models::workflow::TaskDef;
use stroem_db::{InputFileRepo, NewInputFile};
use uuid::Uuid;

pub struct UploadTarget<'a> {
    pub workspace: &'a str,
    pub task_name: &'a str,
    pub original_name: &'a str,
    pub content_type: &'a str,
    pub user_id: Option<Uuid>,
    /// Metric label: `api`, `mcp_inline` or `mcp_url`.
    pub origin: &'static str,
}

#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub file_id: String,
    pub name: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub content_type: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug)]
pub enum UploadError {
    InvalidName,
    TooLarge { max: u64 },
    NotConfigured,
    Internal(anyhow::Error),
}

impl UploadError {
    fn result_label(&self) -> &'static str {
        match self {
            UploadError::InvalidName => "invalid_name",
            UploadError::TooLarge { .. } => "too_large",
            UploadError::NotConfigured | UploadError::Internal(_) => "error",
        }
    }
}

/// Fixed texts of spec § 8; never a name, key or path.
impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::InvalidName => f.write_str("invalid file name"),
            UploadError::TooLarge { max } => write!(f, "file is larger than {max} bytes"),
            UploadError::NotConfigured => f.write_str(super::STORAGE_NOT_CONFIGURED),
            UploadError::Internal(_) => f.write_str("the file could not be stored"),
        }
    }
}

/// True when the task declares at least one `type: file` input field.
pub fn task_has_file_field(task: &TaskDef) -> bool {
    task.input.values().any(|f| f.field_type == FILE_TYPE)
}

/// The request body went over a length limit enforced by axum / http-body.
fn is_length_limit_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<http_body_util::LengthLimitError>()
            .is_some()
    })
}

pub async fn store_upload(
    state: &AppState,
    t: UploadTarget<'_>,
    body: BoxStream<'static, anyhow::Result<Bytes>>,
    content_length: Option<u64>,
) -> Result<UploadResponse, UploadError> {
    let origin = t.origin;
    let result = store_upload_inner(state, t, body, content_length).await;
    let label = match &result {
        Ok(_) => "ok",
        Err(e) => e.result_label(),
    };
    metrics::counter!(
        crate::metrics::STROEM_INPUT_FILE_UPLOADS_TOTAL,
        "origin" => origin,
        "result" => label
    )
    .increment(1);
    if let Ok(r) = &result {
        metrics::counter!(crate::metrics::STROEM_INPUT_FILE_BYTES_TOTAL, "origin" => origin)
            .increment(r.size_bytes);
    }
    result
}

async fn store_upload_inner(
    state: &AppState,
    t: UploadTarget<'_>,
    body: BoxStream<'static, anyhow::Result<Bytes>>,
    content_length: Option<u64>,
) -> Result<UploadResponse, UploadError> {
    let safe = safe_name(t.original_name).ok_or(UploadError::InvalidName)?;
    let archive = state.input_blob.clone().ok_or(UploadError::NotConfigured)?;
    let cfg = &state.input_files_config;
    let max = cfg.max_file_bytes;
    if content_length.is_some_and(|n| n > max) {
        return Err(UploadError::TooLarge { max });
    }

    let file_id = new_file_id();
    let key = state.input_file_storage_key(t.workspace, &file_id);
    let stored = put_counted(archive.as_ref(), &key, t.content_type, body, max)
        .await
        .map_err(|e| match e {
            PutError::TooLarge { max } => UploadError::TooLarge { max },
            PutError::Body(e) if is_length_limit_error(&e) => UploadError::TooLarge { max },
            PutError::Body(e) | PutError::Storage(e) => UploadError::Internal(e),
        })?;

    let new = NewInputFile {
        file_id: &file_id,
        workspace: t.workspace,
        task_name: t.task_name,
        uploaded_by_user_id: t.user_id,
        origin: t.origin,
        original_name: t.original_name,
        safe_name: &safe,
        content_type: t.content_type,
        size_bytes: stored.size_bytes as i64,
        sha256: &stored.sha256,
        storage_key: &key,
    };
    let created_at = match insert_and_read(state, &new).await {
        Ok(c) => c,
        Err(e) => {
            if let Err(del) = archive.delete(&key).await {
                tracing::warn!("discarding an unrecorded upload failed: {del:#}");
            }
            return Err(UploadError::Internal(e));
        }
    };
    Ok(UploadResponse {
        file_id,
        name: t.original_name.to_string(),
        size_bytes: stored.size_bytes,
        sha256: stored.sha256,
        content_type: t.content_type.to_string(),
        expires_at: created_at + chrono::Duration::seconds(cfg.upload_ttl_secs as i64),
    })
}

async fn insert_and_read(
    state: &AppState,
    new: &NewInputFile<'_>,
) -> anyhow::Result<DateTime<Utc>> {
    InputFileRepo::insert(&state.pool, new).await?;
    InputFileRepo::get(&state.pool, new.file_id)
        .await?
        .map(|r| r.created_at)
        .ok_or_else(|| anyhow::anyhow!("input_file row missing after insert"))
}
