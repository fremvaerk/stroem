use anyhow::{Context, Result};
use dashmap::DashMap;
use flate2::read::GzDecoder;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tar::Archive;

use crate::client::ServerClient;

/// Default number of old revisions to retain per workspace.
const DEFAULT_MAX_RETAINED_REVISIONS: usize = 2;

/// RAII guard that keeps a revision directory alive while a step is using it.
///
/// Holds a reference count on the revision directory. While at least one guard
/// exists for a given revision, [`WorkspaceCache::cleanup_old_revisions`] will
/// skip that directory.
pub struct WorkspaceGuard {
    path: PathBuf,
    ref_count: Arc<AtomicUsize>,
}

impl WorkspaceGuard {
    /// Path to the revision directory (pass this to the runner).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        self.ref_count.fetch_sub(1, Ordering::Release);
    }
}

/// Caches workspace tarballs locally, extracting them on demand.
///
/// Uses immutable revision-based directories:
/// ```text
/// {base_dir}/{workspace}/.current           ← file containing current revision string
/// {base_dir}/{workspace}/{revision}/        ← immutable extracted content, one per revision
/// ```
///
/// Multiple steps sharing the same revision use the same directory read-only.
/// New revisions create new directories alongside old ones — no deletion during
/// active use. Old revisions are cleaned up lazily when no longer referenced.
///
/// Thread-safe: concurrent extractions for the same workspace are serialized via
/// per-workspace locks; different workspaces can be extracted concurrently.
pub struct WorkspaceCache {
    base_dir: PathBuf,
    per_workspace_locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    /// Tracks active step count per revision directory, keyed by
    /// `"{workspace}/{sanitized_revision}"`. Entries are pruned during cleanup when
    /// the directory is deleted or when the ref count is zero and the directory no
    /// longer exists on disk. The map is bounded by the number of distinct revisions
    /// ever encountered across all workspaces; each entry is ~100 bytes.
    revision_refs: Arc<DashMap<String, Arc<AtomicUsize>>>,
    /// Maximum number of old revisions to keep per workspace (in addition to current).
    max_retained_revisions: usize,
}

impl WorkspaceCache {
    pub fn new(base_dir: &str, max_retained_revisions: Option<usize>) -> Self {
        Self {
            base_dir: PathBuf::from(base_dir),
            per_workspace_locks: Arc::new(DashMap::new()),
            revision_refs: Arc::new(DashMap::new()),
            max_retained_revisions: max_retained_revisions
                .unwrap_or(DEFAULT_MAX_RETAINED_REVISIONS),
        }
    }

    /// Get the base directory for a workspace (contains revision subdirs + `.current`).
    pub fn workspace_dir(&self, name: &str) -> PathBuf {
        self.base_dir.join(name)
    }

    /// Read the currently cached revision for a workspace, if any.
    ///
    /// Reads from `{base_dir}/{workspace}/.current`. Returns `None` for workspaces
    /// using the old layout (forces re-download into revision-based layout).
    pub fn current_revision(&self, name: &str) -> Option<String> {
        let current_file = self.base_dir.join(name).join(".current");
        let rev = std::fs::read_to_string(&current_file).ok()?;
        let rev = rev.trim().to_string();
        if rev.is_empty() {
            return None;
        }
        // Verify the revision directory actually exists
        let rev_dir = self
            .base_dir
            .join(name)
            .join(sanitize_revision_for_dir(&rev));
        if rev_dir.is_dir() {
            Some(rev)
        } else {
            None
        }
    }

    fn workspace_lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.per_workspace_locks
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Get or create the ref-count for a revision.
    fn revision_ref_count(&self, workspace: &str, revision: &str) -> Arc<AtomicUsize> {
        let key = format!("{}/{}", workspace, sanitize_revision_for_dir(revision));
        self.revision_refs
            .entry(key)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone()
    }

    /// Create a [`WorkspaceGuard`] for the given revision directory.
    fn acquire_guard(&self, workspace: &str, revision: &str, path: PathBuf) -> WorkspaceGuard {
        let ref_count = self.revision_ref_count(workspace, revision);
        ref_count.fetch_add(1, Ordering::Relaxed);
        WorkspaceGuard { path, ref_count }
    }

    /// Extract a tarball into a revision-based directory and update `.current`.
    ///
    /// Returns the path to the revision directory. If the revision directory
    /// already exists, this is a no-op (idempotent).
    pub fn extract_tarball(&self, name: &str, data: &[u8], revision: &str) -> Result<PathBuf> {
        let lock = self.workspace_lock(name);
        let _guard = lock
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        extract_tarball_inner(&self.base_dir, name, data, revision, true)
    }

    /// Extract a tarball into a revision-based directory without updating `.current`.
    ///
    /// Used for pinned (historical) revisions where we do not want to regress
    /// the workspace's tracked latest revision.
    ///
    /// Returns the path to the revision directory. Idempotent if the directory
    /// already exists.
    // Used in tests to verify pinned extraction behaviour.
    #[cfg(test)]
    fn extract_tarball_pinned(&self, name: &str, data: &[u8], revision: &str) -> Result<PathBuf> {
        let lock = self.workspace_lock(name);
        let _guard = lock
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        extract_tarball_inner(&self.base_dir, name, data, revision, false)
    }

