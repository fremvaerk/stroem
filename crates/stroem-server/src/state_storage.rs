use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use uuid::Uuid;

/// Pluggable archive backend for task state snapshots.
///
/// Operates on raw bytes (gzip tarballs). Each implementation handles
/// a different storage backend (S3, local filesystem).
#[async_trait]
pub trait StateArchive: Send + Sync {
    /// Store a state snapshot under the given key.
    async fn store(&self, key: &str, data: &[u8]) -> Result<()>;
    /// Retrieve a state snapshot. Returns `None` if the key doesn't exist.
    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Delete a state snapshot.
    async fn delete(&self, key: &str) -> Result<()>;
}

// ─── S3 State Archive Backend ─────────────────────────────────────────────

#[cfg(feature = "s3")]
pub use s3_state_archive::S3StateArchive;

#[cfg(feature = "s3")]
mod s3_state_archive {
    use super::*;
    use crate::config::ArchiveConfig;

    /// S3-backed state archive implementation.
    pub struct S3StateArchive {
        client: aws_sdk_s3::Client,
        bucket: String,
    }

    impl S3StateArchive {
        /// Create from an `ArchiveConfig` (production init — builds AWS SDK client).
        pub async fn from_config(config: &ArchiveConfig) -> Result<Self> {
            let region = config
                .region
                .as_deref()
                .context("S3 state archive requires 'region' field")?;
            let bucket = config
                .bucket
                .as_deref()
                .context("S3 state archive requires 'bucket' field")?;

            let mut aws_config_builder =
                aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .region(aws_sdk_s3::config::Region::new(region.to_string()));

            if let Some(ref endpoint) = config.endpoint {
                aws_config_builder = aws_config_builder.endpoint_url(endpoint);
            }

            let aws_config = aws_config_builder.load().await;
            let s3_sdk_config = aws_sdk_s3::config::Builder::from(&aws_config)
                .force_path_style(config.endpoint.is_some())
                .build();
            let client = aws_sdk_s3::Client::from_conf(s3_sdk_config);

            tracing::info!("S3 state archival enabled: bucket={}", bucket);

            Ok(Self {
                client,
                bucket: bucket.to_string(),
            })
        }

        /// Create from a pre-built client (for testing).
        pub fn from_client(client: aws_sdk_s3::Client, bucket: String) -> Self {
            Self { client, bucket }
        }
    }

    #[async_trait]
    impl StateArchive for S3StateArchive {
        async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .content_type("application/gzip")
                .body(data.to_vec().into())
                .send()
                .await
                .with_context(|| format!("Failed to store state snapshot to S3: {}", key))?;
            tracing::info!("Stored state snapshot to S3: s3://{}/{}", self.bucket, key);
            Ok(())
        }

        async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
            match self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
            {
                Ok(output) => {
                    let bytes = output
                        .body
                        .collect()
                        .await
                        .context("Failed to read S3 state object body")?
                        .into_bytes();
                    Ok(Some(bytes.to_vec()))
                }
                Err(sdk_err) => {
                    if let aws_sdk_s3::error::SdkError::ServiceError(ref service_err) = sdk_err {
                        if service_err.err().is_no_such_key() {
                            return Ok(None);
                        }
                    }
                    Err(anyhow::anyhow!("S3 state retrieve failed: {}", sdk_err))
                }
            }
        }

        async fn delete(&self, key: &str) -> Result<()> {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .with_context(|| format!("Failed to delete S3 state snapshot: {}", key))?;
            tracing::debug!("Deleted S3 state snapshot: {}", key);
            Ok(())
        }
    }
}

// ─── Local State Archive Backend ─────────────────────────────────────────

/// Local filesystem state archive backend.
///
/// Maps archive keys to files under `base_path` — the key's `/` separators
/// become subdirectories.
pub struct LocalStateArchive {
    base_path: std::path::PathBuf,
}

impl LocalStateArchive {
    /// Create a new local state archive rooted at `base_path`.
    pub fn new(base_path: impl AsRef<std::path::Path>) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
        }
    }

    fn key_path(&self, key: &str) -> Result<std::path::PathBuf> {
        // Reject keys with path traversal components
        if key.contains("..") || key.starts_with('/') {
            anyhow::bail!(
                "Invalid state archive key (path traversal attempt): {}",
                key
            );
        }
        Ok(self.base_path.join(key))
    }
}

#[async_trait]
impl StateArchive for LocalStateArchive {
    async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.key_path(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create state archive dir: {:?}", parent))?;
        }
        // Write atomically: write to a temp file, then rename into place.
        let tmp_path = path.with_extension("tmp");
        tokio::fs::write(&tmp_path, data)
            .await
            .with_context(|| format!("Failed to write temp state file: {:?}", tmp_path))?;
        tokio::fs::rename(&tmp_path, &path)
            .await
            .with_context(|| format!("Failed to rename state file into place: {:?}", path))?;
        tracing::debug!("Stored state snapshot to local file: {:?}", path);
        Ok(())
    }

    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.key_path(key)?;
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Failed to read state file: {:?}", path)),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.key_path(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                tracing::debug!("Deleted local state file: {:?}", path);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Failed to delete state file: {:?}", path)),
        }
    }
}

// ─── In-Memory State Archive (for testing) ───────────────────────────────

/// In-memory state archive for testing.
pub struct InMemoryStateArchive {
    data: std::sync::RwLock<std::collections::HashMap<String, Vec<u8>>>,
}

