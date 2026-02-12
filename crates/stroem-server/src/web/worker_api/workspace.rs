use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;
use std::sync::Arc;

/// GET /worker/workspace/:ws.tar.gz - Download workspace as tarball
#[tracing::instrument(skip(state))]
pub async fn download_workspace(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(ws_filename): Path<String>,
) -> impl IntoResponse {
    // Strip .tar.gz suffix to get workspace name
    let ws_name = ws_filename.strip_suffix(".tar.gz").unwrap_or(&ws_filename);

    let workspace_path = match state.workspaces.get_path(ws_name) {
        Some(p) => p.to_path_buf(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": format!("Workspace '{}' not found", ws_name)})),
            )
                .into_response();
        }
    };

    let revision = state.workspaces.get_revision(ws_name);

    // Check If-None-Match for caching
    if let Some(etag) = headers.get(header::IF_NONE_MATCH) {
        if let Ok(etag_str) = etag.to_str() {
            if let Some(ref rev) = revision {
                if etag_str.trim_matches('"') == rev.as_str() {
                    return StatusCode::NOT_MODIFIED.into_response();
                }
            }
        }
    }

    // Build tarball in a blocking task
    let tarball = match tokio::task::spawn_blocking(move || build_tarball(&workspace_path)).await {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => {
            tracing::error!("Failed to build workspace tarball: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "Failed to build tarball"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("Tarball task panicked: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": "Internal error"})),
            )
                .into_response();
        }
    };

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CONTENT_TYPE, "application/gzip".parse().unwrap());
    if let Some(ref rev) = revision {
        response_headers.insert("X-Revision", rev.parse().unwrap());
        response_headers.insert(header::ETAG, format!("\"{}\"", rev).parse().unwrap());
    }

    (StatusCode::OK, response_headers, tarball).into_response()
}

/// Build a gzipped tar archive of a workspace directory
fn build_tarball(workspace_path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    let buf = Vec::new();
    let encoder = GzEncoder::new(buf, Compression::default());
    let mut archive = tar::Builder::new(encoder);

    // Walk all files in the workspace directory
    archive.append_dir_all(".", workspace_path)?;

    let encoder = archive.into_inner()?;
    let compressed = encoder.finish()?;

    Ok(compressed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::fs;
    use tempfile::TempDir;
    use walkdir::WalkDir;

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
        let tarball = build_tarball(workspace_path).unwrap();
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
        let tarball = build_tarball(workspace_path).unwrap();

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
        let tarball = build_tarball(workspace_path).unwrap();

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
        let tarball = build_tarball(workspace_path).unwrap();

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

        let result = build_tarball(nonexistent_path);
        assert!(result.is_err(), "Should fail for nonexistent path");
    }
}