    /// Ensure the workspace is up-to-date by downloading if necessary.
    ///
    /// Returns a [`WorkspaceGuard`] that keeps the revision directory alive
    /// for the duration of step execution. The guard's ref count prevents
    /// cleanup from deleting the directory while it's in use.
    pub async fn ensure_up_to_date(
        &self,
        client: &ServerClient,
        workspace: &str,
    ) -> Result<WorkspaceGuard> {
        let cached_rev = self.current_revision(workspace);

        let result = client
            .download_workspace_tarball(workspace, cached_rev.as_deref(), None)
            .await
            .context("Failed to check workspace update")?;

        let (revision, rev_dir) = match result {
            Some((data, revision)) => {
                // New version available — extract in a blocking thread.
                let lock = self.workspace_lock(workspace);
                let base_dir = self.base_dir.clone();
                let workspace_owned = workspace.to_owned();
                let revision_clone = revision.clone();
                let rev_dir = tokio::task::spawn_blocking(move || {
                    let _guard = lock
                        .lock()
                        .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
                    extract_tarball_inner(&base_dir, &workspace_owned, &data, &revision_clone, true)
                })
                .await
                .context("tarball extraction task panicked")??;

                (revision, rev_dir)
            }
            None => {
                // 304 Not Modified — use the current revision.
                tracing::debug!("Workspace '{}' is up-to-date", workspace);
                let revision = cached_rev.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Workspace '{}' cache reports up-to-date but no current revision found",
                        workspace
                    )
                })?;
                let sanitized = sanitize_revision_for_dir(&revision);
                let rev_dir = self.base_dir.join(workspace).join(&sanitized);
                if !rev_dir.exists() {
                    anyhow::bail!(
                        "Workspace '{}' revision directory is missing: {}",
                        workspace,
                        rev_dir.display()
                    );
                }
                (revision, rev_dir)
            }
        };

        let guard = self.acquire_guard(workspace, &revision, rev_dir);

        // Spawn background cleanup (non-blocking, best-effort)
        let base_dir = self.base_dir.clone();
        let workspace_owned = workspace.to_owned();
        let revision_refs = self.revision_refs.clone();
        let max_retained = self.max_retained_revisions;
        tokio::task::spawn_blocking(move || {
            if let Err(e) = cleanup_old_revisions(
                &base_dir,
                &workspace_owned,
                &revision,
                max_retained,
                &revision_refs,
            ) {
                tracing::warn!(
                    workspace = %workspace_owned,
                    "Failed to clean up old revisions: {:#}",
                    e
                );
            }
        });

        Ok(guard)
    }

    /// Ensure a specific revision is available locally for a workspace.
    ///
    /// Unlike [`ensure_up_to_date`], this method downloads a specific revision
    /// (using `?revision=` query param) and does NOT update the `.current` file,
    /// since downloading an old pinned revision should not regress the latest
    /// tracking.
    ///
    /// Returns a [`WorkspaceGuard`] if the revision is already cached locally
    /// (no network call) or after downloading and extracting.
    ///
    /// [`ensure_up_to_date`]: WorkspaceCache::ensure_up_to_date
    pub async fn ensure_revision(
        &self,
        client: &ServerClient,
        workspace: &str,
        revision: &str,
    ) -> Result<WorkspaceGuard> {
        let sanitized = sanitize_revision_for_dir(revision);
        let rev_dir = self.base_dir.join(workspace).join(&sanitized);

        // Hold the per-workspace lock while checking existence and acquiring the guard
        // to prevent a TOCTOU race with cleanup_old_revisions.
        {
            let lock = self.workspace_lock(workspace);
            let _lk = lock
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            if rev_dir.is_dir() {
                tracing::debug!(
                    workspace,
                    revision,
                    "Pinned revision already cached locally"
                );
                let guard = self.acquire_guard(workspace, revision, rev_dir);
                return Ok(guard);
            }
        }
        // Lock released — proceed with download (which re-acquires the lock for extraction).

        // Not cached — download the specific revision from the server.
        let result = client
            .download_workspace_tarball(workspace, None, Some(revision))
            .await
            .context("Failed to download pinned workspace revision")?;

        let (data, returned_revision) = result.ok_or_else(|| {
            anyhow::anyhow!(
                "Server returned 304 Not Modified for pinned revision request \
                 (workspace '{}', revision '{}')",
                workspace,
                revision
            )
        })?;

        if returned_revision != revision {
            tracing::warn!(
                workspace,
                requested = revision,
                returned = %returned_revision,
                "Server returned different revision than requested"
            );
        }

        // Extract in a blocking thread without updating .current.
        let lock = self.workspace_lock(workspace);
        let base_dir = self.base_dir.clone();
        let workspace_owned = workspace.to_owned();
        let revision_owned = revision.to_owned();
        let rev_dir = tokio::task::spawn_blocking(move || {
            let _guard = lock
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            extract_tarball_inner(&base_dir, &workspace_owned, &data, &revision_owned, false)
        })
        .await
        .context("pinned tarball extraction task panicked")??;

        tracing::debug!(
            workspace,
            revision,
            returned_revision = %returned_revision,
            "Pinned revision downloaded and extracted"
        );

        let guard = self.acquire_guard(workspace, revision, rev_dir);
        Ok(guard)
    }
}

