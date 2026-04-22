//! Admin/operator endpoints for uploading state snapshots out-of-band.
//!
//! See `docs/internal/2026-04-21-state-upload-design.md`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::Read;

use anyhow::{anyhow, Context, Result};

/// Build the JSON map that becomes `state.json` from the upload's query
/// string. The reserved `mode` key is filtered out; any other key/value
/// pair is copied verbatim as a string. Duplicate keys: last wins (already
/// how Axum's `Query<HashMap>` delivers them, but documenting here anyway).
///
/// Returns `None` if the resulting map is empty, so callers can decide
/// whether to emit a `state.json` entry at all.
pub fn build_state_json_from_params(
    params: &BTreeMap<String, String>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    for (k, v) in params {
        if k == "mode" {
            continue;
        }
        out.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Maximum decompressed snapshot size (50 MB). Applies both pre-pack (incoming
/// uploaded tarball) and post-pack (merged tarball) — the merged form must
/// still fit once the prior snapshot is overlaid.
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 50 * 1024 * 1024;

/// Sentinel `task_name` used in the synthetic upload job row and in
/// `workspace_state.written_by_task` for global workspace state uploads.
pub(crate) const GLOBAL_TASK_NAME_SENTINEL: &str = "_global_state";

/// Build the bytes of a new snapshot tarball given an optional prior
/// tarball, the uploaded tarball, and the state.json key/value pairs from
/// query params.
///
/// Returns `(bytes, has_json)` where `has_json` is `true` when the resulting
/// tarball contains a root-level `state.json`. This avoids a redundant
/// decompress pass in the callers.
///
/// Algorithm:
/// 1. If `prior` is `Some`: unpack into `files: HashMap<String, Vec<u8>>`.
///    Else: start empty. This is how the handler signals `mode=replace`:
///    pass `None` for `prior` regardless of whether one exists in the DB.
/// 2. Unpack `uploaded` into a temp map; reject with error if it contains
///    a root-level `state.json` entry.
/// 3. Overlay: files_from_upload overwrite files_from_prior at the same path.
/// 4. Compute state.json:
///    - Start from `files["state.json"]` if prior had one and it parsed
///      successfully; else empty object.
///    - Shallow-merge `state_params` on top (string values; new wins).
///    - If non-empty: set `files["state.json"]` to the serialised JSON.
/// 5. Repack `files` in deterministic order into a new gzip tarball.
/// 6. If the repacked output exceeds `MAX_SNAPSHOT_BYTES`, return an error.
#[allow(dead_code)]
pub fn build_snapshot(
    prior: Option<&[u8]>,
    uploaded: &[u8],
    state_params: &BTreeMap<String, String>,
) -> Result<(Vec<u8>, bool)> {
    // Step 1: unpack prior (if any)
    let mut files: HashMap<String, Vec<u8>> = match prior {
        Some(bytes) => unpack_tarball(bytes).context("unpack prior snapshot")?,
        None => HashMap::new(),
    };

    // Step 2: unpack uploaded
    let uploaded_files = unpack_tarball(uploaded).context("unpack uploaded tarball")?;

    // Reject root-level state.json inside uploaded tarball (contract: state
    // values must arrive via query params).
    if uploaded_files.contains_key("state.json") {
        return Err(anyhow!(
            "Uploaded tarball must not contain a root-level state.json; pass state values via query parameters"
        ));
    }

    // Step 3: overlay
    for (path, bytes) in uploaded_files {
        files.insert(path, bytes);
    }

    // Step 4: compute state.json
    let mut merged_state: serde_json::Map<String, serde_json::Value> = match files.get("state.json")
    {
        Some(bytes) => serde_json::from_slice(bytes).unwrap_or_default(),
        None => serde_json::Map::new(),
    };

    let new_state = build_state_json_from_params(state_params);
    if let Some(new) = new_state {
        for (k, v) in new {
            merged_state.insert(k, v);
        }
    }

    if merged_state.is_empty() {
        files.remove("state.json");
    } else {
        let serialised =
            serde_json::to_vec(&merged_state).context("serialise merged state.json")?;
        files.insert("state.json".to_string(), serialised);
    }

    // Step 5: repack (deterministic order so tests can assert on bytes shape)
    let has_json = files.contains_key("state.json");
    let repacked = pack_tarball(&files)?;

    // Step 6: size guard
    if repacked.len() > MAX_SNAPSHOT_BYTES {
        return Err(anyhow!(
            "Merged snapshot exceeds {} bytes; use mode=replace or reduce payload",
            MAX_SNAPSHOT_BYTES
        ));
    }

    Ok((repacked, has_json))
}

/// Unpack a gzip tarball into a map of `path -> bytes`.
///
/// - Only regular files are kept; directories, symlinks, and hardlinks are ignored.
/// - Paths are normalised to forward slashes with no leading `./`.
/// - An empty tarball (zero file entries) is valid and returns an empty map.
/// - Empty input bytes return an empty map (no error).
/// - Invalid gzip or tar framing returns an error.
/// - Entries with path-traversal components (`..`) or absolute paths are rejected.
/// - Total decompressed size is capped at `MAX_SNAPSHOT_BYTES`; exceeding it is an error.
#[allow(dead_code)]
pub fn unpack_tarball(bytes: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    if bytes.is_empty() {
        return Ok(HashMap::new());
    }

    let decoder = GzDecoder::new(bytes);
    let mut archive = Archive::new(decoder);
    let mut out = HashMap::new();
    let mut total_bytes: usize = 0;

    for entry in archive.entries().context("read tar entries")? {
        let entry = entry.context("read tar entry")?;
        let header_type = entry.header().entry_type();
        if !header_type.is_file() {
            continue;
        }
        let path = entry
            .path()
            .context("read entry path")?
            .to_string_lossy()
            .trim_start_matches("./")
            .to_string();
        if path.is_empty() {
            continue;
        }

        // Reject path traversal / absolute paths. `..` as a path component
        // or a leading `/` could escape when this tarball is later extracted
        // by a consumer that doesn't have its own boundary.
        if path.starts_with('/') || path.split('/').any(|c| c == "..") {
            return Err(anyhow!("Tarball contains unsafe path: {}", path));
        }

        // Bounded read — reject if cumulative decompressed size exceeds the cap.
        let remaining = MAX_SNAPSHOT_BYTES
            .checked_sub(total_bytes)
            .ok_or_else(|| anyhow!("Tarball decompresses beyond size cap"))?;
        let mut limited = entry.take(remaining as u64 + 1);
        let mut buf = Vec::new();
        limited.read_to_end(&mut buf).context("read entry bytes")?;
        if buf.len() > remaining {
            return Err(anyhow!(
                "Tarball decompresses beyond {} byte cap",
                MAX_SNAPSHOT_BYTES
            ));
        }
        total_bytes += buf.len();
        out.insert(path, buf);
    }

    Ok(out)
}

/// Pack a map of `path -> bytes` into a gzip tarball.
///
/// Entries are sorted lexicographically so the output is deterministic.
#[allow(dead_code)]
pub fn pack_tarball(files: &HashMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        let mut entries: Vec<(&String, &Vec<u8>)> = files.iter().collect();
        entries.sort_by_key(|(k, _)| k.as_str());
        for (path, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            builder
                .append_data(&mut header, path, &bytes[..])
                .context("append tar entry")?;
        }
        builder.finish().context("finish tar")?;
    }
    encoder.finish().context("finish gzip")
}

// ─── HTTP handler ────────────────────────────────────────────────────────────

use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::error::AppError;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use uuid::Uuid;

/// Compute SHA-256 of the given bytes, return lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadMode {
    Replace,
    Merge,
}

/// Parse the `?mode=` query param. Defaults to `replace`. Rejects unknown values.
fn parse_mode(params: &BTreeMap<String, String>) -> Result<UploadMode, AppError> {
    match params.get("mode").map(|s| s.as_str()) {
        None | Some("replace") => Ok(UploadMode::Replace),
        Some("merge") => Ok(UploadMode::Merge),
        Some(other) => Err(AppError::BadRequest(format!(
            "Invalid mode '{}': must be 'replace' or 'merge'",
            other
        ))),
    }
}

/// Format the audit-trail `source_id` for a state upload.
///
/// - Auth disabled → `None` (anonymous upload, no attribution).
/// - Auth enabled + API key → `"api_key:{prefix}"` (e.g. `"api_key:strm_a1b2c3d"`).
/// - Auth enabled + JWT session → `"user:{email}"`.
/// - Auth enabled + no user → authentication required error.
fn format_source_id(user: &AuthUser) -> String {
    if user.is_api_key {
        format!(
            "api_key:{}",
            user.api_key_prefix.as_deref().unwrap_or("unknown")
        )
    } else {
        format!("user:{}", user.claims.email)
    }
}

fn resolve_source_id(
    state: &std::sync::Arc<AppState>,
    auth_user: &Option<AuthUser>,
) -> Result<Option<String>, AppError> {
    match (state.config.auth.is_some(), auth_user) {
        (false, _) => Ok(None),
        (true, Some(user)) => Ok(Some(format_source_id(user))),
        (true, None) => Err(AppError::Unauthorized("Authentication required".into())),
    }
}

async fn check_run_permission(
    state: &std::sync::Arc<AppState>,
    auth_user: &Option<AuthUser>,
    ws: &str,
    task_name: &str,
    task_folder: Option<&str>,
) -> Result<(), AppError> {
    use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};

    if state.acl.is_configured() {
        let auth = auth_user
            .as_ref()
            .ok_or_else(|| AppError::Unauthorized("Authentication required".into()))?;
        let user_id = auth.user_id()?;
        let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
            .await
            .context("load ACL context")
            .map_err(AppError::Internal)?;
        let task_path = make_task_path(task_folder, task_name);
        match state
            .acl
            .evaluate(ws, &task_path, &auth.claims.email, &groups, is_admin)
        {
            TaskPermission::Deny => return Err(AppError::not_found("Task")),
            TaskPermission::View => return Err(AppError::Forbidden("View-only access".into())),
            TaskPermission::Run => {} // allowed
        }
    }
    Ok(())
}

