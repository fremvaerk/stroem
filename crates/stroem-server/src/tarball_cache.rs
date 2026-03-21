use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonically increasing counter used to generate unique tmp-file names.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Filesystem-based tarball cache keyed by (workspace, revision).
///
/// Layout: `{cache_dir}/{workspace}/{sanitized_revision}.tar.gz`
///
/// Tarballs are written atomically (tmp file + rename) to prevent partial reads.
/// The cache serves pre-built workspace tarballs so the server can deliver a
/// pinned revision to workers even after the workspace has been updated.
///
/// # Example
///
/// ```no_run
/// use stroem_server::tarball_cache::TarballCache;
/// use std::collections::HashSet;
/// use std::path::PathBuf;
///
/// let cache = TarballCache::new(PathBuf::from("/var/cache/stroem/tarballs"));
/// let data = b"...tarball bytes...";
/// cache.put("default", "abc123", data).unwrap();
///
/// let bytes = cache.get("default", "abc123");
/// assert!(bytes.is_some());
///
/// let keep = HashSet::from(["abc123".to_string()]);
/// cache.cleanup_workspace("default", &keep).unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct TarballCache {
    cache_dir: PathBuf,
}

impl TarballCache {
    /// Create a new [`TarballCache`] rooted at `cache_dir`.
    ///
    /// The directory is created if it does not exist.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Return a reference to the cache directory path.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Return the path where a tarball for `(workspace, revision)` would be stored.
    fn tarball_path(&self, workspace: &str, revision: &str) -> PathBuf {
        self.cache_dir
            .join(workspace)
            .join(format!("{}.tar.gz", sanitize_revision(revision)))
    }

    /// Read a cached tarball for `(workspace, revision)`.
    ///
    /// Returns `None` if the entry is not cached (file does not exist).
    /// IO errors while reading are logged as warnings and treated as a cache miss.
    pub fn get(&self, workspace: &str, revision: &str) -> Option<Vec<u8>> {
        let path = self.tarball_path(workspace, revision);
        match std::fs::read(&path) {
            Ok(data) => {
                tracing::debug!(
                    workspace = %workspace,
                    revision = %revision,
                    path = %path.display(),
                    "Tarball cache hit"
                );
                Some(data)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(
                    workspace = %workspace,
                    revision = %revision,
                    path = %path.display(),
                    "Failed to read cached tarball: {}",
                    e
                );
                None
            }
        }
    }

    /// Store a tarball for `(workspace, revision)` in the cache.
    ///
    /// Uses an atomic write (write to a `.tmp` file then rename) to prevent
    /// workers from reading a partially written tarball. Parent directories are
    /// created if they do not exist. Overwriting an existing entry is
    /// idempotent — the new data replaces the old atomically.
    pub fn put(&self, workspace: &str, revision: &str, data: &[u8]) -> Result<()> {
        let dest = self.tarball_path(workspace, revision);
        let parent = dest.parent().expect("tarball path always has a parent");

        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cache directory {}", parent.display()))?;

        // Use a unique counter so concurrent puts for the same key don't clobber
        // each other's tmp files before the rename.
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_name = format!("{}.{}.tmp", sanitize_revision(revision), seq);
        let tmp = parent.join(tmp_name);

        std::fs::write(&tmp, data)
            .with_context(|| format!("Failed to write tmp file {}", tmp.display()))?;

        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("Failed to rename {} to {}", tmp.display(), dest.display()))?;

        tracing::debug!(
            workspace = %workspace,
            revision = %revision,
            path = %dest.display(),
            bytes = data.len(),
            "Tarball cached"
        );

