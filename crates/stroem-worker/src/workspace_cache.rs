use anyhow::{Context, Result};
use dashmap::DashMap;
use flate2::read::GzDecoder;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tar::Archive;

use crate::client::ServerClient;

/// Caches workspace tarballs locally, extracting them on demand.
/// Each workspace gets its own directory under `base_dir/{name}/`.
/// A `.revision` file tracks the current revision to enable ETag-based caching.
///
/// Thread-safe: concurrent extractions for the same workspace are serialized via
/// per-workspace locks; different workspaces can be extracted concurrently.
pub struct WorkspaceCache {
    base_dir: PathBuf,
    per_workspace_locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
}

impl WorkspaceCache {
    pub fn new(base_dir: &str) -> Self {
        Self {
            base_dir: PathBuf::from(base_dir),
            per_workspace_locks: Arc::new(DashMap::new()),
        }
    }

    /// Get the directory where a workspace is extracted
    pub fn workspace_dir(&self, name: &str) -> PathBuf {
        self.base_dir.join(name)
    }

    /// Read the currently cached revision for a workspace, if any
    pub fn current_revision(&self, name: &str) -> Option<String> {
        let rev_file = self.base_dir.join(name).join(".revision");
        std::fs::read_to_string(rev_file).ok()
    }

    fn workspace_lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.per_workspace_locks
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Extract a tarball into the workspace directory and write the revision file.
    ///
    /// Uses a temp-dir + atomic rename pattern so that concurrent readers always
    /// see a consistent workspace state.  Concurrent calls for the *same* workspace
    /// are serialized via a per-workspace `Mutex`.
    pub fn extract_tarball(&self, name: &str, data: &[u8], revision: &str) -> Result<()> {
        let lock = self.workspace_lock(name);
        let _guard = lock
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        extract_tarball_inner(&self.base_dir, name, data, revision)
    }

    /// Ensure the workspace is up-to-date by downloading if necessary.
    /// Returns the path to the extracted workspace directory.
    pub async fn ensure_up_to_date(
        &self,
        client: &ServerClient,
        workspace: &str,
    ) -> Result<PathBuf> {
        let cached_rev = self.current_revision(workspace);
        let ws_dir = self.workspace_dir(workspace);

        // If we have a cached revision and the directory exists, try conditional fetch
        let result = client
            .download_workspace_tarball(workspace, cached_rev.as_deref())
            .await
            .context("Failed to check workspace update")?;

        match result {
            Some((data, revision)) => {
                // New version available — extract it in a blocking thread to avoid
                // stalling the async runtime during fs ops and tar decompression.
                // Acquire the per-workspace lock here and pass it into the blocking
                // task so concurrent async callers are also serialized.
                let lock = self.workspace_lock(workspace);
                let base_dir = self.base_dir.clone();
                let workspace_owned = workspace.to_owned();
                tokio::task::spawn_blocking(move || {
                    let _guard = lock
                        .lock()
                        .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
                    extract_tarball_inner(&base_dir, &workspace_owned, &data, &revision)
                })
                .await
                .context("tarball extraction task panicked")??;
            }
            None => {
                // 304 Not Modified — our cache is current
                tracing::debug!("Workspace '{}' is up-to-date", workspace);
                // Ensure directory exists (shouldn't happen, but be safe)
                if !ws_dir.exists() {
                    anyhow::bail!(
                        "Workspace '{}' cache reports up-to-date but directory is missing",
                        workspace
                    );
                }
            }
        }

        Ok(ws_dir)
    }
}

/// Inner extraction logic — caller must hold the per-workspace lock.
///
/// Extracts `data` (a gzip-compressed tar archive) into `base_dir/name/`, writing
/// a `.revision` file with `revision`.  Uses a temp directory + atomic rename so
/// concurrent readers always see a consistent workspace state.
fn extract_tarball_inner(base_dir: &Path, name: &str, data: &[u8], revision: &str) -> Result<()> {
    let ws_dir = base_dir.join(name);
    let tmp_dir = base_dir.join(format!("{}.tmp", name));

    // Extract to a temporary directory first
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir).context("Failed to remove temp directory")?;
    }
    std::fs::create_dir_all(&tmp_dir).context("Failed to create temp directory")?;

    let decoder = GzDecoder::new(data);
    let mut archive = Archive::new(decoder);
    archive
        .unpack(&tmp_dir)
        .context("Failed to extract workspace tarball")?;

    // Write revision file into the temp dir
    let rev_file = tmp_dir.join(".revision");
    std::fs::write(&rev_file, revision).context("Failed to write revision file")?;

    // Atomic swap: remove old workspace dir, rename temp into place.
    // Fall back to recursive copy+remove when the rename crosses filesystem boundaries
    // (EXDEV / error code 18 on Unix).
    if ws_dir.exists() {
        std::fs::remove_dir_all(&ws_dir).context("Failed to remove old workspace directory")?;
    }

    rename_or_copy(&tmp_dir, &ws_dir)?;

    tracing::info!(
        "Extracted workspace '{}' (revision: {}) to {}",
        name,
        revision,
        ws_dir.display()
    );

    Ok(())
}