/// POST /api/workspaces/{ws}/tasks/{task}/state — upload a state snapshot for a task.
///
/// See `docs/internal/2026-04-21-state-upload-design.md`.
#[tracing::instrument(skip(state, auth_user, body, params))]
pub async fn upload_task_state(
    State(state): State<std::sync::Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((ws, task)): Path<(String, String)>,
    Query(params): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Auth + source_id
    let source_id = resolve_source_id(&state, &auth_user)?;

    // Workspace + task exist
    let workspace = crate::web::api::get_workspace_or_error(&state, &ws).await?;
    let task_def = workspace
        .tasks
        .get(&task)
        .ok_or_else(|| AppError::not_found("Task"))?;

    // ACL: Run permission required
    check_run_permission(&state, &auth_user, &ws, &task, task_def.folder.as_deref()).await?;

    // State storage configured
    let storage = state
        .state_storage
        .as_ref()
        .ok_or_else(|| AppError::NotFound("State storage not configured".into()))?;

    // Parse mode
    let mode = parse_mode(&params)?;

    // Body can be empty → synthesise an empty tarball
    let uploaded_bytes: Vec<u8> = if body.is_empty() {
        pack_tarball(&HashMap::new()).map_err(AppError::Internal)?
    } else {
        body.to_vec()
    };

    // Prior snapshot bytes (merge mode only)
    let prior_bytes: Option<Vec<u8>> = if matches!(mode, UploadMode::Merge) {
        match stroem_db::TaskStateRepo::get_latest(&state.pool, &ws, &task)
            .await
            .map_err(AppError::Internal)?
        {
            Some(row) => {
                let bytes = storage
                    .retrieve(&row.storage_key)
                    .await
                    .map_err(AppError::Internal)?;
                if let Some(ref b) = bytes {
                    if b.len() > MAX_SNAPSHOT_BYTES {
                        return Err(AppError::Internal(anyhow!(
                            "Prior snapshot at {} exceeds size cap ({} bytes)",
                            row.storage_key,
                            b.len()
                        )));
                    }
                }
                bytes
            }
            None => None,
        }
    } else {
        None
    };

    // Build the new snapshot bytes (also validates no root-level state.json in upload)
    let (new_bytes, has_json) = build_snapshot(prior_bytes.as_deref(), &uploaded_bytes, &params)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Archive write (followed by DB tx; compensating delete on failure)
    let job_id = Uuid::new_v4();
    let key = storage.storage_key(&ws, &task, job_id);
    storage
        .store(&key, &new_bytes)
        .await
        .map_err(AppError::Internal)?;

    // DB tx: synthetic job + state row + prune
    let result = commit_task_upload(
        &state.pool,
        &ws,
        &task,
        &key,
        &new_bytes,
        has_json,
        mode,
        source_id.as_deref(),
        state.workspaces.get_revision(&ws).as_deref(),
        storage.max_snapshots(),
        job_id,
    )
    .await;

    let (snapshot_id, deleted_keys) = match result {
        Ok(r) => r,
        Err(e) => {
            // Compensating delete
            let _ = storage.delete(&key).await;
            return Err(AppError::Internal(e));
        }
    };

    // Background-delete pruned storage keys
    if !deleted_keys.is_empty() {
        let storage_clone = std::sync::Arc::clone(storage);
        tokio::spawn(async move {
            for k in deleted_keys {
                if let Err(e) = storage_clone.delete(&k).await {
                    tracing::warn!("Failed to delete pruned upload snapshot {}: {:#}", k, e);
                }
            }
        });
    }

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "snapshot_id": snapshot_id,
            "job_id": job_id,
        })),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn commit_task_upload(
    pool: &sqlx::PgPool,
    ws: &str,
    task: &str,
    key: &str,
    new_bytes: &[u8],
    has_json: bool,
    mode: UploadMode,
    source_id: Option<&str>,
    revision: Option<&str>,
    max_snapshots: usize,
    job_id: Uuid,
) -> Result<(Uuid, Vec<String>)> {
    let sha = sha256_hex(new_bytes);
    let mode_str = match mode {
        UploadMode::Replace => "replace",
        UploadMode::Merge => "merge",
    };
    let input = serde_json::json!({
        "upload": {
            "size_bytes": new_bytes.len(),
            "sha256": sha,
            "has_json": has_json,
            "uploaded_by": source_id,
            "mode": mode_str,
        }
    });

    let mut tx = pool.begin().await.context("begin upload tx")?;

    sqlx::query(
        r#"
        INSERT INTO job (
            job_id, workspace, task_name, mode, input, output,
            status, source_type, source_id, revision,
            started_at, completed_at
        )
        VALUES ($1, $2, $3, 'distributed', $4, NULL,
                'completed', 'upload', $5, $6,
                NOW(), NOW())
        "#,
    )
    .bind(job_id)
    .bind(ws)
    .bind(task)
    .bind(&input)
    .bind(source_id)
    .bind(revision)
    .execute(&mut *tx)
    .await
    .context("insert synthetic upload job")?;

    let snapshot_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO task_state (id, workspace, task_name, job_id, storage_key, size_bytes, has_json)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(snapshot_id)
    .bind(ws)
    .bind(task)
    .bind(job_id)
    .bind(key)
    .bind(new_bytes.len() as i64)
    .bind(has_json)
    .execute(&mut *tx)
    .await
    .context("insert task_state")?;

    let deleted_keys: Vec<String> = sqlx::query_scalar(
        r#"
        DELETE FROM task_state
        WHERE (workspace, task_name) = ($1, $2)
          AND id IN (
            SELECT id FROM task_state
            WHERE (workspace, task_name) = ($1, $2)
            ORDER BY created_at DESC, id DESC
            OFFSET $3
          )
        RETURNING storage_key
        "#,
    )
    .bind(ws)
    .bind(task)
    .bind(max_snapshots as i64)
    .fetch_all(&mut *tx)
    .await
    .context("prune task_state")?;

    sqlx::query("UPDATE job SET output = $1 WHERE job_id = $2")
        .bind(serde_json::json!({"snapshot_id": snapshot_id}))
        .bind(job_id)
        .execute(&mut *tx)
        .await
        .context("update synthetic job output")?;

    tx.commit().await.context("commit upload tx")?;

    Ok((snapshot_id, deleted_keys))
}

