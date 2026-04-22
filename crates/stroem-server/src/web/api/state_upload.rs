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
) -> Result<Vec<u8>> {
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
    let repacked = pack_tarball(&files)?;

    // Step 6: size guard
    if repacked.len() > MAX_SNAPSHOT_BYTES {
        return Err(anyhow!(
            "Merged snapshot exceeds {} bytes; use mode=replace or reduce payload",
            MAX_SNAPSHOT_BYTES
        ));
    }

    Ok(repacked)
}

/// Unpack a gzip tarball into a map of `path -> bytes`.
///
/// - Only regular files are kept; directories, symlinks, and hardlinks are ignored.
/// - Paths are normalised to forward slashes with no leading `./`.
/// - An empty tarball (zero file entries) is valid and returns an empty map.
/// - Empty input bytes return an empty map (no error).
/// - Invalid gzip or tar framing returns an error.
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

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
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
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf).context("read entry bytes")?;
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

/// Insert a synthetic job row to represent an out-of-band state upload.
///
/// The returned UUID is used as both the `job_id` foreign key on the
/// state-snapshot row and the filename in the archive's storage key.
///
/// Runs inside an existing transaction so the job insert and the
/// state-snapshot insert commit atomically (or roll back together).
///
/// See `docs/internal/2026-04-21-state-upload-design.md` § "Synthetic seed job".
#[allow(clippy::too_many_arguments)]
pub async fn insert_synthetic_upload_job(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: &str,
    task_name: &str,
    source_id: Option<&str>,
    input: serde_json::Value,
    output: serde_json::Value,
    revision: Option<&str>,
) -> Result<uuid::Uuid> {
    let job_id = uuid::Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO job (
            job_id, workspace, task_name, mode, input, output,
            status, source_type, source_id, revision,
            started_at, completed_at
        )
        VALUES ($1, $2, $3, 'distributed', $4, $5,
                'completed', 'upload', $6, $7,
                NOW(), NOW())
        "#,
    )
    .bind(job_id)
    .bind(workspace)
    .bind(task_name)
    .bind(&input)
    .bind(&output)
    .bind(source_id)
    .bind(revision)
    .execute(&mut **tx)
    .await
    .context("insert synthetic upload job")?;

    Ok(job_id)
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

/// Determine whether a tarball contains a root-level `state.json`.
fn has_root_state_json(bytes: &[u8]) -> bool {
    unpack_tarball(bytes)
        .map(|m| m.contains_key("state.json"))
        .unwrap_or(false)
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

fn resolve_source_id(
    state: &std::sync::Arc<AppState>,
    auth_user: &Option<AuthUser>,
) -> Result<Option<String>, AppError> {
    match (state.config.auth.is_some(), auth_user) {
        (false, _) => Ok(None),
        (true, Some(user)) => Ok(Some(format!("user:{}", user.claims.email))),
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

    if let Some(auth) = auth_user {
        if state.acl.is_configured() {
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
    }
    Ok(())
}

/// POST /api/workspaces/{ws}/tasks/{task}/state — upload a state snapshot for a task.
///
/// See `docs/internal/2026-04-21-state-upload-design.md`.
#[tracing::instrument(skip(state, auth_user, body))]
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
            Some(row) => storage
                .retrieve(&row.storage_key)
                .await
                .map_err(AppError::Internal)?,
            None => None,
        }
    } else {
        None
    };

    // Build the new snapshot bytes (also validates no root-level state.json in upload)
    let new_bytes = build_snapshot(prior_bytes.as_deref(), &uploaded_bytes, &params)
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
    mode: UploadMode,
    source_id: Option<&str>,
    revision: Option<&str>,
    max_snapshots: usize,
    job_id: Uuid,
) -> Result<(Uuid, Vec<String>)> {
    let sha = sha256_hex(new_bytes);
    let has_json = has_root_state_json(new_bytes);
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
#[tracing::instrument(skip(state, auth_user, body))]
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
            Some(row) => storage
                .retrieve(&row.storage_key)
                .await
                .map_err(AppError::Internal)?,
            None => None,
        }
    } else {
        None
    };

    // Build new snapshot bytes (also rejects root state.json in upload)
    let new_bytes = build_snapshot(prior_bytes.as_deref(), &uploaded_bytes, &params)
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
    mode: UploadMode,
    source_id: Option<&str>,
    revision: Option<&str>,
    max_snapshots: usize,
    job_id: Uuid,
) -> Result<(Uuid, Vec<String>)> {
    let sha = sha256_hex(new_bytes);
    let has_json = has_root_state_json(new_bytes);
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
        let out = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap();
        assert!(list_paths(&out).is_empty());
    }

    #[test]
    fn build_snapshot_no_prior_with_file_and_state() {
        let uploaded = make_tarball(&[("cert.pem", b"PEMBYTES")]);
        let params = params(&[("domain", "example.com")]);
        let out = build_snapshot(None, &uploaded, &params).unwrap();
        assert_eq!(list_paths(&out), vec!["cert.pem", "state.json"]);
        assert_eq!(read_file(&out, "cert.pem"), Some(b"PEMBYTES".to_vec()));
        let state: serde_json::Value =
            serde_json::from_slice(&read_file(&out, "state.json").unwrap()).unwrap();
        assert_eq!(state["domain"], "example.com");
    }

    #[test]
    fn build_snapshot_replace_drops_prior_files() {
        // Prior contained a file, uploaded doesn't; replace mode = no prior passed.
        let _prior = make_tarball(&[("old.pem", b"OLD")]);
        let uploaded = make_tarball(&[("new.pem", b"NEW")]);
        let out = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap();
        assert_eq!(list_paths(&out), vec!["new.pem"]);
    }

    #[test]
    fn build_snapshot_merge_preserves_prior_files() {
        let prior = make_tarball(&[("keep.pem", b"KEEP"), ("replace.pem", b"OLD")]);
        let uploaded = make_tarball(&[("replace.pem", b"NEW"), ("add.pem", b"ADD")]);
        let out = build_snapshot(Some(&prior), &uploaded, &BTreeMap::new()).unwrap();
        assert_eq!(list_paths(&out), vec!["add.pem", "keep.pem", "replace.pem"]);
        assert_eq!(read_file(&out, "replace.pem"), Some(b"NEW".to_vec()));
        assert_eq!(read_file(&out, "keep.pem"), Some(b"KEEP".to_vec()));
    }

    #[test]
    fn build_snapshot_merge_shallow_merges_state_json() {
        let prior_state = serde_json::json!({ "domain": "old.com", "keep": "yes" });
        let prior_state_bytes = serde_json::to_vec(&prior_state).unwrap();
        let prior = make_tarball(&[("state.json", prior_state_bytes.as_slice())]);
        let uploaded = make_tarball(&[]);
        let params = params(&[("domain", "new.com"), ("extra", "42")]);
        let out = build_snapshot(Some(&prior), &uploaded, &params).unwrap();
        let state: serde_json::Value =
            serde_json::from_slice(&read_file(&out, "state.json").unwrap()).unwrap();
        assert_eq!(state["domain"], "new.com"); // overwritten
        assert_eq!(state["keep"], "yes"); // preserved
        assert_eq!(state["extra"], "42"); // added
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
        let out = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap();
        assert!(read_file(&out, "subdir/state.json").is_some());
    }

    #[test]
    fn build_snapshot_merge_with_no_prior_behaves_as_replace() {
        let uploaded = make_tarball(&[("f.txt", b"bytes")]);
        let out = build_snapshot(None, &uploaded, &BTreeMap::new()).unwrap();
        assert_eq!(list_paths(&out), vec!["f.txt"]);
    }
}
