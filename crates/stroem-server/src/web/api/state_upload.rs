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
    let mut merged_state: serde_json::Map<String, serde_json::Value> =
        match files.get("state.json") {
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
        assert_eq!(out["domain"], serde_json::Value::String("example.com".into()));
    }

    #[test]
    fn mode_is_filtered_out() {
        let out = build_state_json_from_params(&params(&[
            ("mode", "replace"),
            ("domain", "example.com"),
        ]))
        .unwrap();
        assert!(!out.contains_key("mode"));
        assert_eq!(out["domain"], serde_json::Value::String("example.com".into()));
    }

    #[test]
    fn empty_value_is_preserved() {
        let out = build_state_json_from_params(&params(&[("foo", "")])).unwrap();
        assert_eq!(out["foo"], serde_json::Value::String(String::new()));
    }

    #[test]
    fn values_are_always_strings() {
        let out =
            build_state_json_from_params(&params(&[("days", "30"), ("ok", "true")])).unwrap();
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
        assert!(err.to_string().contains("must not contain a root-level state.json"));
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