/// POST /api/workspaces/{ws}/state — upload a global workspace state snapshot (admin only).
#[tracing::instrument(skip(state, auth_user, body, params))]
pub async fn upload_global_state(
    State(state): State<std::sync::Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(ws): Path<String>,
    Query(params): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Auth + admin required when auth is enabled
    let source_id = resolve_source_id(&state, &auth_user)?;
    if state.config.auth.is_some() {
        let auth = auth_user
            .as_ref()
            .ok_or_else(|| AppError::Unauthorized("Authentication required".into()))?;
        if !auth.is_admin() {
            return Err(AppError::Forbidden(
                "Admin privileges required for global-state upload".into(),
            ));
        }
    }

    // Workspace exists
    let _workspace = crate::web::api::get_workspace_or_error(&state, &ws).await?;

    // Storage configured
    let storage = state
        .state_storage
        .as_ref()
        .ok_or_else(|| AppError::NotFound("State storage not configured".into()))?;

    // Parse mode
    let mode = parse_mode(&params)?;

    // Body → bytes, empty synthesised
    let uploaded_bytes: Vec<u8> = if body.is_empty() {
        pack_tarball(&HashMap::new()).map_err(AppError::Internal)?
    } else {
        body.to_vec()
    };

    // Prior snapshot (merge mode only)
    let prior_bytes: Option<Vec<u8>> = if matches!(mode, UploadMode::Merge) {
        match stroem_db::WorkspaceStateRepo::get_latest(&state.pool, &ws)
            .await
            .map_err(AppError::Internal)?
        {
            Some(row) => {
                let bytes = storage
                    .retrieve(&row.storage_key)
                    .await
                    .map_err(AppError::Internal)?;
                if let Some(ref b) = bytes {
                    if b.len() > MAX_SNAPSHOT_BYTES {
                        return Err(AppError::Internal(anyhow!(
                            "Prior snapshot at {} exceeds size cap ({} bytes)",
                            row.storage_key,
                            b.len()
                        )));
                    }
                }
                bytes
            }
            None => None,
        }
    } else {
        None
    };

    // Build new snapshot bytes (also rejects root state.json in upload)
    let (new_bytes, has_json) = build_snapshot(prior_bytes.as_deref(), &uploaded_bytes, &params)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Archive write
    let job_id = Uuid::new_v4();
    let key = storage.global_storage_key(&ws, job_id);
    storage
        .store(&key, &new_bytes)
        .await
        .map_err(AppError::Internal)?;

    // DB tx
    let result = commit_global_upload(
        &state.pool,
        &ws,
        &key,
        &new_bytes,
        has_json,
        mode,
        source_id.as_deref(),
        state.workspaces.get_revision(&ws).as_deref(),
        storage.global_max_snapshots(),
        job_id,
    )
    .await;

    let (snapshot_id, deleted_keys) = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = storage.delete(&key).await;
            return Err(AppError::Internal(e));
        }
    };

    // Background prune cleanup
    if !deleted_keys.is_empty() {
        let storage_clone = std::sync::Arc::clone(storage);
        tokio::spawn(async move {
            for k in deleted_keys {
                if let Err(e) = storage_clone.delete(&k).await {
                    tracing::warn!(
                        "Failed to delete pruned global upload snapshot {}: {:#}",
                        k,
                        e
                    );
                }
            }
        });
    }

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "snapshot_id": snapshot_id,
            "job_id": job_id,
        })),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn commit_global_upload(
    pool: &sqlx::PgPool,
    ws: &str,
    key: &str,
    new_bytes: &[u8],
    has_json: bool,
    mode: UploadMode,
    source_id: Option<&str>,
    revision: Option<&str>,
    max_snapshots: usize,
    job_id: Uuid,
) -> Result<(Uuid, Vec<String>)> {
    let sha = sha256_hex(new_bytes);
    let mode_str = match mode {
        UploadMode::Replace => "replace",
        UploadMode::Merge => "merge",
    };
    let input = serde_json::json!({
        "upload": {
            "size_bytes": new_bytes.len(),
            "sha256": sha,
            "has_json": has_json,
            "uploaded_by": source_id,
            "mode": mode_str,
        }
    });

    let mut tx = pool.begin().await.context("begin global upload tx")?;

    sqlx::query(
        r#"
        INSERT INTO job (
            job_id, workspace, task_name, mode, input, output,
            status, source_type, source_id, revision,
            started_at, completed_at
        )
        VALUES ($1, $2, $3, 'distributed', $4, NULL,
                'completed', 'upload', $5, $6,
                NOW(), NOW())
        "#,
    )
    .bind(job_id)
    .bind(ws)
    .bind(GLOBAL_TASK_NAME_SENTINEL)
    .bind(&input)
    .bind(source_id)
    .bind(revision)
    .execute(&mut *tx)
    .await
    .context("insert synthetic upload job (global)")?;

    let snapshot_id = Uuid::new_v4();

    sqlx::query(
        r#"
        INSERT INTO workspace_state (id, workspace, written_by_task, job_id, storage_key, size_bytes, has_json)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(snapshot_id)
    .bind(ws)
    .bind(GLOBAL_TASK_NAME_SENTINEL)
    .bind(job_id)
    .bind(key)
    .bind(new_bytes.len() as i64)
    .bind(has_json)
    .execute(&mut *tx)
    .await
    .context("insert workspace_state")?;

    let deleted_keys: Vec<String> = sqlx::query_scalar(
        r#"
        DELETE FROM workspace_state
        WHERE workspace = $1
          AND id IN (
            SELECT id FROM workspace_state
            WHERE workspace = $1
            ORDER BY created_at DESC, id DESC
            OFFSET $2
          )
        RETURNING storage_key
        "#,
    )
    .bind(ws)
    .bind(max_snapshots as i64)
    .fetch_all(&mut *tx)
    .await
    .context("prune workspace_state")?;

    sqlx::query("UPDATE job SET output = $1 WHERE job_id = $2")
        .bind(serde_json::json!({"snapshot_id": snapshot_id}))
        .bind(job_id)
        .execute(&mut *tx)
        .await
        .context("update synthetic global job output")?;

    tx.commit().await.context("commit global upload tx")?;

    Ok((snapshot_id, deleted_keys))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn empty_params_returns_none() {
        assert!(build_state_json_from_params(&params(&[])).is_none());
    }

    #[test]
    fn only_mode_returns_none() {
        assert!(build_state_json_from_params(&params(&[("mode", "merge")])).is_none());
    }

    #[test]
    fn single_param_becomes_string_entry() {
        let out = build_state_json_from_params(&params(&[("domain", "example.com")])).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out["domain"],
            serde_json::Value::String("example.com".into())
        );
    }

    #[test]
    fn mode_is_filtered_out() {
        let out = build_state_json_from_params(&params(&[
            ("mode", "replace"),
            ("domain", "example.com"),
        ]))
        .unwrap();
        assert!(!out.contains_key("mode"));
        assert_eq!(
            out["domain"],
            serde_json::Value::String("example.com".into())
        );
    }

    #[test]
    fn empty_value_is_preserved() {
        let out = build_state_json_from_params(&params(&[("foo", "")])).unwrap();
        assert_eq!(out["foo"], serde_json::Value::String(String::new()));
    }

    #[test]
    fn values_are_always_strings() {
        let out = build_state_json_from_params(&params(&[("days", "30"), ("ok", "true")])).unwrap();
        assert_eq!(out["days"], serde_json::Value::String("30".into()));
        assert_eq!(out["ok"], serde_json::Value::String("true".into()));
    }

    /// Build a gzip tarball from (path, bytes) pairs for test fixtures.
    fn make_tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let map: HashMap<String, Vec<u8>> = files
            .iter()
            .map(|(p, b)| ((*p).to_string(), b.to_vec()))
            .collect();
        pack_tarball(&map).unwrap()
    }

    /// Extract file bytes from a gzip tarball by path (test helper).
    fn read_file(tarball: &[u8], path: &str) -> Option<Vec<u8>> {
        unpack_tarball(tarball).unwrap().remove(path)
    }

    /// List all paths in a tarball (test helper).
    fn list_paths(tarball: &[u8]) -> Vec<String> {
        let mut keys: Vec<String> = unpack_tarball(tarball).unwrap().into_keys().collect();
        keys.sort();
        keys
    }

    #[test]
    fn build_snapshot_no_prior_no_uploaded_no_params_is_empty() {
        let uploaded = make_tarball(&[]);
        let (out, has_json) = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap();
        assert!(list_paths(&out).is_empty());
        assert!(!has_json);
    }

    #[test]
    fn build_snapshot_no_prior_with_file_and_state() {
        let uploaded = make_tarball(&[("cert.pem", b"PEMBYTES")]);
        let params = params(&[("domain", "example.com")]);
        let (out, has_json) = build_snapshot(None, &uploaded, &params).unwrap();
        assert_eq!(list_paths(&out), vec!["cert.pem", "state.json"]);
        assert_eq!(read_file(&out, "cert.pem"), Some(b"PEMBYTES".to_vec()));
        let state: serde_json::Value =
            serde_json::from_slice(&read_file(&out, "state.json").unwrap()).unwrap();
        assert_eq!(state["domain"], "example.com");
        assert!(has_json);
    }

    #[test]
    fn build_snapshot_replace_drops_prior_files() {
        // Prior contained a file, uploaded doesn't; replace mode = no prior passed.
        let _prior = make_tarball(&[("old.pem", b"OLD")]);
        let uploaded = make_tarball(&[("new.pem", b"NEW")]);
        let (out, has_json) = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap();
        assert_eq!(list_paths(&out), vec!["new.pem"]);
        assert!(!has_json);
    }

    #[test]
    fn build_snapshot_merge_preserves_prior_files() {
        let prior = make_tarball(&[("keep.pem", b"KEEP"), ("replace.pem", b"OLD")]);
        let uploaded = make_tarball(&[("replace.pem", b"NEW"), ("add.pem", b"ADD")]);
        let (out, has_json) = build_snapshot(Some(&prior), &uploaded, &BTreeMap::new()).unwrap();
        assert_eq!(list_paths(&out), vec!["add.pem", "keep.pem", "replace.pem"]);
        assert_eq!(read_file(&out, "replace.pem"), Some(b"NEW".to_vec()));
        assert_eq!(read_file(&out, "keep.pem"), Some(b"KEEP".to_vec()));
        assert!(!has_json);
    }

    #[test]
    fn build_snapshot_merge_shallow_merges_state_json() {
        let prior_state = serde_json::json!({ "domain": "old.com", "keep": "yes" });
        let prior_state_bytes = serde_json::to_vec(&prior_state).unwrap();
        let prior = make_tarball(&[("state.json", prior_state_bytes.as_slice())]);
        let uploaded = make_tarball(&[]);
        let params = params(&[("domain", "new.com"), ("extra", "42")]);
        let (out, has_json) = build_snapshot(Some(&prior), &uploaded, &params).unwrap();
        let state: serde_json::Value =
            serde_json::from_slice(&read_file(&out, "state.json").unwrap()).unwrap();
        assert_eq!(state["domain"], "new.com"); // overwritten
        assert_eq!(state["keep"], "yes"); // preserved
        assert_eq!(state["extra"], "42"); // added
        assert!(has_json);
    }

    #[test]
    fn build_snapshot_rejects_root_state_json_in_upload() {
        let uploaded = make_tarball(&[("state.json", b"{}")]);
        let err = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap_err();
        assert!(err
            .to_string()
            .contains("must not contain a root-level state.json"));
    }

    #[test]
    fn build_snapshot_allows_nested_state_json() {
        let uploaded = make_tarball(&[("subdir/state.json", b"{}")]);
        let (out, has_json) = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap();
        assert!(read_file(&out, "subdir/state.json").is_some());
        assert!(!has_json, "nested state.json does not set has_json");
    }

    #[test]
    fn build_snapshot_merge_with_no_prior_behaves_as_replace() {
        let uploaded = make_tarball(&[("f.txt", b"bytes")]);
        let (out, has_json) = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap();
        assert_eq!(list_paths(&out), vec!["f.txt"]);
        assert!(!has_json);
    }

    #[test]
    fn format_source_id_api_key() {
        use crate::web::api::middleware::AuthUser;
        use stroem_common::models::auth::Claims;
        let now = chrono::Utc::now().timestamp();
        let user = AuthUser {
            claims: Claims {
                sub: "user-1".into(),
                email: "test@example.com".into(),
                is_admin: false,
                iat: now,
                exp: now + 3600,
            },
            is_api_key: true,
            api_key_prefix: Some("strm_a1b2c3d".into()),
        };
        let sid = format_source_id(&user);
        assert_eq!(sid, "api_key:strm_a1b2c3d");
        assert!(sid.starts_with("api_key:"));
    }

    #[test]
    fn format_source_id_jwt_user() {
        use crate::web::api::middleware::AuthUser;
        use stroem_common::models::auth::Claims;
        let now = chrono::Utc::now().timestamp();
        let user = AuthUser {
            claims: Claims {
                sub: "user-2".into(),
                email: "ala@allunite.com".into(),
                is_admin: false,
                iat: now,
                exp: now + 900,
            },
            is_api_key: false,
            api_key_prefix: None,
        };
        let sid = format_source_id(&user);
        assert_eq!(sid, "user:ala@allunite.com");
        assert!(sid.starts_with("user:"));
    }

    #[test]
    fn format_source_id_api_key_missing_prefix_falls_back() {
        use crate::web::api::middleware::AuthUser;
        use stroem_common::models::auth::Claims;
        let now = chrono::Utc::now().timestamp();
        let user = AuthUser {
            claims: Claims {
                sub: "user-3".into(),
                email: "x@example.com".into(),
                is_admin: false,
                iat: now,
                exp: now + 3600,
            },
            is_api_key: true,
            api_key_prefix: None, // edge case: shouldn't happen, but guard is there
        };
        let sid = format_source_id(&user);
        assert_eq!(sid, "api_key:unknown");
    }

    /// Build a raw gzip tarball where the path is written directly into the
    /// header bytes without the `tar` crate's path-safety checks.
    /// This lets us craft hostile entries (`../foo`, `/etc/shadow`) that the
    /// real `tar` crate refuses to produce via its safe API.
    fn make_hostile_tarball(raw_path: &str, content: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        // Build a minimal POSIX/ustar tar block by hand.
        // A tar header is exactly 512 bytes; we need one header + data blocks.
        let block_size = 512usize;
        let data_blocks = content.len().div_ceil(block_size);
        let total_blocks = 1 + data_blocks + 2; // header + data + 2 end-of-archive blocks
        let mut tar_bytes = vec![0u8; total_blocks * block_size];

        // Name field: bytes 0..100, NUL-terminated
        let name_bytes = raw_path.as_bytes();
        let copy_len = name_bytes.len().min(99);
        tar_bytes[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        // Mode: bytes 100..108
        tar_bytes[100..107].copy_from_slice(b"0000644");
        tar_bytes[107] = 0;

        // UID/GID: bytes 108..124
        tar_bytes[108..115].copy_from_slice(b"0000000");
        tar_bytes[115] = 0;
        tar_bytes[116..123].copy_from_slice(b"0000000");
        tar_bytes[123] = 0;

        // File size in octal: bytes 124..136
        let size_str = format!("{:011o}\0", content.len());
        tar_bytes[124..136].copy_from_slice(size_str.as_bytes());

        // Mtime: bytes 136..148
        tar_bytes[136..147].copy_from_slice(b"00000000000");
        tar_bytes[147] = 0;

        // Type flag: '0' = regular file (byte 156)
        tar_bytes[156] = b'0';

        // Magic: bytes 257..265 ("ustar  \0")
        tar_bytes[257..265].copy_from_slice(b"ustar  \0");

        // Compute checksum: sum all bytes with checksum field (148..156) treated as spaces
        for b in tar_bytes[148..156].iter_mut() {
            *b = b' ';
        }
        let chksum: u32 = tar_bytes[..block_size].iter().map(|&b| b as u32).sum();
        let chksum_str = format!("{:06o}\0 ", chksum);
        tar_bytes[148..156].copy_from_slice(chksum_str.as_bytes());

        // Write file content into the data blocks
        tar_bytes[block_size..block_size + content.len()].copy_from_slice(content);

        // Gzip-compress the raw tar
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn unpack_tarball_rejects_path_traversal() {
        let uploaded = make_hostile_tarball("../escape.pem", b"bad");
        let err = unpack_tarball(&uploaded).unwrap_err();
        assert!(err.to_string().contains("unsafe path"));
    }

    #[test]
    fn unpack_tarball_rejects_absolute_path() {
        let uploaded = make_hostile_tarball("/etc/shadow", b"bad");
        let err = unpack_tarball(&uploaded).unwrap_err();
        assert!(err.to_string().contains("unsafe path"));
    }

    #[test]
    fn unpack_tarball_skips_symlinks() {
        // Build a tarball with a symlink entry and verify it's silently skipped.
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            // Regular file that should be kept
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(&mut header, "real.txt", &b"abc"[..])
                .unwrap();
            // Symlink that should be skipped
            let mut link_header = tar::Header::new_gnu();
            link_header.set_size(0);
            link_header.set_mode(0o777);
            link_header.set_entry_type(tar::EntryType::Symlink);
            link_header.set_link_name("real.txt").unwrap();
            link_header.set_cksum();
            builder
                .append_data(&mut link_header, "link.txt", &[][..])
                .unwrap();
            builder.finish().unwrap();
        }
        let bytes = encoder.finish().unwrap();
        let out = unpack_tarball(&bytes).unwrap();
        assert!(out.contains_key("real.txt"));
        assert!(!out.contains_key("link.txt"));
    }

    #[test]
    fn unpack_tarball_rejects_oversize_cumulative() {
        // Build a tarball whose total decompressed bytes exceed MAX_SNAPSHOT_BYTES.
        // Use pseudo-random bytes so gzip doesn't compress them to nothing.
        let size = MAX_SNAPSHOT_BYTES + 1;
        let mut data: Vec<u8> = Vec::with_capacity(size);
        // Simple LCG to generate incompressible-enough bytes without pulling in rand.
        let mut seed: u32 = 0xDEAD_BEEF;
        for _ in 0..size {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((seed >> 16) as u8);
        }
        let uploaded = make_tarball(&[("big.bin", data.as_slice())]);
        let err = unpack_tarball(&uploaded).unwrap_err();
        assert!(
            err.to_string().contains("size cap") || err.to_string().contains("beyond"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn build_snapshot_merge_preserves_state_json_with_no_params() {
        let prior_state = serde_json::json!({ "domain": "old.com", "keep": "yes" });
        let prior_state_bytes = serde_json::to_vec(&prior_state).unwrap();
        let prior = make_tarball(&[("state.json", prior_state_bytes.as_slice())]);
        let uploaded = make_tarball(&[]);
        let (out, has_json) = build_snapshot(Some(&prior), &uploaded, &BTreeMap::new()).unwrap();
        assert!(has_json);
        let state: serde_json::Value =
            serde_json::from_slice(&read_file(&out, "state.json").unwrap()).unwrap();
        assert_eq!(state["domain"], "old.com");
        assert_eq!(state["keep"], "yes");
    }

    #[test]
    fn build_snapshot_merge_empty_prior_with_params_creates_state_json() {
        let prior = make_tarball(&[("some.txt", b"data")]); // prior exists but no state.json
        let uploaded = make_tarball(&[]);
        let p = params(&[("domain", "example.com")]);
        let (out, has_json) = build_snapshot(Some(&prior), &uploaded, &p).unwrap();
        assert!(has_json);
        let state: serde_json::Value =
            serde_json::from_slice(&read_file(&out, "state.json").unwrap()).unwrap();
        assert_eq!(state["domain"], "example.com");
        // File from prior is preserved
        assert_eq!(read_file(&out, "some.txt"), Some(b"data".to_vec()));
    }
}