        Ok(())
    }

    /// Remove stale tarballs for `workspace`, keeping only the revisions in `keep_revisions`.
    ///
    /// `keep_revisions` must contain **raw** (unsanitized) revision strings — this
    /// method sanitizes them before comparing against filenames on disk.
    ///
    /// Only files whose names end in `.tar.gz` inside the workspace directory are
    /// considered. IO errors are logged as warnings but do not cause this method
    /// to fail. A non-existent workspace directory is silently ignored.
    pub fn cleanup_workspace(
        &self,
        workspace: &str,
        keep_revisions: &HashSet<String>,
    ) -> Result<()> {
        let ws_dir = self.cache_dir.join(workspace);

        let entries = match std::fs::read_dir(&ws_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    workspace = %workspace,
                    "Workspace cache directory does not exist, nothing to clean up"
                );
                return Ok(());
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "Failed to read workspace cache directory {}",
                        ws_dir.display()
                    )
                })
            }
        };

        // Build the set of sanitized revisions to keep for fast lookup.
        let keep_sanitized: HashSet<String> = keep_revisions
            .iter()
            .map(|r| sanitize_revision(r))
            .collect();

        for entry_result in entries {
            let entry = match entry_result {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        workspace = %workspace,
                        "Failed to read cache directory entry: {}",
                        e
                    );
                    continue;
                }
            };

            let file_name = entry.file_name().to_string_lossy().to_string();

            // Remove orphaned `.tmp` files left behind by crashed `put()` operations.
            if file_name.ends_with(".tmp") {
                let path = entry.path();
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        tracing::debug!(
                            workspace = %workspace,
                            path = %path.display(),
                            "Removed orphaned tmp file"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            workspace = %workspace,
                            path = %path.display(),
                            "Failed to remove orphaned tmp file: {}",
                            e
                        );
                    }
                }
                continue;
            }

            // Only touch files ending in `.tar.gz`.
            if !file_name.ends_with(".tar.gz") {
                continue;
            }

            // Extract the revision stem by stripping the `.tar.gz` suffix.
            let stem = &file_name[..file_name.len() - ".tar.gz".len()];

            if keep_sanitized.contains(stem) {
                continue;
            }

            // Not in the keep set — remove it.
            let path = entry.path();
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    tracing::debug!(
                        workspace = %workspace,
                        path = %path.display(),
                        "Removed stale cached tarball"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        workspace = %workspace,
                        path = %path.display(),
                        "Failed to remove stale cached tarball: {}",
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Remove cached tarballs for workspaces and revisions not in `keep_map`.
    ///
    /// For each workspace directory found inside `cache_dir`:
    /// - If the workspace appears in `keep_map`, delegates to [`cleanup_workspace`]
    ///   using the provided revision set.
    /// - If the workspace does **not** appear in `keep_map` (e.g. it was removed
    ///   from config), the entire workspace cache directory is deleted.
    ///
    /// IO errors for individual workspaces are logged as warnings but do not
    /// cause this method to fail. A non-existent cache directory is silently
    /// ignored.
    pub fn cleanup_all(&self, keep_map: &HashMap<String, HashSet<String>>) -> Result<()> {
        let entries = match std::fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("Failed to read cache dir {}", self.cache_dir.display())
                })
            }
        };

        for entry_result in entries {
            let entry = match entry_result {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("Tarball cache: failed to read cache entry: {}", e);
                    continue;
                }
            };

            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(e) => {
                    tracing::warn!(
                        "Tarball cache: failed to get file type for {}: {}",
                        entry.path().display(),
                        e
                    );
                    continue;
                }
            };

            if !file_type.is_dir() {
                continue;
            }

            let workspace = entry.file_name().to_string_lossy().to_string();

            if let Some(keep_revisions) = keep_map.get(&workspace) {
                // Workspace is known — prune stale revisions within it.
                if let Err(e) = self.cleanup_workspace(&workspace, keep_revisions) {
                    tracing::warn!(
                        "Tarball cache: cleanup_workspace failed for '{}': {:#}",
                        workspace,
                        e
                    );
                }
            } else {
                // Workspace no longer in config — remove the entire directory.
                let path = entry.path();
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => {
                        tracing::debug!(
                            "Tarball cache: removed stale workspace directory {}",
                            path.display()
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Tarball cache: failed to remove stale workspace directory {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

/// Sanitize a revision string for use as a filename component.
///
/// Any character that is not ASCII alphanumeric, `-`, `_`, or `.` is replaced
/// with `_`. This matches the `sanitize_revision_for_dir` pattern used by the
/// worker's `workspace_cache.rs`.
///
/// After character substitution, the special path components `..` and `.` are
/// replaced with `_dotdot_` and `_dot_` respectively to prevent path traversal.
///
/// # Example
///
/// ```
/// use stroem_server::tarball_cache::sanitize_revision;
///
/// assert_eq!(sanitize_revision("abc123"), "abc123");
/// assert_eq!(sanitize_revision("rev/1:2"), "rev_1_2");
/// assert_eq!(sanitize_revision("rev-1_2.3"), "rev-1_2.3");
/// assert_eq!(sanitize_revision("rev 1"), "rev_1");
/// assert_eq!(sanitize_revision(".."), "_dotdot_");
/// assert_eq!(sanitize_revision("."), "_dot_");
/// ```
pub fn sanitize_revision(revision: &str) -> String {
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

    // Guard against path traversal: `..` would resolve to the parent directory
    // when used in `PathBuf::join`, and `.` would resolve to the same directory.
    match sanitized.as_str() {
        ".." => "_dotdot_".to_string(),
        "." => "_dot_".to_string(),
        _ => sanitized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn new_cache(dir: &Path) -> TarballCache {
        TarballCache::new(dir.to_path_buf())
    }

    // ── Basic round-trip ────────────────────────────────────────────────────

    #[test]
    fn test_put_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        let data = b"fake tarball bytes";
        cache.put("my-workspace", "abc123", data).unwrap();

        let result = cache.get("my-workspace", "abc123");
        assert_eq!(result.as_deref(), Some(data.as_ref()));
    }

    // ── Cache miss ──────────────────────────────────────────────────────────

    #[test]
    fn test_get_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        assert!(cache.get("my-workspace", "nonexistent").is_none());
    }

    // ── Parent directory creation ────────────────────────────────────────────

    #[test]
    fn test_put_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // The workspace sub-directory does not exist yet.
        let ws_dir = dir.path().join("brand-new-workspace");
        assert!(!ws_dir.exists());

        cache.put("brand-new-workspace", "rev1", b"data").unwrap();

        assert!(ws_dir.is_dir());
        assert!(cache.get("brand-new-workspace", "rev1").is_some());
    }

    // ── Overwrite is idempotent ──────────────────────────────────────────────

    #[test]
    fn test_put_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        cache.put("ws", "rev1", b"first").unwrap();
        cache.put("ws", "rev1", b"second").unwrap();

        let result = cache.get("ws", "rev1").unwrap();
        assert_eq!(result, b"second");
    }

    // ── Cleanup removes stale entries ────────────────────────────────────────

    #[test]
    fn test_cleanup_removes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        cache.put("ws", "keep1", b"k1").unwrap();
        cache.put("ws", "keep2", b"k2").unwrap();
        cache.put("ws", "stale1", b"s1").unwrap();
        cache.put("ws", "stale2", b"s2").unwrap();

        let keep: HashSet<String> = ["keep1".to_string(), "keep2".to_string()]
            .into_iter()
            .collect();

        cache.cleanup_workspace("ws", &keep).unwrap();

        // Kept entries still present.
        assert!(cache.get("ws", "keep1").is_some());
        assert!(cache.get("ws", "keep2").is_some());

        // Stale entries removed.
        assert!(cache.get("ws", "stale1").is_none());
        assert!(cache.get("ws", "stale2").is_none());

        // Files on disk should not exist either.
        let ws_dir = dir.path().join("ws");
        assert!(!ws_dir.join("stale1.tar.gz").exists());
        assert!(!ws_dir.join("stale2.tar.gz").exists());
    }

    // ── Cleanup keeps matching entries ───────────────────────────────────────

    #[test]
    fn test_cleanup_keeps_matching() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        cache.put("ws", "rev1", b"r1").unwrap();
        cache.put("ws", "rev2", b"r2").unwrap();

        let keep: HashSet<String> = ["rev1".to_string(), "rev2".to_string()]
            .into_iter()
            .collect();

        cache.cleanup_workspace("ws", &keep).unwrap();

        assert_eq!(cache.get("ws", "rev1").as_deref(), Some(b"r1".as_ref()));
        assert_eq!(cache.get("ws", "rev2").as_deref(), Some(b"r2".as_ref()));
    }

    // ── Cleanup on non-existent workspace ────────────────────────────────────

    #[test]
    fn test_cleanup_nonexistent_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Should succeed silently.
        let keep = HashSet::new();
        cache.cleanup_workspace("does-not-exist", &keep).unwrap();
    }

    // ── Atomic write — concurrent puts don't corrupt ─────────────────────────

    #[test]
    fn test_atomic_write() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(new_cache(dir.path()));

        // Spin up several threads that all write the same (workspace, revision)
        // simultaneously with different payloads.
        let handles: Vec<_> = (0_u8..16)
            .map(|i| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || {
                    let payload = vec![i; 1024];
                    cache.put("ws", "concurrent-rev", &payload).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // After all threads finish the file must be readable and must be exactly
        // 1024 bytes (one of the payloads written atomically — never a mix).
        let result = cache.get("ws", "concurrent-rev").unwrap();
        assert_eq!(
            result.len(),
            1024,
            "concurrent puts must not corrupt the file (got {} bytes)",
            result.len()
        );
        // Every byte in the result must be the same value (i.e. one complete
        // payload, not a splice of two different ones).
        let byte = result[0];
        assert!(
            result.iter().all(|&b| b == byte),
            "file content must be one atomic payload, not a corrupted mix"
        );
    }

    // ── No tmp file left behind after successful put ──────────────────────────

    #[test]
    fn test_no_tmp_file_after_put() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        cache.put("ws", "rev1", b"content").unwrap();

        let ws_dir = dir.path().join("ws");
        for entry in std::fs::read_dir(&ws_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(
                !name.ends_with(".tmp"),
                "tmp file should not remain after successful put: {}",
                name
            );
        }
    }

    // ── Cleanup ignores non-.tar.gz files ────────────────────────────────────

    #[test]
    fn test_cleanup_ignores_non_tarball_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Put a legitimate tarball.
        cache.put("ws", "rev1", b"data").unwrap();

        // Write a non-tarball file directly into the workspace cache dir.
        let ws_dir = dir.path().join("ws");
        let other_file = ws_dir.join("somefile.txt");
        std::fs::write(&other_file, b"keep me").unwrap();

        // Cleanup with empty keep set.
        let keep = HashSet::new();
        cache.cleanup_workspace("ws", &keep).unwrap();

        // The non-.tar.gz file must be untouched.
        assert!(
            other_file.exists(),
            "cleanup must not remove non-.tar.gz files"
        );
    }

    // ── Cleanup handles sanitized keep-set revisions correctly ───────────────

    #[test]
    fn test_cleanup_sanitizes_keep_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Revision with a special character — stored as sanitized filename.
        cache.put("ws", "rev/1", b"data").unwrap();

        // The file on disk is named "rev_1.tar.gz".
        let ws_dir = dir.path().join("ws");
        assert!(ws_dir.join("rev_1.tar.gz").exists());

        // keep_revisions contains the RAW revision string.
        let keep: HashSet<String> = ["rev/1".to_string()].into_iter().collect();
        cache.cleanup_workspace("ws", &keep).unwrap();

        // The tarball must survive because its sanitized name matches.
        assert!(
            ws_dir.join("rev_1.tar.gz").exists(),
            "sanitized revision in keep set must be preserved"
        );
    }

    // ── cleanup_all ──────────────────────────────────────────────────────────

    #[test]
    fn test_cleanup_all_removes_unknown_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // Two workspaces in the cache.
        cache.put("ws-keep", "rev1", b"data").unwrap();
        cache.put("ws-remove", "rev1", b"data").unwrap();

        // Only ws-keep is in the keep_map.
        let mut keep_map = HashMap::new();
        keep_map.insert("ws-keep".to_string(), HashSet::from(["rev1".to_string()]));

        cache.cleanup_all(&keep_map).unwrap();

        // ws-keep survives.
        assert!(cache.get("ws-keep", "rev1").is_some());
        // ws-remove is gone entirely.
        assert!(!dir.path().join("ws-remove").exists());
    }

    #[test]
    fn test_cleanup_all_keeps_all_known_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        cache.put("ws1", "rev1", b"a").unwrap();
        cache.put("ws2", "rev2", b"b").unwrap();

        let mut keep_map = HashMap::new();
        keep_map.insert("ws1".to_string(), HashSet::from(["rev1".to_string()]));
        keep_map.insert("ws2".to_string(), HashSet::from(["rev2".to_string()]));

        cache.cleanup_all(&keep_map).unwrap();

        assert!(cache.get("ws1", "rev1").is_some());
        assert!(cache.get("ws2", "rev2").is_some());
    }

    #[test]
    fn test_cleanup_all_nonexistent_cache_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Point cache at a directory that doesn't exist.
        let cache = TarballCache::new(dir.path().join("nonexistent"));
        let keep_map = HashMap::new();
        // Should succeed silently.
        cache.cleanup_all(&keep_map).unwrap();
    }

    #[test]
    fn test_cleanup_all_prunes_stale_revisions_in_known_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        cache.put("ws", "keep-rev", b"keep").unwrap();
        cache.put("ws", "stale-rev", b"stale").unwrap();

        let mut keep_map = HashMap::new();
        keep_map.insert("ws".to_string(), HashSet::from(["keep-rev".to_string()]));

        cache.cleanup_all(&keep_map).unwrap();

        assert!(cache.get("ws", "keep-rev").is_some());
        assert!(cache.get("ws", "stale-rev").is_none());
    }

    // ── sanitize_revision path traversal (assert_ne style) ───────────────────

    #[test]
    fn test_sanitize_revision_path_traversal() {
        // ".." must not pass through — it would cause PathBuf::join to traverse up
        let result = sanitize_revision("..");
        assert_ne!(
            result, "..",
            "'..' must be replaced to prevent path traversal"
        );
        assert!(!result.is_empty());

        // "." must not pass through — it would resolve to current directory
        let result = sanitize_revision(".");
        assert_ne!(result, ".", "'.' must be replaced to prevent path issues");

        // Variations with padding are fine (they contain alphanumeric chars)
        assert_eq!(sanitize_revision("a..b"), "a..b"); // dots within a string are safe
        assert_eq!(sanitize_revision("..abc"), "..abc"); // leading dots with suffix are safe
    }

    // ── sanitize_revision collision ───────────────────────────────────────────

    #[test]
    fn test_sanitize_collision_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        // "rev/1" and "rev:1" both sanitize to "rev_1"
        cache.put("ws", "rev/1", b"first").unwrap();
        cache.put("ws", "rev:1", b"second").unwrap();

        // Both get() calls return the same (last written) value
        let r1 = cache.get("ws", "rev/1").unwrap();
        let r2 = cache.get("ws", "rev:1").unwrap();
        assert_eq!(r1, r2);
        assert_eq!(r1, b"second");
    }

    // ── cleanup removes orphaned .tmp files ───────────────────────────────────

    #[test]
    fn test_cleanup_removes_orphaned_tmp_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = new_cache(dir.path());

        cache.put("ws", "rev1", b"data").unwrap();

        // Simulate an orphaned tmp file from a crashed put()
        let ws_dir = dir.path().join("ws");
        let orphan = ws_dir.join("rev2.0.tmp");
        std::fs::write(&orphan, b"partial write").unwrap();
        assert!(orphan.exists());

        let keep: HashSet<String> = ["rev1".to_string()].into_iter().collect();
        cache.cleanup_workspace("ws", &keep).unwrap();

        // The orphaned .tmp file should be cleaned up
        assert!(
            !orphan.exists(),
            "orphaned .tmp files should be removed during cleanup"
        );
        // The kept tarball should still exist
        assert!(cache.get("ws", "rev1").is_some());
    }

    // ── sanitize_revision ────────────────────────────────────────────────────

    #[test]
    fn test_sanitize_revision() {
        // Alphanumeric characters pass through unchanged.
        assert_eq!(sanitize_revision("abc123"), "abc123");

        // 40-char hex SHA1 passes through unchanged.
        let sha1 = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0";
        assert_eq!(sanitize_revision(sha1), sha1);

        // Allowed punctuation passes through unchanged.
        assert_eq!(sanitize_revision("rev-1_2.3"), "rev-1_2.3");

        // Slashes, colons, and spaces are replaced with underscores.
        assert_eq!(sanitize_revision("rev/1:2"), "rev_1_2");
        assert_eq!(sanitize_revision("rev 1"), "rev_1");

        // A mix of several special characters.
        assert_eq!(sanitize_revision("refs/heads/main"), "refs_heads_main");

        // Empty string stays empty.
        assert_eq!(sanitize_revision(""), "");

        // Unicode characters are replaced with underscores.
        assert_eq!(sanitize_revision("rév"), "r_v");
    }

    // ── Path-traversal guard ─────────────────────────────────────────────────

    #[test]
    fn test_sanitize_revision_dotdot() {
        // `..` must never pass through — it would traverse to the parent directory.
        assert_eq!(
            sanitize_revision(".."),
            "_dotdot_",
            "'..' must be replaced with '_dotdot_'"
        );

        // `.` must also be replaced — it is a no-op component but signals intent.
        assert_eq!(
            sanitize_revision("."),
            "_dot_",
            "'.' must be replaced with '_dot_'"
        );

        // A revision that merely contains dots elsewhere must not be affected.
        assert_eq!(sanitize_revision("v1.2.3"), "v1.2.3");
        assert_eq!(sanitize_revision("rev-1_2.3"), "rev-1_2.3");

        // Verify that path traversal cannot occur: the sanitized revision joined
        // onto a base path must stay within that base path.
        let base = std::path::PathBuf::from("/var/cache/stroem/tarballs/ws");
        let joined = base.join(sanitize_revision(".."));
        assert!(
            joined.starts_with("/var/cache/stroem/tarballs/ws"),
            "sanitized '..' must not escape the base directory: got {}",
            joined.display()
        );
    }
}