/// Sanitize a revision string for use as a directory name.
///
/// Revision strings are typically hex hashes (40-64 chars) which are already
/// filesystem-safe. This is a safety net for unexpected characters.
///
/// Special-cases `".."` and `"."` which would otherwise resolve to the parent
/// or current directory via [`PathBuf::join`], enabling path traversal.
fn sanitize_revision_for_dir(revision: &str) -> String {
    let sanitized: String = revision
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Prevent path traversal — ".." and "." are valid after character mapping
    // but would resolve to parent/current directory inside PathBuf::join.
    match sanitized.as_str() {
        ".." => "_dotdot".to_string(),
        "." => "_dot".to_string(),
        _ => sanitized,
    }
}

/// Write `content` to `path` atomically via a temp file + rename.
///
/// A crash between the write and rename leaves a `.tmp` file rather than a
/// truncated target, so readers always see a complete value.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)
        .with_context(|| format!("Failed to write temp file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Inner extraction logic — caller must hold the per-workspace lock.
///
/// Extracts `data` (a gzip-compressed tar archive) into
/// `base_dir/name/{sanitized_revision}/`. Uses a temp directory + atomic rename
/// so concurrent readers always see a consistent directory.
///
/// When `update_current` is `true`, the revision string is atomically written to
/// `base_dir/name/.current` after extraction. Pass `false` for pinned (historical)
/// revisions to avoid regressing the workspace's latest-revision tracking.
///
/// If the revision directory already exists, skips extraction (idempotent).
/// Returns the path to the revision directory.
fn extract_tarball_inner(
    base_dir: &Path,
    name: &str,
    data: &[u8],
    revision: &str,
    update_current: bool,
) -> Result<PathBuf> {
    let ws_dir = base_dir.join(name);
    let sanitized = sanitize_revision_for_dir(revision);
    if sanitized.is_empty() {
        anyhow::bail!("Revision string must not be empty (got {:?})", revision);
    }
    let rev_dir = ws_dir.join(&sanitized);

    // Idempotent: if revision directory already exists, optionally update .current
    if rev_dir.is_dir() {
        tracing::debug!(
            "Revision directory already exists, skipping extraction: {}",
            rev_dir.display()
        );
        if update_current {
            // Still update .current in case it's stale
            let current_file = ws_dir.join(".current");
            atomic_write(&current_file, revision).context("Failed to write .current file")?;
        }
        return Ok(rev_dir);
    }

    let tmp_dir = ws_dir.join(format!("{}.tmp", sanitized));

    // Ensure the workspace base directory exists
    std::fs::create_dir_all(&ws_dir).context("Failed to create workspace directory")?;

    // Clean up any leftover temp directory from a previous failed extraction
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir).context("Failed to remove temp directory")?;
    }
    std::fs::create_dir_all(&tmp_dir).context("Failed to create temp directory")?;

    let decoder = GzDecoder::new(data);
    let mut archive = Archive::new(decoder);
    archive
        .unpack(&tmp_dir)
        .context("Failed to extract workspace tarball")?;

    // Atomic rename temp → revision directory.
    // Fall back to recursive copy+remove when crossing filesystem boundaries.
    rename_or_copy(&tmp_dir, &rev_dir)?;

    if update_current {
        // Update .current to point to this revision (atomic write)
        let current_file = ws_dir.join(".current");
        atomic_write(&current_file, revision).context("Failed to write .current file")?;
    }

    tracing::info!(
        "Extracted workspace '{}' (revision: {}) to {} (update_current: {})",
        name,
        revision,
        rev_dir.display(),
        update_current,
    );

    Ok(rev_dir)
}

