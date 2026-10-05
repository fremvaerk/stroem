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

/// The state partition a request reads or writes (spec § 7.6): a job's own
/// `(workspace, task, git_ref)`. Derived on the server from the job, never
/// from the client's path — a cross-workspace step's worker sends the action
/// OWNER's workspace there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateCoords {
    pub workspace: String,
    pub task_name: String,
    pub git_ref: Option<String>,
}

impl StateCoords {
    pub fn of_job(job: &stroem_db::JobRow) -> Self {
        StateCoords {
            workspace: job.workspace.clone(),
            task_name: job.task_name.clone(),
            git_ref: job.git_ref.clone(),
        }
    }

    /// An old worker's download (no `job_id`): the path, NULL partition.
    pub fn from_path(workspace: &str, task_name: &str) -> Self {
        StateCoords {
            workspace: workspace.to_string(),
            task_name: task_name.to_string(),
            git_ref: None,
        }
    }
}

/// `?job_id=` on state downloads.
#[derive(Debug, Default, serde::Deserialize)]
pub struct StateDownloadQuery {
    #[serde(default)]
    pub job_id: Option<Uuid>,
}

async fn load_job(state: &AppState, job_id: Uuid) -> Result<stroem_db::JobRow, AppError> {
    stroem_db::JobRepo::get(&state.pool, job_id)
        .await
        .context("lookup job for state request")?
        .ok_or_else(|| AppError::NotFound(format!("Job {} not found", job_id)))
}

async fn download_coords(
    state: &AppState,
    path_workspace: &str,
    path_task: &str,
    job_id: Option<Uuid>,
) -> Result<StateCoords, AppError> {
    match job_id {
        Some(id) => Ok(StateCoords::of_job(&load_job(state, id).await?)),
        None => Ok(StateCoords::from_path(path_workspace, path_task)),
    }
}

