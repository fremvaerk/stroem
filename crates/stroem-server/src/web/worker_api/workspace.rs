use crate::state::AppState;
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::Deserialize;
use std::sync::Arc;

/// Optional query parameters for the workspace tarball download endpoint.
///
/// Workers that received a `revision` in the claim response should pass it
/// back here so the server can serve the exact pinned tarball, even if the
/// workspace has since been updated.
#[derive(Debug, Deserialize)]
pub struct WorkspaceQuery {
    /// Specific workspace revision the worker wants. When absent, the current
    /// live revision is served (backward-compatible behaviour).
    #[serde(default)]
    pub revision: Option<String>,
}

/// GET /worker/workspace/:ws.tar.gz - Download workspace as tarball
///
/// Accepts an optional `?revision=<rev>` query parameter. When present, the
/// server attempts to serve the exact revision requested:
///
/// - If the revision is cached on disk, the cached bytes are returned immediately
///   (subject to the normal `If-None-Match` 304 short-circuit).
/// - If the revision matches the workspace's current live revision, the tarball
///   is built from the filesystem, cached, and returned.
/// - If the revision is neither cached nor the current revision (i.e. it has
///   been superseded), a 404 is returned so the worker knows it cannot obtain
///   that revision and should abandon the step.
///
/// When `?revision` is absent the existing behaviour is preserved: the live
/// tarball is built and returned, and the result is also stored in the cache
/// under the current revision for future pinned requests.
#[tracing::instrument(skip(state, headers))]
pub async fn download_workspace(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(ws_filename): Path<String>,
    Query(query): Query<WorkspaceQuery>,
) -> Result<impl IntoResponse, AppError> {
    // Strip .tar.gz suffix to get workspace name
    let ws_name = ws_filename.strip_suffix(".tar.gz").unwrap_or(&ws_filename);

    // Ensure the workspace exists and is loadable before we do anything else.
    let workspace_path = state
        .workspaces
        .get_path(ws_name)
        .map(|p| p.to_path_buf())
        .ok_or_else(|| AppError::NotFound(format!("Workspace '{}' not found", ws_name)))?;

    let current_revision = state.workspaces.get_revision(ws_name);

    if let Some(requested_rev) = query.revision {
        // ── Revision-pinned path ──────────────────────────────────────────────
        //
        // Step 1: Check If-None-Match against the requested revision.
        if let Some(etag) = headers.get(header::IF_NONE_MATCH) {
            if etag
                .to_str()
                .map(|s| s.trim_matches('"') == requested_rev.as_str())
                .unwrap_or(false)
            {
                return Ok(StatusCode::NOT_MODIFIED.into_response());
            }
        }

        // Step 2: Try the on-disk cache.
        let cache = Arc::clone(&state.tarball_cache);
        let ws_name_owned = ws_name.to_owned();
        let requested_rev_clone = requested_rev.clone();

        let cached =
            tokio::task::spawn_blocking(move || cache.get(&ws_name_owned, &requested_rev_clone))
                .await
                .map_err(|e| {
                    AppError::Internal(anyhow::anyhow!(e).context("tarball cache get panicked"))
                })?;

        if let Some(tarball) = cached {
            tracing::debug!(
                workspace = %ws_name,
                revision = %requested_rev,
                "Serving revision-pinned tarball from cache"
            );
            return Ok(build_tarball_response(tarball, &requested_rev).into_response());
        }

        // Step 3: Cache miss — only build if the requested revision is still current.
        match &current_revision {
            Some(current_rev) if current_rev == &requested_rev => {
                // Requested revision is current — build, cache, and serve.
                let library_paths = state.workspaces.get_library_paths();
                let tarball = tokio::task::spawn_blocking(move || {
                    build_tarball(&workspace_path, &library_paths)
                })
                .await
                .map_err(|e| {
                    AppError::Internal(anyhow::anyhow!(e).context("tarball task panicked"))
                })?
                .context("build workspace tarball")?;

                // Store in cache (best-effort, fire-and-forget — a write failure does not abort the response).
                let cache = Arc::clone(&state.tarball_cache);
                let ws_name_owned = ws_name.to_owned();
                let rev_clone = requested_rev.clone();
                let tarball_clone = tarball.clone();
                std::mem::drop(tokio::task::spawn_blocking(move || {
                    if let Err(e) = cache.put(&ws_name_owned, &rev_clone, &tarball_clone) {
                        tracing::warn!(
                            workspace = %ws_name_owned,
                            revision = %rev_clone,
                            "Failed to cache tarball: {:#}",
                            e
                        );
                    }
                }));

                Ok(build_tarball_response(tarball, &requested_rev).into_response())
            }
            _ => {
                // Revision is neither cached nor current — it has been superseded.
                Err(AppError::NotFound(format!(
                    "Revision '{}' no longer available for workspace '{}'",
                    requested_rev, ws_name
                )))
            }
        }
    } else {
        // ── Backward-compatible live path ─────────────────────────────────────
        //
        // No revision pinning requested. Build the tarball from the live
        // filesystem, then opportunistically store it in the cache under the
        // current revision so that future pinned requests can be served cheaply.

        // Check If-None-Match against the current revision.
        if let Some(etag) = headers.get(header::IF_NONE_MATCH) {
            if let Ok(etag_str) = etag.to_str() {
                if let Some(ref rev) = current_revision {
                    if etag_str.trim_matches('"') == rev.as_str() {
                        return Ok(StatusCode::NOT_MODIFIED.into_response());
                    }
                }
            }
        }

        let library_paths = state.workspaces.get_library_paths();
        let tarball = match tokio::task::spawn_blocking(move || {
            build_tarball(&workspace_path, &library_paths)
        })
        .await
        {
            Ok(result) => result.context("build workspace tarball")?,
            Err(e) => {
                return Err(AppError::Internal(
                    anyhow::anyhow!(e).context("tarball task panicked"),
                ));
            }
        };

        // Opportunistically cache under the current revision (fire-and-forget).
        if let Some(ref rev) = current_revision {
            let cache = Arc::clone(&state.tarball_cache);
            let ws_name_owned = ws_name.to_owned();
            let rev_clone = rev.clone();
            let tarball_clone = tarball.clone();
            std::mem::drop(tokio::task::spawn_blocking(move || {
                if let Err(e) = cache.put(&ws_name_owned, &rev_clone, &tarball_clone) {
                    tracing::warn!(
                        workspace = %ws_name_owned,
                        revision = %rev_clone,
                        "Failed to cache tarball: {:#}",
                        e
                    );
                }
            }));
        }

        let mut response_headers = HeaderMap::new();
        response_headers.insert(header::CONTENT_TYPE, "application/gzip".parse().unwrap());
        if let Some(ref rev) = current_revision {
            if let Ok(val) = rev.parse() {
                response_headers.insert("X-Revision", val);
            }
            if let Ok(val) = format!("\"{}\"", rev).parse() {
                response_headers.insert(header::ETAG, val);
            }
        }

        Ok((StatusCode::OK, response_headers, tarball).into_response())
    }
}