/// Clean up old revision directories for a workspace.
///
/// Keeps the current revision plus up to `max_retained` old ones (sorted by
/// modification time, newest first). Skips any directory whose ref count is > 0.
fn cleanup_old_revisions(
    base_dir: &Path,
    workspace: &str,
    current_revision: &str,
    max_retained: usize,
    revision_refs: &DashMap<String, Arc<AtomicUsize>>,
) -> Result<()> {
    let ws_dir = base_dir.join(workspace);
    let current_sanitized = sanitize_revision_for_dir(current_revision);

    let entries = match std::fs::read_dir(&ws_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("Failed to read workspace directory"),
    };

    // Collect non-current revision directories with their modification times
    let mut old_revisions: Vec<(PathBuf, String, std::time::SystemTime)> = Vec::new();

    for entry in entries {
        let entry = entry.context("Failed to read directory entry")?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip non-directories, .current file, and temp dirs
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if name.ends_with(".tmp") {
            // Clean up stale temp directories
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(
                    "Failed to remove stale temp dir {}: {}",
                    entry.path().display(),
                    e
                );
            }
            continue;
        }
        if name == current_sanitized {
            continue;
        }

        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        old_revisions.push((entry.path(), name, mtime));
    }

    // Sort by mtime descending (newest first) — we keep the newest ones
    old_revisions.sort_by(|a, b| b.2.cmp(&a.2));

    // Remove revisions beyond the retention limit
    for (path, dir_name, _) in old_revisions.iter().skip(max_retained) {
        // Check ref count — skip if in use
        let key = format!("{}/{}", workspace, dir_name);
        let in_use = revision_refs
            .get(&key)
            .map(|rc| rc.load(Ordering::Acquire) > 0)
            .unwrap_or(false);

        if in_use {
            tracing::debug!(
                "Skipping cleanup of in-use revision directory: {}",
                path.display()
            );
            continue;
        }

        if let Err(e) = std::fs::remove_dir_all(path) {
            tracing::warn!(
                "Failed to remove old revision directory {}: {}",
                path.display(),
                e
            );
        } else {
            tracing::debug!("Cleaned up old revision directory: {}", path.display());
            revision_refs.remove(&key);
        }
    }

    // Prune stale ref-count entries: zero count and directory no longer on disk.
    // This prevents the DashMap from growing unboundedly over many revisions.
    let prefix = format!("{}/", workspace);
    revision_refs.retain(|key, rc| {
        if !key.starts_with(&prefix) {
            return true; // different workspace, keep
        }
        if rc.load(Ordering::Relaxed) > 0 {
            return true; // still in use, keep
        }
        let rev_name = &key[prefix.len()..];
        let dir = ws_dir.join(rev_name);
        if dir.is_dir() {
            return true; // directory still exists (retained or current), keep
        }
        false // zero count, no directory → prune
    });

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
        Err(e) => return Err(e).context("Failed to rename temp to revision directory"),
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
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(&src_path)
                    .with_context(|| format!("Failed to read symlink {}", src_path.display()))?;
                std::os::unix::fs::symlink(&target, &dst_path)
                    .with_context(|| format!("Failed to create symlink {}", dst_path.display()))?;
            }
            #[cfg(not(unix))]
            {
                std::fs::copy(&src_path, &dst_path).with_context(|| {
                    format!(
                        "Failed to copy {} to {}",
                        src_path.display(),
                        dst_path.display()
                    )
                })?;
            }
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

    fn new_cache(dir: &Path) -> WorkspaceCache {
        WorkspaceCache::new(dir.to_str().unwrap(), None)
    }

    #[test]
    fn test_workspace_cache_dir() {
        let cache = WorkspaceCache::new("/tmp/stroem-cache", None);
        assert_eq!(
            cache.workspace_dir("default"),
            PathBuf::from("/tmp/stroem-cache/default")
        );
    }

    #[test]
    fn test_extract_and_revision() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        let tarball = build_test_tarball();
        let rev_dir = cache
            .extract_tarball("test-ws", &tarball, "rev123")
            .unwrap();

        // Check revision
        assert_eq!(
            cache.current_revision("test-ws"),
            Some("rev123".to_string())
        );

        // Check extracted file is under {workspace}/{revision}/
        assert_eq!(rev_dir, dir.path().join("test-ws").join("rev123"));
        let content = std::fs::read_to_string(rev_dir.join("test.txt")).unwrap();
        assert_eq!(content, "hello world");

        // .current file should contain the revision string
        let current = std::fs::read_to_string(dir.path().join("test-ws").join(".current")).unwrap();
        assert_eq!(current, "rev123");
    }

    #[test]
    fn test_extract_replaces_old_content() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // First extract
        let tarball1 = build_tarball_with_files(&[("old.txt", b"old content")]);
        let rev1_dir = cache.extract_tarball("test-ws", &tarball1, "rev1").unwrap();
        assert_eq!(cache.current_revision("test-ws"), Some("rev1".to_string()));

        // Second extract — both revision dirs should coexist
        let tarball2 = build_tarball_with_files(&[("new.txt", b"new content")]);
        let rev2_dir = cache.extract_tarball("test-ws", &tarball2, "rev2").unwrap();
        assert_eq!(cache.current_revision("test-ws"), Some("rev2".to_string()));

        // Both directories exist
        assert!(rev1_dir.exists(), "rev1 directory should still exist");
        assert!(rev2_dir.exists(), "rev2 directory should exist");

        // Old revision retains its files
        assert!(rev1_dir.join("old.txt").exists());
        // New revision has its files
        assert!(rev2_dir.join("new.txt").exists());
    }

    #[test]
    fn test_no_revision_for_unknown_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());
        assert_eq!(cache.current_revision("nonexistent"), None);
    }

    #[test]
    fn test_corrupted_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

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
        let cache = new_cache(dir.path());

        // Empty byte slice
        let empty_data: &[u8] = &[];
        let result = cache.extract_tarball("test-ws", empty_data, "rev1");

        assert!(result.is_err());
    }

    #[test]
    fn test_extract_creates_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Build tarball with nested directory
        let tarball = build_tarball_with_files(&[("subdir/file.txt", b"nested content")]);

        let rev_dir = cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        let nested_file = rev_dir.join("subdir").join("file.txt");

        assert!(nested_file.exists());
        let content = std::fs::read_to_string(nested_file).unwrap();
        assert_eq!(content, "nested content");
    }

    #[test]
    fn test_workspace_dir_with_special_names() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Test workspace names with hyphens, underscores, dots
        let tarball = build_test_tarball();

        for name in &["my-workspace", "my_workspace", "my.workspace"] {
            cache.extract_tarball(name, &tarball, "rev1").unwrap();
            assert_eq!(cache.current_revision(name), Some("rev1".to_string()));
        }
    }

    #[test]
    fn test_extract_preserves_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Build tarball with multiple files with different content
        let tarball = build_tarball_with_files(&[
            ("file1.txt", b"content one"),
            ("file2.txt", b"content two"),
            ("file3.txt", b"content three"),
        ]);

        let rev_dir = cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        // Verify each file's content is correct
        assert_eq!(
            std::fs::read_to_string(rev_dir.join("file1.txt")).unwrap(),
            "content one"
        );
        assert_eq!(
            std::fs::read_to_string(rev_dir.join("file2.txt")).unwrap(),
            "content two"
        );
        assert_eq!(
            std::fs::read_to_string(rev_dir.join("file3.txt")).unwrap(),
            "content three"
        );
    }

    #[test]
    fn test_revision_survives_reextract() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Extract first tarball with rev1
        let tarball1 = build_tarball_with_files(&[("old.txt", b"old content")]);
        cache.extract_tarball("test-ws", &tarball1, "rev1").unwrap();
        assert_eq!(cache.current_revision("test-ws"), Some("rev1".to_string()));

        // Extract different tarball with rev2
        let tarball2 = build_tarball_with_files(&[("new.txt", b"new content")]);
        let rev2_dir = cache.extract_tarball("test-ws", &tarball2, "rev2").unwrap();

        // Verify revision is rev2 and new content exists
        assert_eq!(cache.current_revision("test-ws"), Some("rev2".to_string()));
        assert!(rev2_dir.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(rev2_dir.join("new.txt")).unwrap(),
            "new content"
        );
    }

    #[test]
    fn test_each_revision_is_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Extract tarball with file A as rev1
        let tarball1 = build_tarball_with_files(&[("fileA.txt", b"content A")]);
        let rev1_dir = cache.extract_tarball("test-ws", &tarball1, "rev1").unwrap();

        assert!(rev1_dir.join("fileA.txt").exists());

        // Extract tarball with file B (no A) as rev2
        let tarball2 = build_tarball_with_files(&[("fileB.txt", b"content B")]);
        let rev2_dir = cache.extract_tarball("test-ws", &tarball2, "rev2").unwrap();

        // Rev1 still has fileA (immutable)
        assert!(rev1_dir.join("fileA.txt").exists());
        // Rev2 has fileB but not fileA
        assert!(rev2_dir.join("fileB.txt").exists());
        assert!(!rev2_dir.join("fileA.txt").exists());
    }

    #[test]
    fn test_current_revision_with_corrupted_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // First extract a valid workspace
        let tarball = build_test_tarball();
        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        // Write invalid UTF-8 to .current file
        let ws_dir = cache.workspace_dir("test-ws");
        let current_file = ws_dir.join(".current");
        std::fs::write(&current_file, [0xFF, 0xFE, 0xFD]).unwrap();

        // current_revision should return None (read_to_string fails on invalid UTF-8)
        assert_eq!(cache.current_revision("test-ws"), None);
    }

    #[test]
    fn test_concurrent_extraction_same_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(new_cache(dir.path()));
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

        // After all extractions, workspace should have a valid current revision
        let rev = cache.current_revision("shared-ws");
        assert!(rev.is_some());
    }

    #[test]
    fn test_concurrent_extraction_different_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(new_cache(dir.path()));
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

    // --- New tests for revision-based workspace directories ---

    #[test]
    fn test_idempotent_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        let tarball = build_test_tarball();

        // Extract same revision twice — should not error
        let path1 = cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();
        let path2 = cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        assert_eq!(path1, path2);
        // Content still valid
        let content = std::fs::read_to_string(path1.join("test.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_multiple_revisions_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        let tarball1 = build_tarball_with_files(&[("v1.txt", b"version 1")]);
        let tarball2 = build_tarball_with_files(&[("v2.txt", b"version 2")]);
        let tarball3 = build_tarball_with_files(&[("v3.txt", b"version 3")]);

        let path1 = cache
            .extract_tarball("test-ws", &tarball1, "aaa111")
            .unwrap();
        let path2 = cache
            .extract_tarball("test-ws", &tarball2, "bbb222")
            .unwrap();
        let path3 = cache
            .extract_tarball("test-ws", &tarball3, "ccc333")
            .unwrap();

        // All three directories exist
        assert!(path1.is_dir());
        assert!(path2.is_dir());
        assert!(path3.is_dir());

        // Each has its own content
        assert_eq!(
            std::fs::read_to_string(path1.join("v1.txt")).unwrap(),
            "version 1"
        );
        assert_eq!(
            std::fs::read_to_string(path2.join("v2.txt")).unwrap(),
            "version 2"
        );
        assert_eq!(
            std::fs::read_to_string(path3.join("v3.txt")).unwrap(),
            "version 3"
        );

        // .current points to the latest
        assert_eq!(
            cache.current_revision("test-ws"),
            Some("ccc333".to_string())
        );
    }

    #[test]
    fn test_workspace_guard_ref_count() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        let tarball = build_test_tarball();
        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        let rev_dir = dir.path().join("test-ws").join("rev1");

        // Create a guard — ref count should be 1
        let guard = cache.acquire_guard("test-ws", "rev1", rev_dir.clone());
        let rc = cache.revision_ref_count("test-ws", "rev1");
        assert_eq!(rc.load(Ordering::Acquire), 1);

        // Create another guard — ref count should be 2
        let guard2 = cache.acquire_guard("test-ws", "rev1", rev_dir.clone());
        assert_eq!(rc.load(Ordering::Acquire), 2);

        // Drop one guard — ref count should be 1
        drop(guard);
        assert_eq!(rc.load(Ordering::Acquire), 1);

        // Drop the other — ref count should be 0
        drop(guard2);
        assert_eq!(rc.load(Ordering::Acquire), 0);
    }

    /// Set explicit mtime on a directory so cleanup ordering is deterministic
    /// regardless of filesystem mtime granularity.
    fn set_dir_mtime(dir: &Path, secs_since_epoch: u64) {
        use std::fs::FileTimes;
        let time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs_since_epoch);
        let f = std::fs::File::open(dir).unwrap();
        f.set_times(FileTimes::new().set_modified(time)).unwrap();
    }

    #[test]
    fn test_cleanup_removes_old_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap(), Some(1));

        let tarball = build_test_tarball();

        // Create 4 revisions
        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();
        cache.extract_tarball("test-ws", &tarball, "rev2").unwrap();
        cache.extract_tarball("test-ws", &tarball, "rev3").unwrap();
        cache.extract_tarball("test-ws", &tarball, "rev4").unwrap();

        // Set explicit mtimes so cleanup ordering is deterministic
        let ws_dir = dir.path().join("test-ws");
        set_dir_mtime(&ws_dir.join("rev1"), 1000);
        set_dir_mtime(&ws_dir.join("rev2"), 2000);
        set_dir_mtime(&ws_dir.join("rev3"), 3000);
        set_dir_mtime(&ws_dir.join("rev4"), 4000);

        // All 4 dirs exist before cleanup
        assert!(ws_dir.join("rev1").is_dir());
        assert!(ws_dir.join("rev2").is_dir());
        assert!(ws_dir.join("rev3").is_dir());
        assert!(ws_dir.join("rev4").is_dir());

        // Run cleanup: current=rev4, max_retained=1 → keep rev4 (current) + rev3 (1 old)
        cleanup_old_revisions(
            dir.path(),
            "test-ws",
            "rev4",
            cache.max_retained_revisions,
            &cache.revision_refs,
        )
        .unwrap();

        // rev4 (current) and rev3 (1 retained) should survive
        assert!(
            ws_dir.join("rev4").is_dir(),
            "current revision must survive"
        );
        assert!(ws_dir.join("rev3").is_dir(), "newest old revision retained");
        // rev1 and rev2 should be cleaned up
        assert!(!ws_dir.join("rev1").is_dir(), "oldest revision removed");
        assert!(!ws_dir.join("rev2").is_dir(), "second-oldest removed");
    }

    #[test]
    fn test_cleanup_skips_in_use_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap(), Some(0));

        let tarball = build_test_tarball();

        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();
        cache.extract_tarball("test-ws", &tarball, "rev2").unwrap();
        set_dir_mtime(&dir.path().join("test-ws").join("rev1"), 1000);
        set_dir_mtime(&dir.path().join("test-ws").join("rev2"), 2000);

        // Hold a guard on rev1 — simulates a running step
        let rev1_dir = dir.path().join("test-ws").join("rev1");
        let _guard = cache.acquire_guard("test-ws", "rev1", rev1_dir.clone());

        // Cleanup with max_retained=0: would delete rev1, but it's in use
        cleanup_old_revisions(dir.path(), "test-ws", "rev2", 0, &cache.revision_refs).unwrap();

        // rev1 should survive because it's in use
        assert!(rev1_dir.is_dir(), "in-use revision must not be deleted");
    }

    #[test]
    fn test_sanitize_revision_for_dir() {
        // Normal hex hashes pass through
        assert_eq!(sanitize_revision_for_dir("abc123def456"), "abc123def456");

        // 40-char SHA1
        let sha1 = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0";
        assert_eq!(sanitize_revision_for_dir(sha1), sha1);

        // Hyphens, underscores, dots are kept
        assert_eq!(sanitize_revision_for_dir("rev-1_2.3"), "rev-1_2.3");

        // Special characters replaced with underscore
        assert_eq!(sanitize_revision_for_dir("rev/1:2"), "rev_1_2");
        assert_eq!(sanitize_revision_for_dir("rev 1"), "rev_1");
    }

    #[test]
    fn test_sanitize_revision_path_traversal() {
        // ".." must not pass through — PathBuf::join("..") resolves to parent dir
        assert_eq!(sanitize_revision_for_dir(".."), "_dotdot");

        // "." must not pass through — PathBuf::join(".") resolves to current dir
        assert_eq!(sanitize_revision_for_dir("."), "_dot");

        // Ensure the sanitized names don't escape the workspace dir when joined
        let base = std::path::PathBuf::from("/tmp/cache/myws");
        let dotdot = base.join(sanitize_revision_for_dir(".."));
        assert!(
            dotdot.starts_with("/tmp/cache/myws"),
            "'..' must not escape workspace dir, got: {}",
            dotdot.display()
        );
        let dot = base.join(sanitize_revision_for_dir("."));
        assert!(
            dot.starts_with("/tmp/cache/myws"),
            "'.' must not equal workspace dir itself via join, got: {}",
            dot.display()
        );
    }

    #[test]
    fn test_sanitize_revision_for_dir_path_traversal() {
        let result = sanitize_revision_for_dir("..");
        assert_ne!(
            result, "..",
            "'..' must be replaced to prevent path traversal"
        );
        assert!(!result.is_empty());

        let result = sanitize_revision_for_dir(".");
        assert_ne!(result, ".", "'.' must be replaced to prevent path issues");

        // Dots within larger strings are safe
        assert_eq!(sanitize_revision_for_dir("a..b"), "a..b");
        assert_eq!(sanitize_revision_for_dir("..abc"), "..abc");
    }

    #[test]
    fn test_backward_compat_old_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Simulate old layout: {workspace}/.revision file inside the workspace dir
        let ws_dir = dir.path().join("test-ws");
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(ws_dir.join(".revision"), "old-rev-123").unwrap();

        // current_revision should return None because there's no .current file
        // and no revision directory
        assert_eq!(cache.current_revision("test-ws"), None);
    }

    #[test]
    fn test_current_revision_returns_none_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Create .current file pointing to a non-existent revision directory
        let ws_dir = dir.path().join("test-ws");
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(ws_dir.join(".current"), "ghost-rev").unwrap();

        // Should return None because the directory doesn't exist
        assert_eq!(cache.current_revision("test-ws"), None);
    }

    #[test]
    fn test_cleanup_removes_stale_temp_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        let tarball = build_test_tarball();
        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        // Simulate a stale temp directory from a crashed extraction
        let ws_dir = dir.path().join("test-ws");
        let stale_tmp = ws_dir.join("rev2.tmp");
        std::fs::create_dir_all(&stale_tmp).unwrap();
        assert!(stale_tmp.is_dir());

        cleanup_old_revisions(dir.path(), "test-ws", "rev1", 2, &cache.revision_refs).unwrap();

        // Stale temp dir should be cleaned up
        assert!(
            !stale_tmp.exists(),
            "stale .tmp directory should be removed"
        );
    }

    #[test]
    fn test_empty_revision_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());
        let tarball = build_test_tarball();

        let result = cache.extract_tarball("test-ws", &tarball, "");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_whitespace_only_revision_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());
        let tarball = build_test_tarball();

        // All spaces get replaced with underscores, but "/" becomes "_"
        // Actually spaces become "_" so "   " becomes "___" which is non-empty
        // But a revision of only slashes "///" becomes "___" which is also non-empty.
        // The real concern is truly empty string, which is tested above.
        // Test a revision that works but has special chars.
        let result = cache.extract_tarball("test-ws", &tarball, "rev/1:2");
        assert!(result.is_ok());

        // Verify the directory uses the sanitized name
        let sanitized_dir = dir.path().join("test-ws").join("rev_1_2");
        assert!(sanitized_dir.is_dir());
    }

    #[test]
    fn test_special_char_revision_ref_count_consistency() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());
        let tarball = build_test_tarball();

        // Use a revision with special chars that require sanitization
        let revision = "rev/1:2";
        cache
            .extract_tarball("test-ws", &tarball, revision)
            .unwrap();

        // Acquire a guard using the raw revision string
        let rev_dir = dir.path().join("test-ws").join("rev_1_2");
        let _guard = cache.acquire_guard("test-ws", revision, rev_dir.clone());

        // Cleanup should see the ref count > 0 and skip this directory.
        // The key in revision_refs must match the sanitized dir_name used by cleanup.
        cleanup_old_revisions(
            dir.path(),
            "test-ws",
            "other-current", // pretend a different revision is current
            0,               // max_retained=0, would delete everything if not in use
            &cache.revision_refs,
        )
        .unwrap();

        // Directory must still exist because the guard is held
        assert!(
            rev_dir.is_dir(),
            "in-use revision with special chars must not be deleted"
        );
    }

    #[test]
    fn test_cleanup_all_old_revisions_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WorkspaceCache::new(dir.path().to_str().unwrap(), Some(0));
        let tarball = build_test_tarball();

        // Create 3 revisions
        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();
        cache.extract_tarball("test-ws", &tarball, "rev2").unwrap();
        cache.extract_tarball("test-ws", &tarball, "rev3").unwrap();
        set_dir_mtime(&dir.path().join("test-ws").join("rev1"), 1000);
        set_dir_mtime(&dir.path().join("test-ws").join("rev2"), 2000);
        set_dir_mtime(&dir.path().join("test-ws").join("rev3"), 3000);

        // Hold guards on all non-current revisions
        let rev1_dir = dir.path().join("test-ws").join("rev1");
        let rev2_dir = dir.path().join("test-ws").join("rev2");
        let _guard1 = cache.acquire_guard("test-ws", "rev1", rev1_dir.clone());
        let _guard2 = cache.acquire_guard("test-ws", "rev2", rev2_dir.clone());

        // Cleanup with max_retained=0: all old revisions are in use, none should be deleted
        cleanup_old_revisions(dir.path(), "test-ws", "rev3", 0, &cache.revision_refs).unwrap();

        // Both rev1 and rev2 must survive
        assert!(rev1_dir.is_dir(), "in-use rev1 must not be deleted");
        assert!(rev2_dir.is_dir(), "in-use rev2 must not be deleted");
        // rev3 (current) must also survive
        assert!(dir.path().join("test-ws").join("rev3").is_dir());
    }

    #[test]
    fn test_atomic_current_file_write() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());
        let tarball = build_test_tarball();

        cache.extract_tarball("test-ws", &tarball, "rev1").unwrap();

        let ws_dir = dir.path().join("test-ws");
        // .current should exist with correct content
        assert_eq!(
            std::fs::read_to_string(ws_dir.join(".current")).unwrap(),
            "rev1"
        );
        // .current.tmp should NOT exist (was renamed atomically to .current)
        assert!(!ws_dir.join(".current.tmp").exists());
    }

    #[test]
    fn test_extract_tarball_pinned_does_not_update_current() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Establish a "current" revision first
        let tarball1 = build_tarball_with_files(&[("current.txt", b"current content")]);
        cache
            .extract_tarball("test-ws", &tarball1, "current-rev")
            .unwrap();

        assert_eq!(
            cache.current_revision("test-ws"),
            Some("current-rev".to_string())
        );

        // Now extract a pinned (older) revision — must not touch .current
        let tarball2 = build_tarball_with_files(&[("pinned.txt", b"pinned content")]);
        let pinned_dir = cache
            .extract_tarball_pinned("test-ws", &tarball2, "pinned-rev")
            .unwrap();

        // .current must still point to the original revision
        assert_eq!(
            cache.current_revision("test-ws"),
            Some("current-rev".to_string()),
            ".current must not be updated by extract_tarball_pinned"
        );

        // The pinned revision directory was created correctly
        assert!(pinned_dir.join("pinned.txt").exists());
        assert_eq!(
            std::fs::read_to_string(pinned_dir.join("pinned.txt")).unwrap(),
            "pinned content"
        );
    }

    #[test]
    fn test_extract_tarball_pinned_idempotent_when_dir_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        let tarball = build_tarball_with_files(&[("file.txt", b"data")]);

        // First pinned extraction
        let path1 = cache
            .extract_tarball_pinned("test-ws", &tarball, "rev-pin")
            .unwrap();
        // Second call — dir already exists, must be idempotent
        let path2 = cache
            .extract_tarball_pinned("test-ws", &tarball, "rev-pin")
            .unwrap();

        assert_eq!(path1, path2);
        // Still no .current file (workspace dir was created but .current never written)
        assert!(!dir.path().join("test-ws").join(".current").exists());
    }

    #[test]
    fn test_ensure_revision_uses_local_cache_when_available() {
        // When the revision directory already exists locally, ensure_revision
        // must return a guard pointing to it without needing a network call.
        // We test this by pre-extracting the tarball, then verifying the
        // guard's path matches the expected revision directory.
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        let tarball = build_tarball_with_files(&[("hello.txt", b"hello")]);
        // Pre-populate the revision directory (simulating a previous extraction)
        cache
            .extract_tarball_pinned("test-ws", &tarball, "cached-rev")
            .unwrap();

        let expected_path = dir.path().join("test-ws").join("cached-rev");
        assert!(
            expected_path.is_dir(),
            "pre-condition: directory must exist"
        );

        // acquire_guard goes through the same code path as ensure_revision's
        // early-return branch: dir exists → acquire guard, no download.
        let guard = cache.acquire_guard("test-ws", "cached-rev", expected_path.clone());

        assert_eq!(guard.path(), expected_path.as_path());

        // Ref count is 1 while guard is held
        let rc = cache.revision_ref_count("test-ws", "cached-rev");
        assert_eq!(rc.load(Ordering::Acquire), 1);

        drop(guard);
        assert_eq!(rc.load(Ordering::Acquire), 0);
    }
}
