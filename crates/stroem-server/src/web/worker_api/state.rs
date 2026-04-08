use crate::state::AppState;
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use std::sync::Arc;
use uuid::Uuid;

/// Extract `state.json` from a gzip tarball and return the parsed JSON value.
///
/// Iterates over tarball entries looking for a file named `state.json`
/// (at any path depth). Returns `None` if the tarball cannot be decoded,
/// if no `state.json` entry is found, or if the entry is not valid JSON.
pub fn extract_state_json(data: &[u8]) -> Option<serde_json::Value> {
    use flate2::read::GzDecoder;
    use std::ffi::OsStr;
    use std::io::Read;
    use tar::Archive;

    let decoder = GzDecoder::new(data);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries().ok()? {
        let Ok(mut entry) = entry else { continue };
        let Ok(path) = entry.path().map(|p| p.into_owned()) else {
            continue;
        };
        if path.file_name() == Some(OsStr::new("state.json")) {
            let mut contents = String::new();
            entry.read_to_string(&mut contents).ok()?;
            return serde_json::from_str(&contents).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a gzip tarball in memory containing the given (path, data) pairs.
    fn build_test_tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            for (path, data) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, &data[..]).unwrap();
            }
            builder.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[test]
    fn test_extract_state_json_valid() {
        let json = br#"{"cursor": "abc123", "count": 42}"#;
        let tarball = build_test_tarball(&[("state.json", json)]);
        let result = extract_state_json(&tarball);
        assert_eq!(
            result,
            Some(serde_json::json!({"cursor": "abc123", "count": 42}))
        );
    }

    #[test]
    fn test_extract_state_json_no_state_file() {
        let tarball = build_test_tarball(&[("other.txt", b"hello")]);
        assert_eq!(extract_state_json(&tarball), None);
    }

    #[test]
    fn test_extract_state_json_invalid_json() {
        let tarball = build_test_tarball(&[("state.json", b"not json")]);
        assert_eq!(extract_state_json(&tarball), None);
    }

    #[test]
    fn test_extract_state_json_empty_bytes() {
        assert_eq!(extract_state_json(&[]), None);
    }

    #[test]
    fn test_extract_state_json_corrupt_gzip() {
        assert_eq!(extract_state_json(&[0x1f, 0x8b, 0x00, 0xff]), None);
    }

    #[test]
    fn test_extract_state_json_with_other_files() {
        let json = br#"{"key": "value"}"#;
        let tarball = build_test_tarball(&[
            ("README.md", b"# readme"),
            ("state.json", json),
            ("cert.pem", b"-----BEGIN CERTIFICATE-----"),
        ]);
        let result = extract_state_json(&tarball);
        assert_eq!(result, Some(serde_json::json!({"key": "value"})));
    }

    #[test]
    fn test_extract_state_json_nested_path() {
        // state.json nested inside a subdirectory should still be found
        let json = br#"{"nested": true}"#;
        let tarball = build_test_tarball(&[("subdir/state.json", json)]);
        let result = extract_state_json(&tarball);
        assert_eq!(result, Some(serde_json::json!({"nested": true})));
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct UploadStateQuery {
    /// Whether the tarball contains a JSON sidecar (has_json flag stored in DB).
    #[serde(default)]
    pub has_json: bool,
}

/// GET /worker/state/{ws}/{task} — Download the latest state snapshot tarball.
///
/// Returns `200 OK` with `Content-Type: application/gzip` and an
/// `X-Snapshot-Id` header carrying the snapshot UUID when a snapshot exists.
///
/// Returns `204 No Content` when no snapshot has been stored yet, or when
/// the snapshot key recorded in the database is no longer present in the
/// archive (stale reference).
///
/// Returns `404 Not Found` when state storage is not configured.
#[tracing::instrument(skip(state))]
pub async fn download_state(
    State(state): State<Arc<AppState>>,
    Path((workspace, task_name)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let storage = state
        .state_storage
        .as_ref()
        .ok_or_else(|| AppError::NotFound("State storage not configured".into()))?;

    let snapshot = match stroem_db::TaskStateRepo::get_latest(&state.pool, &workspace, &task_name)
        .await
        .context("lookup latest state snapshot")?
    {
        Some(s) => s,
        None => return Ok(StatusCode::NO_CONTENT.into_response()),
    };

    let data = match storage
        .retrieve(&snapshot.storage_key)
        .await
        .context("retrieve state snapshot")?
    {
        Some(d) => d,
        None => {
            tracing::warn!(
                snapshot_id = %snapshot.id,
                storage_key = %snapshot.storage_key,
                "State snapshot referenced in DB but not found in archive"
            );
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/gzip".parse().unwrap());
    if let Ok(val) = snapshot.id.to_string().parse() {
        headers.insert("x-snapshot-id", val);
    }

    Ok((StatusCode::OK, headers, data).into_response())
}

/// POST /worker/state/{ws}/{task}/{job_id} — Upload a state snapshot tarball.
///
/// The request body must be the raw gzip bytes of the snapshot. The
/// `?has_json=true` query parameter can be passed when the tarball also
/// contains a JSON sidecar file.
///
/// On success returns `201 Created` with `{ "snapshot_id": "<uuid>" }`.
///
/// The upload is rejected with `404 Not Found` when:
/// - State storage is not configured.
/// - The referenced job does not exist.
///
/// The upload is rejected with `400 Bad Request` when the job does not
/// belong to the specified workspace and task.
///
/// Old snapshots beyond the configured retention limit are pruned from
/// both the DB and the archive backend (best-effort, in the background).
#[tracing::instrument(skip(state, body))]
pub async fn upload_state(
    State(state): State<Arc<AppState>>,
    Path((workspace, task_name, job_id)): Path<(String, String, Uuid)>,
    Query(query): Query<UploadStateQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let storage = state
        .state_storage
        .as_ref()
        .ok_or_else(|| AppError::NotFound("State storage not configured".into()))?;

    // Validate the job exists and belongs to this workspace + task.
    let job = stroem_db::JobRepo::get(&state.pool, job_id)
        .await
        .context("lookup job for state upload")?
        .ok_or_else(|| AppError::NotFound(format!("Job {} not found", job_id)))?;

    if job.workspace != workspace || job.task_name != task_name {
        return Err(AppError::BadRequest(
            "Job does not belong to this workspace/task".into(),
        ));
    }

    // Build storage key and persist the bytes.
    let key = storage.storage_key(&workspace, &task_name, job_id);
    storage
        .store(&key, &body)
        .await
        .context("store state snapshot")?;

    // Record the snapshot and prune old ones atomically. If the DB write fails,
    // delete the blob we just uploaded so no orphaned data is left behind.
    let (snapshot_id, deleted_keys) = match stroem_db::TaskStateRepo::insert_and_prune(
        &state.pool,
        &workspace,
        &task_name,
        job_id,
        &key,
        body.len() as i64,
        query.has_json,
        storage.max_snapshots(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            // Compensating action: delete the orphaned archive blob.
            if let Err(del_err) = storage.delete(&key).await {
                tracing::error!(
                    "Failed to clean up orphaned state blob {}: {:#}",
                    key,
                    del_err
                );
            }
            return Err(AppError::Internal(
                e.context("insert state snapshot record"),
            ));
        }
    };

    // Delete pruned snapshots from the archive backend (best-effort, background).
    if !deleted_keys.is_empty() {
        // SAFETY: state_storage is Some — we checked above.
        let storage_clone = Arc::clone(storage);
        tokio::spawn(async move {
            for key in deleted_keys {
                if let Err(e) = storage_clone.delete(&key).await {
                    tracing::warn!("Failed to delete pruned state snapshot {}: {:#}", key, e);
                }
            }
        });
    }

    tracing::info!(
        workspace = %workspace,
        task_name = %task_name,
        %job_id,
        bytes = body.len(),
        has_json = query.has_json,
        "Stored state snapshot"
    );

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "snapshot_id": snapshot_id })),
    )
        .into_response())
}