/// Build the HTTP response for a revision-pinned tarball.
///
/// Sets `Content-Type: application/gzip`, `X-Revision`, and `ETag` headers.
/// Header values that cannot be represented as valid HTTP header values
/// (e.g. non-visible-ASCII characters) are silently omitted rather than
/// causing a panic.
fn build_tarball_response(tarball: Vec<u8>, revision: &str) -> impl IntoResponse {
    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CONTENT_TYPE, "application/gzip".parse().unwrap());
    if let Ok(val) = revision.parse() {
        response_headers.insert("X-Revision", val);
    }
    if let Ok(val) = format!("\"{}\"", revision).parse() {
        response_headers.insert(header::ETAG, val);
    }
    (StatusCode::OK, response_headers, tarball)
}

/// Build a gzipped tar archive of a workspace directory with library overlays
fn build_tarball(
    workspace_path: &std::path::Path,
    library_paths: &std::collections::HashMap<String, std::path::PathBuf>,
) -> anyhow::Result<Vec<u8>> {
    let buf = Vec::new();
    let encoder = GzEncoder::new(buf, Compression::default());
    let mut archive = tar::Builder::new(encoder);

    // Do not follow symlinks — prevents a malicious library from exfiltrating
    // server-side files via symlinks pointing outside the library directory.
    archive.follow_symlinks(false);

    // Walk all files in the workspace directory
    archive.append_dir_all(".", workspace_path)?;

    // Overlay library source directories under _libraries/{lib_name}/
    for (lib_name, lib_path) in library_paths {
        if lib_path.exists() {
            archive.append_dir_all(format!("_libraries/{}", lib_name), lib_path)?;
        }
    }

    let encoder = archive.into_inner()?;
    let compressed = encoder.finish()?;

    Ok(compressed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use walkdir::WalkDir;

    // ── WorkspaceQuery deserialization ───────────────────────────────────────
    //
    // axum's Query extractor uses serde's Deserialize impl. We test the struct
    // directly via serde_json (same trait, no extra crate dependency needed).

    #[test]
    fn test_workspace_query_revision_present() {
        let q: WorkspaceQuery = serde_json::from_value(serde_json::json!({"revision": "abc123"}))
            .expect("should deserialize");
        assert_eq!(q.revision.as_deref(), Some("abc123"));
    }

    #[test]
    fn test_workspace_query_revision_absent_is_none() {
        // serde(default) must yield None when the field is missing.
        let q: WorkspaceQuery =
            serde_json::from_value(serde_json::json!({})).expect("should deserialize empty object");
        assert!(q.revision.is_none());
    }

    #[test]
    fn test_workspace_query_revision_null_is_none() {
        // Explicit null also yields None.
        let q: WorkspaceQuery = serde_json::from_value(serde_json::json!({"revision": null}))
            .expect("should deserialize null revision");
        assert!(q.revision.is_none());
    }

    #[test]
    fn test_workspace_query_revision_sha_roundtrips() {
        let sha = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0";
        let q: WorkspaceQuery = serde_json::from_value(serde_json::json!({"revision": sha}))
            .expect("should deserialize sha1");
        assert_eq!(q.revision.as_deref(), Some(sha));
    }

    /// Extract tarball bytes into a temporary directory and return file map
    fn extract_tarball(
        tarball_bytes: &[u8],
    ) -> anyhow::Result<std::collections::HashMap<String, String>> {
        let decoder = GzDecoder::new(tarball_bytes);
        let mut archive = tar::Archive::new(decoder);

        let temp_dir = TempDir::new()?;
        archive.unpack(temp_dir.path())?;

        let mut files = std::collections::HashMap::new();
        for entry in fs::read_dir(temp_dir.path())? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                let content = fs::read_to_string(&path)?;
                files.insert(name, content);
            } else if path.is_dir() {
                // Recursively collect files from subdirectories
                for sub_entry in WalkDir::new(&path) {
                    let sub_entry = sub_entry?;
                    if sub_entry.path().is_file() {
                        let relative = sub_entry.path().strip_prefix(temp_dir.path())?;
                        let name = relative.to_string_lossy().to_string();
                        let content = fs::read_to_string(sub_entry.path())?;
                        files.insert(name, content);
                    }
                }
            }
        }

        Ok(files)
    }

    #[test]
    fn test_build_tarball_contains_files() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();

        // Create some test files
        fs::write(workspace_path.join("file1.txt"), "content1").unwrap();
        fs::write(workspace_path.join("file2.txt"), "content2").unwrap();

        // Build tarball
        let tarball = build_tarball(workspace_path, &HashMap::new()).unwrap();
        assert!(!tarball.is_empty(), "Tarball should not be empty");

        // Extract and verify
        let files = extract_tarball(&tarball).unwrap();
        assert!(files.contains_key("file1.txt"), "Should contain file1.txt");
        assert!(files.contains_key("file2.txt"), "Should contain file2.txt");
        assert_eq!(files.get("file1.txt").unwrap(), "content1");
        assert_eq!(files.get("file2.txt").unwrap(), "content2");
    }

    #[test]
    fn test_build_tarball_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();

        // Build tarball of empty directory
        let tarball = build_tarball(workspace_path, &HashMap::new()).unwrap();

        // Should produce a valid tarball, even if small
        assert!(
            !tarball.is_empty(),
            "Even empty directory tarball should have gz header"
        );

        // Verify it's a valid gzip file
        let decoder = GzDecoder::new(&tarball[..]);
        let mut archive = tar::Archive::new(decoder);
        let entries: Vec<_> = archive.entries().unwrap().collect();

        // Empty directory may have zero entries or just the directory entry
        assert!(
            entries.is_empty() || entries.len() == 1,
            "Empty workspace should have 0-1 entries"
        );
    }

    #[test]
    fn test_build_tarball_preserves_content() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();

        // Create a file with specific content including newlines and special chars
        let test_content = "Line 1\nLine 2\nSpecial chars: !@#$%^&*()";
        fs::write(workspace_path.join("test.txt"), test_content).unwrap();

        // Build tarball
        let tarball = build_tarball(workspace_path, &HashMap::new()).unwrap();

        // Extract and verify content is preserved exactly
        let files = extract_tarball(&tarball).unwrap();
        assert_eq!(
            files.get("test.txt").unwrap(),
            test_content,
            "Content should be preserved exactly"
        );
    }

    #[test]
    fn test_build_tarball_includes_subdirectories() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();

        // Create nested directory structure
        let subdir = workspace_path.join("subdir");
        let nested = subdir.join("nested");
        fs::create_dir_all(&nested).unwrap();

        fs::write(workspace_path.join("root.txt"), "root content").unwrap();
        fs::write(subdir.join("sub.txt"), "sub content").unwrap();
        fs::write(nested.join("nested.txt"), "nested content").unwrap();

        // Build tarball
        let tarball = build_tarball(workspace_path, &HashMap::new()).unwrap();

        // Extract and verify all files are present
        let files = extract_tarball(&tarball).unwrap();
        assert!(files.contains_key("root.txt"), "Should contain root.txt");
        assert!(
            files.contains_key("subdir/sub.txt"),
            "Should contain subdir/sub.txt"
        );
        assert!(
            files.contains_key("subdir/nested/nested.txt"),
            "Should contain subdir/nested/nested.txt"
        );

        assert_eq!(files.get("root.txt").unwrap(), "root content");
        assert_eq!(files.get("subdir/sub.txt").unwrap(), "sub content");
        assert_eq!(
            files.get("subdir/nested/nested.txt").unwrap(),
            "nested content"
        );
    }

    #[test]
    fn test_build_tarball_nonexistent_path_fails() {
        let nonexistent_path = std::path::Path::new("/this/path/does/not/exist/12345");

        let result = build_tarball(nonexistent_path, &HashMap::new());
        assert!(result.is_err(), "Should fail for nonexistent path");
    }

    #[test]
    fn test_build_tarball_with_library_overlay() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();

        // Create workspace file
        fs::write(workspace_path.join("task.yaml"), "name: deploy").unwrap();

        // Create library directory
        let lib_dir = TempDir::new().unwrap();
        let scripts_dir = lib_dir.path().join("scripts");
        fs::create_dir_all(&scripts_dir).unwrap();
        fs::write(scripts_dir.join("notify.sh"), "#!/bin/bash\necho notify").unwrap();

        let mut library_paths = HashMap::new();
        library_paths.insert("common".to_string(), lib_dir.path().to_path_buf());

        let tarball = build_tarball(workspace_path, &library_paths).unwrap();
        let files = extract_tarball(&tarball).unwrap();

        assert!(
            files.contains_key("task.yaml"),
            "Should contain workspace files"
        );
        assert!(
            files.contains_key("_libraries/common/scripts/notify.sh"),
            "Should contain library scripts under _libraries/common/"
        );
        assert_eq!(
            files.get("_libraries/common/scripts/notify.sh").unwrap(),
            "#!/bin/bash\necho notify"
        );
    }

    #[test]
    fn test_build_tarball_with_multiple_libraries() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();
        fs::write(workspace_path.join("root.txt"), "root").unwrap();

        // Library 1
        let lib1_dir = TempDir::new().unwrap();
        fs::write(lib1_dir.path().join("lib1.sh"), "echo lib1").unwrap();

        // Library 2
        let lib2_dir = TempDir::new().unwrap();
        fs::write(lib2_dir.path().join("lib2.sh"), "echo lib2").unwrap();

        let mut library_paths = HashMap::new();
        library_paths.insert("lib1".to_string(), lib1_dir.path().to_path_buf());
        library_paths.insert("lib2".to_string(), lib2_dir.path().to_path_buf());

        let tarball = build_tarball(workspace_path, &library_paths).unwrap();
        let files = extract_tarball(&tarball).unwrap();

        assert!(files.contains_key("root.txt"));
        assert!(files.contains_key("_libraries/lib1/lib1.sh"));
        assert!(files.contains_key("_libraries/lib2/lib2.sh"));
    }

    #[test]
    fn test_build_tarball_skips_nonexistent_library() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_path = temp_dir.path();
        fs::write(workspace_path.join("root.txt"), "root").unwrap();

        let mut library_paths = HashMap::new();
        library_paths.insert("missing".to_string(), PathBuf::from("/this/does/not/exist"));

        // Should succeed — nonexistent library paths are skipped
        let tarball = build_tarball(workspace_path, &library_paths).unwrap();
        let files = extract_tarball(&tarball).unwrap();
        assert!(files.contains_key("root.txt"));
    }
}