/// GET /worker/global-state/{ws} — Download the latest global workspace state snapshot.
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
pub async fn download_global_state(
    State(state): State<Arc<AppState>>,
    Path(workspace): Path<String>,
    Query(query): Query<StateDownloadQuery>,
) -> Result<impl IntoResponse, AppError> {
    let storage = state
        .state_storage
        .as_ref()
        .ok_or_else(|| AppError::NotFound("State storage not configured".into()))?;

    let coords = download_coords(&state, &workspace, "", query.job_id).await?;
    let snapshot = match stroem_db::WorkspaceStateRepo::get_latest_for_ref(
        &state.pool,
        &coords.workspace,
        coords.git_ref.as_deref(),
    )
    .await
    .context("lookup latest global state snapshot")?
    {
        Some(s) => s,
        None => return Ok(StatusCode::NO_CONTENT.into_response()),
    };

    let data = match storage
        .retrieve(&snapshot.storage_key)
        .await
        .context("retrieve global state snapshot")?
    {
        Some(d) => d,
        None => {
            tracing::warn!(
                snapshot_id = %snapshot.id,
                storage_key = %snapshot.storage_key,
                "Global state snapshot referenced in DB but not found in archive"
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

/// POST /worker/global-state/{ws}/{job_id} — Upload a global workspace state snapshot.
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
/// The partition is the job's own `(workspace, ref)`, not the path's.
///
/// Old snapshots beyond the configured retention limit are pruned from
/// both the DB and the archive backend (best-effort, in the background).
#[tracing::instrument(skip(state, body))]
pub async fn upload_global_state(
    State(state): State<Arc<AppState>>,
    Path((_path_workspace, job_id)): Path<(String, Uuid)>,
    Query(query): Query<UploadStateQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let storage = state
        .state_storage
        .as_ref()
        .ok_or_else(|| AppError::NotFound("State storage not configured".into()))?;

    // The job, not the path, decides the partition (spec § 7.6).
    let job = load_job(&state, job_id).await?;
    let coords = StateCoords::of_job(&job);

    // Build storage key and persist the bytes.
    let key = storage.global_storage_key(&coords.workspace, job_id);
    storage
        .store(&key, &body)
        .await
        .context("store global state snapshot")?;

    // Persist the parsed sidecar (migration 047) using exactly the gate and
    // extractor the claim path used to apply on every claim.
    let state_json = if query.has_json {
        extract_state_json(&body)
    } else {
        None
    };

    // Record the snapshot and prune old ones atomically inside a transaction
    // owned here. If anything fails, delete the blob we just uploaded so no
    // orphaned data is left behind.
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            let _ = storage.delete(&key).await;
            return Err(AppError::Internal(
                anyhow::anyhow!(e).context("begin global state upload tx"),
            ));
        }
    };

    let (snapshot_id, deleted_keys) = match stroem_db::WorkspaceStateRepo::insert_and_prune_for_ref(
        &mut tx,
        &coords.workspace,
        coords.git_ref.as_deref(),
        &job.task_name,
        job_id,
        &key,
        body.len() as i64,
        query.has_json,
        state_json.as_ref(),
        storage.global_max_snapshots(),
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            // Compensating action: delete the orphaned archive blob.
            if let Err(del_err) = storage.delete(&key).await {
                tracing::error!(
                    "Failed to clean up orphaned global state blob {}: {:#}",
                    key,
                    del_err
                );
            }
            return Err(AppError::Internal(
                e.context("insert global state snapshot record"),
            ));
        }
    };

    if let Err(e) = tx.commit().await {
        let _ = storage.delete(&key).await;
        return Err(AppError::Internal(
            anyhow::anyhow!(e).context("commit global state upload tx"),
        ));
    }

    // Delete pruned snapshots from the archive backend (best-effort, background).
    if !deleted_keys.is_empty() {
        let storage_clone = Arc::clone(storage);
        tokio::spawn(async move {
            for key in deleted_keys {
                if let Err(e) = storage_clone.delete(&key).await {
                    tracing::warn!(
                        "Failed to delete pruned global state snapshot {}: {:#}",
                        key,
                        e
                    );
                }
            }
        });
    }

    tracing::info!(
        workspace = %coords.workspace,
        task_name = %job.task_name,
        %job_id,
        bytes = body.len(),
        has_json = query.has_json,
        "Stored global state snapshot"
    );

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "snapshot_id": snapshot_id })),
    )
        .into_response())
}

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

    #[test]
    fn state_coords_of_job_uses_the_jobs_own_partition() {
        let mut job = stroem_db::JobRow::test_default();
        job.workspace = "etl".to_string();
        job.task_name = "nightly".to_string();
        job.git_ref = Some("release/2.3".to_string());
        assert_eq!(
            StateCoords::of_job(&job),
            StateCoords {
                workspace: "etl".to_string(),
                task_name: "nightly".to_string(),
                git_ref: Some("release/2.3".to_string()),
            }
        );
    }

    #[test]
    fn state_coords_from_path_is_the_null_partition() {
        assert_eq!(
            StateCoords::from_path("billing", "export"),
            StateCoords {
                workspace: "billing".to_string(),
                task_name: "export".to_string(),
                git_ref: None,
            }
        );
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
    Query(query): Query<StateDownloadQuery>,
) -> Result<impl IntoResponse, AppError> {
    let storage = state
        .state_storage
        .as_ref()
        .ok_or_else(|| AppError::NotFound("State storage not configured".into()))?;

    let coords = download_coords(&state, &workspace, &task_name, query.job_id).await?;
    let snapshot = match stroem_db::TaskStateRepo::get_latest_for_ref(
        &state.pool,
        &coords.workspace,
        &coords.task_name,
        coords.git_ref.as_deref(),
    )
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
/// The partition is the job's own `(workspace, task, ref)`, not the path's.
///
/// Old snapshots beyond the configured retention limit are pruned from
/// both the DB and the archive backend (best-effort, in the background).
#[tracing::instrument(skip(state, body))]
pub async fn upload_state(
    State(state): State<Arc<AppState>>,
    Path((_path_workspace, _path_task, job_id)): Path<(String, String, Uuid)>,
    Query(query): Query<UploadStateQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let storage = state
        .state_storage
        .as_ref()
        .ok_or_else(|| AppError::NotFound("State storage not configured".into()))?;

    // The job, not the path, decides the partition (spec § 7.6). A
    // cross-workspace step's worker sends the action owner's workspace in
    // the path; that used to answer 400 and lose the state.
    let job = load_job(&state, job_id).await?;
    let coords = StateCoords::of_job(&job);

    // Build storage key and persist the bytes.
    let key = storage.storage_key(&coords.workspace, &coords.task_name, job_id);
    storage
        .store(&key, &body)
        .await
        .context("store state snapshot")?;

    // Persist the parsed sidecar (migration 047) using exactly the gate and
    // extractor the claim path used to apply on every claim.
    let state_json = if query.has_json {
        extract_state_json(&body)
    } else {
        None
    };

    // Record the snapshot and prune old ones atomically inside a transaction
    // owned here. If anything fails, delete the blob we just uploaded so no
    // orphaned data is left behind.
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            let _ = storage.delete(&key).await;
            return Err(AppError::Internal(
                anyhow::anyhow!(e).context("begin state upload tx"),
            ));
        }
    };

    let (snapshot_id, deleted_keys) = match stroem_db::TaskStateRepo::insert_and_prune_for_ref(
        &mut tx,
        &coords.workspace,
        &coords.task_name,
        coords.git_ref.as_deref(),
        job_id,
        &key,
        body.len() as i64,
        query.has_json,
        state_json.as_ref(),
        storage.max_snapshots(),
        None,
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

    if let Err(e) = tx.commit().await {
        let _ = storage.delete(&key).await;
        return Err(AppError::Internal(
            anyhow::anyhow!(e).context("commit state upload tx"),
        ));
    }

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
        workspace = %coords.workspace,
        task_name = %coords.task_name,
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
