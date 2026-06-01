use anyhow::Result;
use std::sync::Arc;
use uuid::Uuid;

use crate::blob_storage::BlobArchive;

// ─── StateStorage ─────────────────────────────────────────────────────────

/// High-level wrapper around [`BlobArchive`] for task state operations.
///
/// Handles key construction and delegates storage operations to the
/// configured backend.
pub struct StateStorage {
    archive: Arc<dyn BlobArchive>,
    prefix: String,
    max_snapshots: usize,
    global_max_snapshots: Option<usize>,
}

impl StateStorage {
    /// Create a new `StateStorage` backed by the given archive.
    pub fn new(
        archive: Arc<dyn BlobArchive>,
        prefix: String,
        max_snapshots: usize,
        global_max_snapshots: Option<usize>,
    ) -> Self {
        Self {
            archive,
            prefix,
            max_snapshots,
            global_max_snapshots,
        }
    }

    /// Build the storage key for a state snapshot.
    ///
    /// Format: `{prefix}{workspace}/{task_name}/{job_id}.tar.gz`
    ///
    /// Path traversal sequences (`..`) are replaced with `__` to prevent
    /// archive key escape in both local-filesystem and S3 backends.
    pub fn storage_key(&self, workspace: &str, task_name: &str, job_id: Uuid) -> String {
        let safe_workspace = workspace.replace("..", "__");
        let safe_task_name = task_name.replace("..", "__");
        format!(
            "{}{}/{}/{}.tar.gz",
            self.prefix, safe_workspace, safe_task_name, job_id
        )
    }

    /// Build the storage key for a global workspace state snapshot.
    ///
    /// Format: `{prefix}__global__/{workspace}/{job_id}.tar.gz`
    ///
    /// Path traversal sequences (`..`) are replaced with `__` to prevent
    /// archive key escape in both local-filesystem and S3 backends.
    pub fn global_storage_key(&self, workspace: &str, job_id: Uuid) -> String {
        let safe_workspace = workspace.replace("..", "__");
        format!(
            "{}__global__/{}/{}.tar.gz",
            self.prefix, safe_workspace, job_id
        )
    }

    /// Store a state snapshot under the given key.
    pub async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        self.archive
            .put(key, "application/gzip", bytes::Bytes::copy_from_slice(data))
            .await
    }

    /// Retrieve a state snapshot. Returns `None` if not found.
    pub async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.archive.get(key).await?.map(|b| b.bytes.to_vec()))
    }

    /// Delete a state snapshot.
    pub async fn delete(&self, key: &str) -> Result<()> {
        self.archive.delete(key).await
    }

    /// Maximum number of snapshots to retain per task (workspace + task scoping).
    pub fn max_snapshots(&self) -> usize {
        self.max_snapshots
    }

    /// Maximum number of snapshots to retain for global workspace state.
    /// Falls back to `max_snapshots` when not explicitly configured.
    pub fn global_max_snapshots(&self) -> usize {
        self.global_max_snapshots.unwrap_or(self.max_snapshots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_storage::LocalBlobArchive;
    use std::sync::Arc;
    use tempfile::TempDir;

    // ─── StateStorage key format tests ───────────────────────────────

    fn make_archive(dir: &TempDir) -> Arc<dyn BlobArchive> {
        Arc::new(LocalBlobArchive::new(dir.path().to_path_buf()))
    }

    #[test]
    fn test_state_storage_key_format() {
        let dir = TempDir::new().unwrap();
        let storage = StateStorage::new(make_archive(&dir), "state/".to_string(), 5, None);
        let job_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = storage.storage_key("prod", "deploy", job_id);
        assert_eq!(
            key,
            "state/prod/deploy/550e8400-e29b-41d4-a716-446655440000.tar.gz"
        );
    }

    #[test]
    fn test_state_storage_key_no_prefix() {
        let dir = TempDir::new().unwrap();
        let storage = StateStorage::new(make_archive(&dir), String::new(), 3, None);
        let job_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let key = storage.storage_key("ws", "task", job_id);
        assert_eq!(key, "ws/task/00000000-0000-0000-0000-000000000001.tar.gz");
    }

    #[test]
    fn test_state_storage_max_snapshots_returned() {
        let dir = TempDir::new().unwrap();
        let storage = StateStorage::new(make_archive(&dir), "pfx/".to_string(), 7, None);
        assert_eq!(storage.max_snapshots(), 7);
    }

    #[test]
    fn test_global_storage_key_format() {
        let dir = TempDir::new().unwrap();
        let storage = StateStorage::new(make_archive(&dir), "state/".to_string(), 5, None);
        let job_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = storage.global_storage_key("prod", job_id);
        assert_eq!(
            key,
            "state/__global__/prod/550e8400-e29b-41d4-a716-446655440000.tar.gz"
        );
    }

    #[test]
    fn test_global_storage_key_no_prefix() {
        let dir = TempDir::new().unwrap();
        let storage = StateStorage::new(make_archive(&dir), String::new(), 3, None);
        let job_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let key = storage.global_storage_key("ws", job_id);
        assert_eq!(
            key,
            "__global__/ws/00000000-0000-0000-0000-000000000001.tar.gz"
        );
    }

    #[test]
    fn test_global_storage_key_path_traversal_sanitized() {
        let dir = TempDir::new().unwrap();
        let storage = StateStorage::new(make_archive(&dir), "s/".to_string(), 3, None);
        let job_id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let key = storage.global_storage_key("a/../b", job_id);
        assert_eq!(
            key,
            "s/__global__/a/__/b/00000000-0000-0000-0000-000000000002.tar.gz"
        );
    }

    // ─── Round-trip tests via LocalBlobArchive ────────────────────────

    #[tokio::test]
    async fn test_state_storage_round_trip() {
        let dir = TempDir::new().unwrap();
        let storage = StateStorage::new(make_archive(&dir), "state/".to_string(), 5, None);
        let job_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = storage.storage_key("ws", "task", job_id);
        storage.store(&key, b"snapshot-bytes").await.unwrap();
        let retrieved = storage.retrieve(&key).await.unwrap();
        assert_eq!(retrieved, Some(b"snapshot-bytes".to_vec()));
        storage.delete(&key).await.unwrap();
        assert_eq!(storage.retrieve(&key).await.unwrap(), None);
    }
}