/// Rename `src` to `dst`, falling back to a recursive copy + remove when the
/// two paths reside on different filesystems (EXDEV, OS error 18 on Unix).
fn rename_or_copy(src: &Path, dst: &Path) -> Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => {}
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(18) => {
            // Cross-filesystem move: copy recursively then remove the source tree.
            copy_dir_all(src, dst)?;
            std::fs::remove_dir_all(src).context("Failed to remove temp directory after copy")?;
        }
        Err(e) => return Err(e).context("Failed to rename temp to workspace directory"),
    }
    Ok(())
}

/// Recursively copy the directory tree rooted at `src` into `dst`.
///
/// `dst` must not exist yet; it will be created by this function.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("Failed to create directory {}", dst.display()))?;

    for entry in std::fs::read_dir(src)
        .with_context(|| format!("Failed to read directory {}", src.display()))?
    {
        let entry = entry.with_context(|| format!("Failed to read entry in {}", src.display()))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("Failed to get file type for {}", src_path.display()))?;

        if file_type.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "Failed to copy {} to {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    fn build_test_tarball() -> Vec<u8> {
        let buf = Vec::new();
        let encoder = GzEncoder::new(buf, Compression::default());
        let mut archive = tar::Builder::new(encoder);

        // Add a test file
        let data = b"hello world";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "test.txt", &data[..])
            .unwrap();

        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    fn build_tarball_with_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        let buf = Vec::new();
        let encoder = GzEncoder::new(buf, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, &data[..]).unwrap();
        }
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn test_workspace_cache_dir() {
        let cache = WorkspaceCache::new("/tmp/stroem-cache");
        assert_eq!(
            cache.workspace_dir("default"),
            PathBuf::from("/tmp/stroem-cache/default")
        );
    }

    #[test]
    fn test_extract_and_revision() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        let tarball = build_test_tarball();
        cache
            .extract_tarball("test-ws", &tarball, "rev123")
            .unwrap();

        // Check revision
        assert_eq!(
            cache.current_revision("test-ws"),
            Some("rev123".to_string())
        );

        // Check extracted file
        let ws_dir = cache.workspace_dir("test-ws");
        let content = std::fs::read_to_string(ws_dir.join("test.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_extract_replaces_old_content() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // First extract
        let tarball = build_test_tarball();
        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();
        assert_eq!(cache.current_revision("test-ws"), Some("rev1".to_string()));

        // Second extract replaces
        cache.extract_tarball("test-ws", &tarball, "rev2").unwrap();
        assert_eq!(cache.current_revision("test-ws"), Some("rev2".to_string()));
    }

    #[test]
    fn test_no_revision_for_unknown_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());
        assert_eq!(cache.current_revision("nonexistent"), None);
    }

    #[test]
    fn test_corrupted_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // Invalid (non-gzip) data
        let invalid_data = b"this is not a gzip tarball";
        let result = cache.extract_tarball("test-ws", invalid_data, "rev1");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to extract workspace tarball"));
    }

    #[test]
    fn test_empty_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // Empty byte slice
        let empty_data: &[u8] = &[];
        let result = cache.extract_tarball("test-ws", empty_data, "rev1");

        assert!(result.is_err());
    }

    #[test]
    fn test_extract_creates_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // Build tarball with nested directory
        let tarball = build_tarball_with_files(&[("subdir/file.txt", b"nested content")]);

        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        let ws_dir = cache.workspace_dir("test-ws");
        let nested_file = ws_dir.join("subdir").join("file.txt");

        assert!(nested_file.exists());
        let content = std::fs::read_to_string(nested_file).unwrap();
        assert_eq!(content, "nested content");
    }

    #[test]
    fn test_workspace_dir_with_special_names() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // Test workspace names with hyphens, underscores, dots
        let tarball = build_test_tarball();

        for name in &["my-workspace", "my_workspace", "my.workspace"] {
            cache.extract_tarball(name, &tarball, "rev1").unwrap();
            let ws_dir = cache.workspace_dir(name);
            assert!(ws_dir.exists());
            assert_eq!(cache.current_revision(name), Some("rev1".to_string()));
        }
    }

    #[test]
    fn test_extract_preserves_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // Build tarball with multiple files with different content
        let tarball = build_tarball_with_files(&[
            ("file1.txt", b"content one"),
            ("file2.txt", b"content two"),
            ("file3.txt", b"content three"),
        ]);

        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        let ws_dir = cache.workspace_dir("test-ws");

        // Verify each file's content is correct
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("file1.txt")).unwrap(),
            "content one"
        );
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("file2.txt")).unwrap(),
            "content two"
        );
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("file3.txt")).unwrap(),
            "content three"
        );
    }

    #[test]
    fn test_revision_survives_reextract() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // Extract first tarball with rev1
        let tarball1 = build_tarball_with_files(&[("old.txt", b"old content")]);
        cache.extract_tarball("test-ws", &tarball1, "rev1").unwrap();
        assert_eq!(cache.current_revision("test-ws"), Some("rev1".to_string()));

        // Extract different tarball with rev2
        let tarball2 = build_tarball_with_files(&[("new.txt", b"new content")]);
        cache.extract_tarball("test-ws", &tarball2, "rev2").unwrap();

        // Verify revision is rev2 and new content exists
        assert_eq!(cache.current_revision("test-ws"), Some("rev2".to_string()));
        let ws_dir = cache.workspace_dir("test-ws");
        assert!(ws_dir.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("new.txt")).unwrap(),
            "new content"
        );
    }

    #[test]
    fn test_extract_removes_stale_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // Extract tarball with file A
        let tarball1 = build_tarball_with_files(&[("fileA.txt", b"content A")]);
        cache.extract_tarball("test-ws", &tarball1, "rev1").unwrap();

        let ws_dir = cache.workspace_dir("test-ws");
        assert!(ws_dir.join("fileA.txt").exists());

        // Extract tarball with file B (no A)
        let tarball2 = build_tarball_with_files(&[("fileB.txt", b"content B")]);
        cache.extract_tarball("test-ws", &tarball2, "rev2").unwrap();

        // Verify A is gone and B exists
        assert!(!ws_dir.join("fileA.txt").exists());
        assert!(ws_dir.join("fileB.txt").exists());
    }

    #[test]
    fn test_current_revision_with_corrupted_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap());

        // First extract a valid workspace
        let tarball = build_test_tarball();
        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        // Write invalid UTF-8 to .revision file
        let ws_dir = cache.workspace_dir("test-ws");
        let rev_file = ws_dir.join(".revision");
        std::fs::write(&rev_file, [0xFF, 0xFE, 0xFD]).unwrap();

        // current_revision should return None (read_to_string fails on invalid UTF-8)
        assert_eq!(cache.current_revision("test-ws"), None);
    }

    #[test]
    fn test_concurrent_extraction_same_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(WorkspaceCache::new(dir.path().to_str().unwrap()));
        let tarball1 = build_tarball_with_files(&[("v1.txt", b"version 1")]);
        let tarball2 = build_tarball_with_files(&[("v2.txt", b"version 2")]);

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let cache = Arc::clone(&cache);
                let data = if i % 2 == 0 {
                    tarball1.clone()
                } else {
                    tarball2.clone()
                };
                let rev = format!("rev{}", i);
                std::thread::spawn(move || {
                    cache.extract_tarball("shared-ws", &data, &rev).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // After all extractions, workspace should be valid (not corrupted)
        let ws_dir = cache.workspace_dir("shared-ws");
        assert!(ws_dir.exists());
        let rev = cache.current_revision("shared-ws");
        assert!(rev.is_some());
    }

    #[test]
    fn test_concurrent_extraction_different_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(WorkspaceCache::new(dir.path().to_str().unwrap()));
        let tarball = build_test_tarball();

        // Different workspaces can extract concurrently without blocking each other
        let handles: Vec<_> = (0..5)
            .map(|i| {
                let cache = Arc::clone(&cache);
                let data = tarball.clone();
                let ws_name = format!("workspace-{}", i);
                std::thread::spawn(move || {
                    cache.extract_tarball(&ws_name, &data, "rev1").unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // All workspaces should exist with correct revisions
        for i in 0..5 {
            let ws_name = format!("workspace-{}", i);
            assert!(cache.workspace_dir(&ws_name).exists());
            assert_eq!(cache.current_revision(&ws_name), Some("rev1".to_string()));
        }
    }

    #[test]
    fn test_copy_dir_all() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");

        // Build a small source tree
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("root.txt"), b"root").unwrap();
        std::fs::write(src.join("sub").join("nested.txt"), b"nested").unwrap();

        copy_dir_all(&src, &dst).unwrap();

        assert_eq!(std::fs::read(dst.join("root.txt")).unwrap(), b"root");
        assert_eq!(
            std::fs::read(dst.join("sub").join("nested.txt")).unwrap(),
            b"nested"
        );
    }

    #[test]
    fn test_rename_or_copy_same_fs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("file.txt"), b"data").unwrap();

        rename_or_copy(&src, &dst).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read(dst.join("file.txt")).unwrap(), b"data");
    }
}