impl InMemoryStateArchive {
    /// Create a new empty in-memory state archive.
    pub fn new() -> Self {
        Self {
            data: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemoryStateArchive {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StateArchive for InMemoryStateArchive {
    async fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        self.data
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), data.to_vec());
        Ok(())
    }

    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.data
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        Ok(())
    }
}

// ─── StateStorage ─────────────────────────────────────────────────────────

/// High-level wrapper around [`StateArchive`] for task state operations.
///
/// Handles key construction and delegates storage operations to the
/// configured backend.
pub struct StateStorage {
    archive: Arc<dyn StateArchive>,
    prefix: String,
    max_snapshots: usize,
    global_max_snapshots: Option<usize>,
}

impl StateStorage {
    /// Create a new `StateStorage` backed by the given archive.
    pub fn new(
        archive: Arc<dyn StateArchive>,
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
        self.archive.store(key, data).await
    }

    /// Retrieve a state snapshot. Returns `None` if not found.
    pub async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.archive.retrieve(key).await
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
    use std::sync::Arc;
    use tempfile::TempDir;

    // ─── LocalStateArchive tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_local_archive_store_and_retrieve() {
        let dir = TempDir::new().unwrap();
        let archive = LocalStateArchive::new(dir.path());
        archive.store("ws/task/job.tar.gz", b"hello").await.unwrap();
        let data = archive.retrieve("ws/task/job.tar.gz").await.unwrap();
        assert_eq!(data, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_local_archive_retrieve_missing() {
        let dir = TempDir::new().unwrap();
        let archive = LocalStateArchive::new(dir.path());
        assert_eq!(archive.retrieve("nonexistent").await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_local_archive_delete() {
        let dir = TempDir::new().unwrap();
        let archive = LocalStateArchive::new(dir.path());
        archive.store("key", b"data").await.unwrap();
        archive.delete("key").await.unwrap();
        assert_eq!(archive.retrieve("key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_local_archive_delete_missing_is_ok() {
        let dir = TempDir::new().unwrap();
        let archive = LocalStateArchive::new(dir.path());
        // Deleting a nonexistent key must not error
        archive.delete("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn test_local_archive_path_traversal_rejected() {
        let dir = TempDir::new().unwrap();
        let archive = LocalStateArchive::new(dir.path());
        assert!(archive.store("../escape", b"data").await.is_err());
        assert!(archive.retrieve("../escape").await.is_err());
        assert!(archive.store("/absolute", b"data").await.is_err());
    }

    // ─── InMemoryStateArchive tests ───────────────────────────────────

    #[tokio::test]
    async fn test_in_memory_archive_roundtrip() {
        let archive = InMemoryStateArchive::new();
        archive.store("k1", b"v1").await.unwrap();
        archive.store("k2", b"v2").await.unwrap();
        assert_eq!(archive.retrieve("k1").await.unwrap(), Some(b"v1".to_vec()));
        assert_eq!(archive.retrieve("k2").await.unwrap(), Some(b"v2".to_vec()));
        archive.delete("k1").await.unwrap();
        assert_eq!(archive.retrieve("k1").await.unwrap(), None);
        assert_eq!(archive.retrieve("k2").await.unwrap(), Some(b"v2".to_vec()));
    }

    // ─── StateStorage key format tests ───────────────────────────────

    #[test]
    fn test_state_storage_key_format() {
        let archive = Arc::new(InMemoryStateArchive::new());
        let storage = StateStorage::new(archive, "state/".to_string(), 5, None);
        let job_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = storage.storage_key("prod", "deploy", job_id);
        assert_eq!(
            key,
            "state/prod/deploy/550e8400-e29b-41d4-a716-446655440000.tar.gz"
        );
    }

    #[test]
    fn test_state_storage_key_no_prefix() {
        let archive = Arc::new(InMemoryStateArchive::new());
        let storage = StateStorage::new(archive, String::new(), 3, None);
        let job_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let key = storage.storage_key("ws", "task", job_id);
        assert_eq!(key, "ws/task/00000000-0000-0000-0000-000000000001.tar.gz");
    }

    #[test]
    fn test_state_storage_max_snapshots_returned() {
        let archive = Arc::new(InMemoryStateArchive::new());
        let storage = StateStorage::new(archive, "pfx/".to_string(), 7, None);
        assert_eq!(storage.max_snapshots(), 7);
    }

    #[test]
    fn test_global_storage_key_format() {
        let archive = Arc::new(InMemoryStateArchive::new());
        let storage = StateStorage::new(archive, "state/".to_string(), 5, None);
        let job_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let key = storage.global_storage_key("prod", job_id);
        assert_eq!(
            key,
            "state/__global__/prod/550e8400-e29b-41d4-a716-446655440000.tar.gz"
        );
    }

    #[test]
    fn test_global_storage_key_no_prefix() {
        let archive = Arc::new(InMemoryStateArchive::new());
        let storage = StateStorage::new(archive, String::new(), 3, None);
        let job_id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let key = storage.global_storage_key("ws", job_id);
        assert_eq!(
            key,
            "__global__/ws/00000000-0000-0000-0000-000000000001.tar.gz"
        );
    }

    #[test]
    fn test_global_storage_key_path_traversal_sanitized() {
        let archive = Arc::new(InMemoryStateArchive::new());
        let storage = StateStorage::new(archive, "s/".to_string(), 3, None);
        let job_id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let key = storage.global_storage_key("a/../b", job_id);
        assert_eq!(
            key,
            "s/__global__/a/__/b/00000000-0000-0000-0000-000000000002.tar.gz"
        );
    }
}
