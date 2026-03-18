use anyhow::{Context, Result};
use async_trait::async_trait;
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Metadata needed to construct structured archive keys for job logs.
pub struct JobLogMeta {
    pub workspace: String,
    pub task_name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Pluggable archive backend for log storage.
///
/// Operates on raw bytes — gzip compression/decompression is handled by
/// `LogStorage` before calling these methods.
#[async_trait]
pub trait LogArchive: Send + Sync {
    /// Upload raw bytes to the archive under the given key.
    async fn upload(&self, key: &str, data: &[u8]) -> Result<()>;
    /// Download raw bytes from the archive. Returns `None` if the key doesn't exist.
    async fn download(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Delete an object from the archive.
    async fn delete(&self, key: &str) -> Result<()>;
}

/// Build a structured archive key from job metadata.
///
/// Format: `{prefix}{workspace}/{task}/YYYY/MM/DD/YYYY-MM-DDTHH-MM-SS_{job_id}.jsonl.gz`
pub fn archive_key(prefix: &str, job_id: Uuid, meta: &JobLogMeta) -> String {
    let dt = meta.created_at;
    format!(
        "{}{}/{}/{}/{}/{}_{}.jsonl.gz",
        prefix,
        meta.workspace,
        meta.task_name,
        dt.format("%Y"),
        dt.format("%m/%d"),
        dt.format("%Y-%m-%dT%H-%M-%S"),
        job_id,
    )
}

// ─── S3 Archive Backend ──────────────────────────────────────────────────

#[cfg(feature = "s3")]
pub use s3_archive::S3Archive;

#[cfg(feature = "s3")]
mod s3_archive {
    use super::*;
    use crate::config::ArchiveConfig;

    /// S3-backed archive implementation.
    pub struct S3Archive {
        client: aws_sdk_s3::Client,
        bucket: String,
    }

    impl S3Archive {
        /// Create from an `ArchiveConfig` (production init — builds AWS SDK client).
        pub async fn from_config(config: &ArchiveConfig) -> Result<Self> {
            let region = config
                .region
                .as_deref()
                .context("S3 archive requires 'region' field")?;
            let bucket = config
                .bucket
                .as_deref()
                .context("S3 archive requires 'bucket' field")?;

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

            tracing::info!("S3 log archival enabled: bucket={}", bucket);

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
    impl LogArchive for S3Archive {
        async fn upload(&self, key: &str, data: &[u8]) -> Result<()> {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .content_type("application/gzip")
                .body(data.to_vec().into())
                .send()
                .await
                .with_context(|| format!("Failed to upload log to S3: {}", key))?;
            tracing::info!("Uploaded logs to S3: s3://{}/{}", self.bucket, key);
            Ok(())
        }

        async fn download(&self, key: &str) -> Result<Option<Vec<u8>>> {
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
                        .context("Failed to read S3 object body")?
                        .into_bytes();
                    Ok(Some(bytes.to_vec()))
                }
                Err(sdk_err) => {
                    if let aws_sdk_s3::error::SdkError::ServiceError(ref service_err) = sdk_err {
                        if service_err.err().is_no_such_key() {
                            return Ok(None);
                        }
                    }
                    tracing::warn!("S3 log lookup failed (non-fatal): {}", sdk_err);
                    Ok(None)
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
                .with_context(|| format!("Failed to delete S3 log: {}", key))?;
            tracing::debug!("Deleted S3 log: {}", key);
            Ok(())
        }
    }
}

// ─── Local Archive Backend ───────────────────────────────────────────────

/// Local filesystem archive backend.
///
/// Maps archive keys to files under `base_path` — the key's `/` separators
/// become subdirectories.
pub struct LocalArchive {
    base_path: PathBuf,
}

impl LocalArchive {
    pub fn new(base_path: impl AsRef<Path>) -> Self {
        Self {
            base_path: base_path.as_ref().to_path_buf(),
        }
    }

    fn key_path(&self, key: &str) -> Result<PathBuf> {
        // Reject keys with path traversal components
        if key.contains("..") || key.starts_with('/') {
            anyhow::bail!("Invalid archive key (path traversal attempt): {}", key);
        }
        Ok(self.base_path.join(key))
    }
}

#[async_trait]
impl LogArchive for LocalArchive {
    async fn upload(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.key_path(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create archive dir: {:?}", parent))?;
        }
        fs::write(&path, data)
            .await
            .with_context(|| format!("Failed to write archive file: {:?}", path))?;
        tracing::debug!("Archived log to local file: {:?}", path);
        Ok(())
    }

    async fn download(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.key_path(key)?;
        match fs::read(&path).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Failed to read archive file: {:?}", path)),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.key_path(key)?;
        match fs::remove_file(&path).await {
            Ok(()) => {
                tracing::debug!("Deleted local archive file: {:?}", path);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Failed to delete archive file: {:?}", path)),
        }
    }
}

// ─── LogStorage ──────────────────────────────────────────────────────────

/// A cached, buffered file handle for a single job's log file.
type CachedHandle = Arc<Mutex<BufWriter<File>>>;

/// Log storage handles writing and reading job logs.
///
/// Live logs are always written to local disk as JSONL files. An optional
/// archive backend handles long-term storage (S3, local filesystem, etc.).
///
/// File handles are kept open and buffered across multiple [`append_log`] calls,
/// eliminating the open/close overhead on every chunk. Call [`close_log`] when a
/// job reaches a terminal state to flush and evict the handle.
#[derive(Clone)]
pub struct LogStorage {
    base_dir: PathBuf,
    /// Whether the base directory has been created at least once.
    dir_created: Arc<AtomicBool>,
    /// Open, buffered file handles keyed by job UUID.
    file_cache: Arc<DashMap<Uuid, CachedHandle>>,
    /// Pluggable archive backend (S3, local, etc.).
    archive: Option<Arc<dyn LogArchive>>,
    /// Key prefix for archive objects.
    archive_prefix: String,
}

impl LogStorage {
    /// Create a new log storage backed by `base_dir`.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            dir_created: Arc::new(AtomicBool::new(false)),
            file_cache: Arc::new(DashMap::new()),
            archive: None,
            archive_prefix: String::new(),
        }
    }

    /// Attach an archive backend with the given key prefix.
    pub fn with_archive(mut self, archive: Arc<dyn LogArchive>, prefix: String) -> Self {
        self.archive = Some(archive);
        self.archive_prefix = prefix;
        self
    }

    /// Get the JSONL log file path for a job.
    fn log_path(&self, job_id: Uuid) -> PathBuf {
        self.base_dir.join(format!("{job_id}.jsonl"))
    }

    /// Get the legacy .log file path (for backward compatibility).
    fn legacy_log_path(&self, job_id: Uuid) -> PathBuf {
        self.base_dir.join(format!("{job_id}.log"))
    }

    /// Ensure the log directory exists.
    ///
    /// Uses an [`AtomicBool`] flag so that the directory stat is skipped on
    /// subsequent calls once we know the directory has been created.
    async fn ensure_dir(&self) -> Result<()> {
        if self.dir_created.load(Ordering::Acquire) {
            return Ok(());
        }
        fs::create_dir_all(&self.base_dir)
            .await
            .context("Failed to create log directory")?;
        self.dir_created.store(true, Ordering::Release);
        Ok(())
    }

    /// Return the cached file handle for `job_id`, creating it if absent.
    ///
    /// The directory is only created once (guarded by `dir_created`).
    async fn get_or_open(&self, job_id: Uuid) -> Result<CachedHandle> {
        // Fast path: handle already in cache.
        if let Some(handle) = self.file_cache.get(&job_id) {
            return Ok(Arc::clone(&handle));
        }

        // Slow path: open the file and insert into cache.
        self.ensure_dir().await?;
        let path = self.log_path(job_id);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("Failed to open log file: {path:?}"))?;

        let handle: CachedHandle = Arc::new(Mutex::new(BufWriter::new(file)));

        // Another task may have raced us — `entry()` API on DashMap is
        // synchronous so we use `or_insert` to let the winner's handle win.
        let stored = self
            .file_cache
            .entry(job_id)
            .or_insert_with(|| Arc::clone(&handle));
        Ok(Arc::clone(&stored))
    }

    /// Append a log chunk to the job's log file.
    ///
    /// The file handle is kept open between calls (buffered via [`BufWriter`]),
    /// so only a single `open` syscall is paid over the lifetime of a job.
    /// Data is flushed to the OS on each call so readers can observe it
    /// immediately.
    pub async fn append_log(&self, job_id: Uuid, chunk: &str) -> Result<()> {
        let handle = self.get_or_open(job_id).await?;
        let mut writer = handle.lock().await;
        writer
            .write_all(chunk.as_bytes())
            .await
            .context("Failed to write to log file")?;
        writer.flush().await.context("Failed to flush log file")?;
        Ok(())
    }

    /// Flush and evict the cached file handle for `job_id`.
    ///
    /// Call this when a job reaches a terminal state so the `BufWriter`
    /// internal buffer is drained to disk and the file descriptor is released.
    /// It is safe to call when no handle exists (e.g. if the job wrote no
    /// logs); in that case the method is a no-op.
    pub async fn close_log(&self, job_id: Uuid) {
        if let Some((_, handle)) = self.file_cache.remove(&job_id) {
            // Flush the BufWriter. Ignore errors at close time — the data
            // has already been flushed on every `append_log` call.
            let mut writer = handle.lock().await;
            if let Err(err) = writer.flush().await {
                tracing::warn!("Failed to flush log file on close for job {job_id}: {err}");
            }
        }
    }

    /// Delete local log files for a job (both .jsonl and legacy .log).
    /// Returns true if any file was actually deleted.
    pub async fn delete_local_log(&self, job_id: Uuid) -> bool {
        self.close_log(job_id).await;
        let mut deleted = false;
        let jsonl_path = self.log_path(job_id);
        if jsonl_path.exists() {
            if let Err(e) = fs::remove_file(&jsonl_path).await {
                tracing::warn!("Failed to delete log file {:?}: {}", jsonl_path, e);
            } else {
                deleted = true;
            }
        }
        let legacy_path = self.legacy_log_path(job_id);
        if legacy_path.exists() {
            if let Err(e) = fs::remove_file(&legacy_path).await {
                tracing::warn!("Failed to delete legacy log file {:?}: {}", legacy_path, e);
            } else {
                deleted = true;
            }
        }
        deleted
    }

    /// Delete a job's log from the archive. No-op if no archive is configured.
    pub async fn delete_archive_log(&self, job_id: Uuid, meta: &JobLogMeta) -> Result<()> {
        if let Some(ref archive) = self.archive {
            let key = archive_key(&self.archive_prefix, job_id, meta);
            archive.delete(&key).await?;
        }
        Ok(())
    }

    /// Upload a job's log file to the archive (gzip-compressed). No-op if no archive is configured.
    pub async fn upload_to_archive(&self, job_id: Uuid, meta: &JobLogMeta) -> Result<()> {
        // Defensive flush: ensure any buffered writes are on disk before reading.
        self.close_log(job_id).await;

        if let Some(ref archive) = self.archive {
            let path = self.log_path(job_id);
            if !path.exists() {
                tracing::debug!(
                    "No local log file for job {}, skipping archive upload",
                    job_id
                );
                return Ok(());
            }

            let path_clone = path.clone();
            let compressed = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                use flate2::write::GzEncoder;
                use flate2::Compression;
                use std::io::Write;

                let raw = std::fs::read(&path_clone).with_context(|| {
                    format!(
                        "Failed to read log file for archive upload: {:?}",
                        path_clone
                    )
                })?;
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder
                    .write_all(&raw)
                    .context("Failed to gzip-compress log data")?;
                encoder
                    .finish()
                    .context("Failed to finish gzip compression")
            })
            .await
            .context("spawn_blocking panicked")??;

            let key = archive_key(&self.archive_prefix, job_id, meta);
            archive.upload(&key, &compressed).await?;
        }

        Ok(())
    }

    /// Download a job's log from the archive (gzip-compressed). Returns `None` if
    /// the key doesn't exist or no archive is configured.
    async fn get_log_from_archive(
        &self,
        job_id: Uuid,
        meta: &JobLogMeta,
    ) -> Result<Option<String>> {
        if let Some(ref archive) = self.archive {
            let key = archive_key(&self.archive_prefix, job_id, meta);
            if let Some(bytes) = archive.download(&key).await? {
                use flate2::read::GzDecoder;
                use std::io::Read;

                let mut decoder = GzDecoder::new(&bytes[..]);
                let mut content = String::new();
                decoder
                    .read_to_string(&mut content)
                    .context("Failed to gzip-decompress archive log content")?;

                return Ok(Some(content));
            }
        }
        Ok(None)
    }

    /// Get the full log contents for a job.
    ///
    /// Checks for `.jsonl` first, falls back to legacy `.log` file, then archive.
    pub async fn get_log(&self, job_id: Uuid, meta: &JobLogMeta) -> Result<String> {
        let path = self.log_path(job_id);

        if path.exists() {
            return Self::read_file(&path).await;
        }

        // Fallback to legacy .log file
        let legacy_path = self.legacy_log_path(job_id);
        if legacy_path.exists() {
            return Self::read_file(&legacy_path).await;
        }

        // Fallback to archive
        if let Some(content) = self.get_log_from_archive(job_id, meta).await? {
            return Ok(content);
        }

        Ok(String::new())
    }

    /// Read file contents as a UTF-8 string.
    async fn read_file(path: &Path) -> Result<String> {
        let mut file = fs::File::open(path)
            .await
            .with_context(|| format!("Failed to open log file: {:?}", path))?;

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .context("Failed to read log file")?;

        Ok(contents)
    }

    /// Get log lines for a specific step within a job.
    ///
    /// Reads local files line-by-line to avoid loading the entire log into
    /// memory. Falls back to archive when no local file is found.
    pub async fn get_step_log(
        &self,
        job_id: Uuid,
        step_name: &str,
        meta: &JobLogMeta,
    ) -> Result<String> {
        let path = self.log_path(job_id);
        if path.exists() {
            return self.filter_step_from_file(&path, step_name).await;
        }

        let legacy_path = self.legacy_log_path(job_id);
        if legacy_path.exists() {
            return self.filter_step_from_file(&legacy_path, step_name).await;
        }

        if let Some(result) = self
            .get_step_log_from_archive(job_id, step_name, meta)
            .await?
        {
            return Ok(result);
        }

        Ok(String::new())
    }

    /// Read a local file line-by-line, collecting only lines matching `step_name`.
    async fn filter_step_from_file(&self, path: &Path, step_name: &str) -> Result<String> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let file = fs::File::open(path)
            .await
            .with_context(|| format!("Failed to open log file: {:?}", path))?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut result = String::new();

        while let Some(line) = lines.next_line().await.context("Failed to read log line")? {
            if Self::line_matches_step(&line, step_name) {
                result.push_str(&line);
                result.push('\n');
            }
        }

        Ok(result)
    }

    /// Check if a JSONL line belongs to the given step.
    ///
    /// Uses a fast-path `contains` check to avoid JSON parsing for lines that
    /// cannot possibly match — the vast majority of lines in a multi-step job.
    fn line_matches_step(line: &str, step_name: &str) -> bool {
        // Fast path: if the step name doesn't appear anywhere in the line there
        // is no need to parse the JSON at all.
        if !line.contains(step_name) {
            return false;
        }
        // Parse only when the step name is present in the raw string.
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) {
            parsed.get("step").and_then(|s| s.as_str()) == Some(step_name)
        } else {
            false
        }
    }

    /// Download from archive, decompress, and return only lines matching `step_name`.
    async fn get_step_log_from_archive(
        &self,
        job_id: Uuid,
        step_name: &str,
        meta: &JobLogMeta,
    ) -> Result<Option<String>> {
        let Some(ref archive) = self.archive else {
            return Ok(None);
        };

        let key = archive_key(&self.archive_prefix, job_id, meta);
        match archive.download(&key).await? {
            Some(bytes) => {
                use flate2::read::GzDecoder;
                use std::io::{BufRead, BufReader};

                let decoder = GzDecoder::new(&bytes[..]);
                let reader = BufReader::new(decoder);
                let mut result = String::new();

                for line_result in reader.lines() {
                    let line = line_result.context("Failed to read gzip line")?;
                    if Self::line_matches_step(&line, step_name) {
                        result.push_str(&line);
                        result.push('\n');
                    }
                }

                Ok(Some(result))
            }
            None => Ok(None),
        }
    }

    /// Get the log file path as a string (for storing in database).
    pub fn get_log_path(&self, job_id: Uuid) -> String {
        self.log_path(job_id).to_string_lossy().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    fn jsonl_line(step: &str, stream: &str, line: &str) -> String {
        serde_json::json!({
            "ts": "2025-02-12T10:00:00Z",
            "stream": stream,
            "step": step,
            "line": line,
        })
        .to_string()
    }

    fn test_meta() -> JobLogMeta {
        JobLogMeta {
            workspace: "main".to_string(),
            task_name: "deploy".to_string(),
            created_at: chrono::Utc::now(),
        }
    }

    /// In-memory archive backend for testing.
    struct MockArchive {
        store: TokioMutex<HashMap<String, Vec<u8>>>,
    }

    impl MockArchive {
        fn new() -> Self {
            Self {
                store: TokioMutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl LogArchive for MockArchive {
        async fn upload(&self, key: &str, data: &[u8]) -> Result<()> {
            self.store
                .lock()
                .await
                .insert(key.to_string(), data.to_vec());
            Ok(())
        }
        async fn download(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.store.lock().await.get(key).cloned())
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.store.lock().await.remove(key);
            Ok(())
        }
    }

    // ─── archive_key tests ───────────────────────────────────────────────

    #[test]
    fn test_archive_key_format() {
        let job_id = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let meta = JobLogMeta {
            workspace: "production".to_string(),
            task_name: "deploy".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-03-15T14:30:45Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let key = archive_key("logs/", job_id, &meta);
        assert_eq!(
            key,
            "logs/production/deploy/2025/03/15/2025-03-15T14-30-45_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl.gz"
        );
    }

    #[test]
    fn test_archive_key_no_prefix() {
        let job_id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let meta = JobLogMeta {
            workspace: "main".to_string(),
            task_name: "build".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let key = archive_key("", job_id, &meta);
        assert_eq!(
            key,
            "main/build/2025/01/02/2025-01-02T03-04-05_11111111-2222-3333-4444-555555555555.jsonl.gz"
        );
    }

    #[test]
    fn test_archive_key_with_slash_in_task_name() {
        let job_id = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let meta = JobLogMeta {
            workspace: "main".to_string(),
            task_name: "_hook:deploy/notify".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let key = archive_key("logs/", job_id, &meta);
        assert_eq!(
            key,
            "logs/main/_hook:deploy/notify/2025/06/01/2025-06-01T12-00-00_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl.gz"
        );
    }

    // ─── LogStorage tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_append_and_read_log() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());

        let job_id = Uuid::new_v4();

        let line1 = jsonl_line("build", "stdout", "compiling...");
        let line2 = jsonl_line("build", "stdout", "done");

        storage
            .append_log(job_id, &format!("{}\n", line1))
            .await
            .unwrap();
        storage
            .append_log(job_id, &format!("{}\n", line2))
            .await
            .unwrap();

        // Flush before reading so BufWriter contents are on disk.
        storage.close_log(job_id).await;

        let log = storage.get_log(job_id, &test_meta()).await.unwrap();
        assert_eq!(log, format!("{}\n{}\n", line1, line2));
    }

    #[tokio::test]
    async fn test_read_nonexistent_log() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());

        let job_id = Uuid::new_v4();
        let log = storage.get_log(job_id, &test_meta()).await.unwrap();
        assert_eq!(log, "");
    }

    #[tokio::test]
    async fn test_multiple_jobs() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());

        let job1 = Uuid::new_v4();
        let job2 = Uuid::new_v4();

        let l1 = jsonl_line("s1", "stdout", "Job 1 log");
        let l2 = jsonl_line("s1", "stdout", "Job 2 log");

        storage
            .append_log(job1, &format!("{}\n", l1))
            .await
            .unwrap();
        storage
            .append_log(job2, &format!("{}\n", l2))
            .await
            .unwrap();

        storage.close_log(job1).await;
        storage.close_log(job2).await;

        let meta = test_meta();
        let log1 = storage.get_log(job1, &meta).await.unwrap();
        let log2 = storage.get_log(job2, &meta).await.unwrap();

        assert_eq!(log1, format!("{}\n", l1));
        assert_eq!(log2, format!("{}\n", l2));
    }

    #[tokio::test]
    async fn test_get_step_log_filters_by_step_name() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let build1 = jsonl_line("build", "stdout", "compiling...");
        let test1 = jsonl_line("test", "stdout", "running tests...");
        let build2 = jsonl_line("build", "stdout", "done");

        storage
            .append_log(job_id, &format!("{}\n{}\n{}\n", build1, test1, build2))
            .await
            .unwrap();

        storage.close_log(job_id).await;

        let meta = test_meta();
        let build_logs = storage.get_step_log(job_id, "build", &meta).await.unwrap();
        assert_eq!(build_logs, format!("{}\n{}\n", build1, build2));

        let test_logs = storage.get_step_log(job_id, "test", &meta).await.unwrap();
        assert_eq!(test_logs, format!("{}\n", test1));
    }

    #[tokio::test]
    async fn test_get_step_log_no_matches() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let line = jsonl_line("build", "stdout", "compiling...");
        storage
            .append_log(job_id, &format!("{}\n", line))
            .await
            .unwrap();

        storage.close_log(job_id).await;

        let logs = storage
            .get_step_log(job_id, "deploy", &test_meta())
            .await
            .unwrap();
        assert_eq!(logs, "");
    }

    #[tokio::test]
    async fn test_get_step_log_non_json_lines_skipped() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let valid = jsonl_line("build", "stdout", "tagged line");

        storage
            .append_log(
                job_id,
                &format!("plain text line\n{}\nanother plain line\n", valid),
            )
            .await
            .unwrap();

        storage.close_log(job_id).await;

        let logs = storage
            .get_step_log(job_id, "build", &test_meta())
            .await
            .unwrap();
        assert_eq!(logs, format!("{}\n", valid));
    }

    #[tokio::test]
    async fn test_get_step_log_empty_file() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let logs = storage
            .get_step_log(job_id, "build", &test_meta())
            .await
            .unwrap();
        assert_eq!(logs, "");
    }

    #[tokio::test]
    async fn test_auto_create_directory() {
        let temp_dir = TempDir::new().unwrap();
        let log_dir = temp_dir.path().join("logs");

        assert!(!log_dir.exists());

        let storage = LogStorage::new(&log_dir);
        let job_id = Uuid::new_v4();

        storage.append_log(job_id, "test\n").await.unwrap();
        assert!(log_dir.exists());
    }

    #[tokio::test]
    async fn test_legacy_log_file_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        // Write a legacy .log file directly
        let legacy_path = temp_dir.path().join(format!("{}.log", job_id));
        tokio::fs::write(&legacy_path, "legacy line 1\nlegacy line 2\n")
            .await
            .unwrap();

        let log = storage.get_log(job_id, &test_meta()).await.unwrap();
        assert_eq!(log, "legacy line 1\nlegacy line 2\n");
    }

    #[tokio::test]
    async fn test_jsonl_preferred_over_legacy() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        // Write both legacy and JSONL files
        let legacy_path = temp_dir.path().join(format!("{}.log", job_id));
        tokio::fs::write(&legacy_path, "legacy content\n")
            .await
            .unwrap();

        let jsonl_line = jsonl_line("build", "stdout", "new content");
        storage
            .append_log(job_id, &format!("{}\n", jsonl_line))
            .await
            .unwrap();

        storage.close_log(job_id).await;

        let log = storage.get_log(job_id, &test_meta()).await.unwrap();
        assert!(log.contains("new content"));
        assert!(!log.contains("legacy content"));
    }

    #[tokio::test]
    async fn test_get_step_log_filters_stderr() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let stdout_line = jsonl_line("build", "stdout", "compiling...");
        let stderr_line = jsonl_line("build", "stderr", "warning: unused var");
        let other_step = jsonl_line("test", "stdout", "running tests...");

        storage
            .append_log(
                job_id,
                &format!("{}\n{}\n{}\n", stdout_line, stderr_line, other_step),
            )
            .await
            .unwrap();

        storage.close_log(job_id).await;

        // Both stdout and stderr for "build" should be returned
        let build_logs = storage
            .get_step_log(job_id, "build", &test_meta())
            .await
            .unwrap();
        assert_eq!(build_logs, format!("{}\n{}\n", stdout_line, stderr_line));
    }

    #[tokio::test]
    async fn test_upload_to_archive_noop_without_config() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        // Write some logs
        storage
            .append_log(
                job_id,
                &format!("{}\n", jsonl_line("build", "stdout", "hello")),
            )
            .await
            .unwrap();

        storage.close_log(job_id).await;

        // upload_to_archive should be a no-op (no archive configured) and return Ok
        storage
            .upload_to_archive(job_id, &test_meta())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_upload_to_archive_no_local_file() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        // No local file written — upload_to_archive should return Ok (graceful skip)
        storage
            .upload_to_archive(job_id, &test_meta())
            .await
            .unwrap();
    }

    // --- File handle cache tests ---

    #[tokio::test]
    async fn test_file_handle_reused_across_appends() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let chunks = ["alpha\n", "beta\n", "gamma\n", "delta\n"];

        for chunk in &chunks {
            storage.append_log(job_id, chunk).await.unwrap();
        }

        assert!(
            storage.file_cache.contains_key(&job_id),
            "handle should still be cached before close_log"
        );

        storage.close_log(job_id).await;

        assert!(
            !storage.file_cache.contains_key(&job_id),
            "handle should be evicted after close_log"
        );

        let content = tokio::fs::read_to_string(storage.log_path(job_id))
            .await
            .unwrap();
        assert_eq!(content, "alpha\nbeta\ngamma\ndelta\n");
    }

    #[tokio::test]
    async fn test_close_log_flushes_and_evicts() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let line = jsonl_line("step", "stdout", "important data");
        storage
            .append_log(job_id, &format!("{line}\n"))
            .await
            .unwrap();

        // Calling close_log twice must not panic (second call is a no-op).
        storage.close_log(job_id).await;
        storage.close_log(job_id).await;

        assert!(
            !storage.file_cache.contains_key(&job_id),
            "cache entry must be gone after close_log"
        );

        let on_disk = tokio::fs::read_to_string(storage.log_path(job_id))
            .await
            .unwrap();
        assert!(on_disk.contains("important data"));
    }

    #[tokio::test]
    async fn test_concurrent_appends_different_jobs() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(LogStorage::new(temp_dir.path()));

        let job_a = Uuid::new_v4();
        let job_b = Uuid::new_v4();

        let storage_a = Arc::clone(&storage);
        let storage_b = Arc::clone(&storage);

        let handle_a = tokio::spawn(async move {
            for i in 0_u32..20 {
                storage_a
                    .append_log(job_a, &format!("job-a line {i}\n"))
                    .await
                    .unwrap();
            }
        });

        let handle_b = tokio::spawn(async move {
            for i in 0_u32..20 {
                storage_b
                    .append_log(job_b, &format!("job-b line {i}\n"))
                    .await
                    .unwrap();
            }
        });

        handle_a.await.unwrap();
        handle_b.await.unwrap();

        storage.close_log(job_a).await;
        storage.close_log(job_b).await;

        let content_a = tokio::fs::read_to_string(storage.log_path(job_a))
            .await
            .unwrap();
        let content_b = tokio::fs::read_to_string(storage.log_path(job_b))
            .await
            .unwrap();

        assert_eq!(content_a.lines().count(), 20, "job-a should have 20 lines");
        assert_eq!(content_b.lines().count(), 20, "job-b should have 20 lines");

        assert!(
            !content_a.contains("job-b"),
            "job-a must not contain job-b data"
        );
        assert!(
            !content_b.contains("job-a"),
            "job-b must not contain job-a data"
        );

        for i in 0_u32..20 {
            assert!(content_a.contains(&format!("job-a line {i}")));
            assert!(content_b.contains(&format!("job-b line {i}")));
        }
    }

    #[tokio::test]
    async fn test_concurrent_appends_same_job() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(LogStorage::new(temp_dir.path()));
        let job_id = Uuid::new_v4();

        let mut handles = vec![];
        for t in 0..5_u32 {
            let s = Arc::clone(&storage);
            handles.push(tokio::spawn(async move {
                for i in 0..20_u32 {
                    s.append_log(job_id, &format!("t{t}-line{i}\n"))
                        .await
                        .unwrap();
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        storage.close_log(job_id).await;

        let content = tokio::fs::read_to_string(storage.log_path(job_id))
            .await
            .unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 100, "expected 100 lines (5 tasks × 20 lines)");

        for t in 0..5_u32 {
            for i in 0..20_u32 {
                assert!(
                    content.contains(&format!("t{t}-line{i}")),
                    "missing t{t}-line{i}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_delete_local_log_removes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LogStorage::new(dir.path());
        let job_id = Uuid::new_v4();

        storage.ensure_dir().await.unwrap();
        tokio::fs::write(storage.log_path(job_id), b"test log content")
            .await
            .unwrap();
        assert!(storage.log_path(job_id).exists());

        let deleted = storage.delete_local_log(job_id).await;
        assert!(deleted);
        assert!(!storage.log_path(job_id).exists());
    }

    #[tokio::test]
    async fn test_delete_local_log_removes_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LogStorage::new(dir.path());
        let job_id = Uuid::new_v4();

        storage.ensure_dir().await.unwrap();
        tokio::fs::write(storage.legacy_log_path(job_id), b"legacy content")
            .await
            .unwrap();

        let deleted = storage.delete_local_log(job_id).await;
        assert!(deleted);
        assert!(!storage.legacy_log_path(job_id).exists());
    }

    #[tokio::test]
    async fn test_delete_local_log_no_file_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LogStorage::new(dir.path());
        let job_id = Uuid::new_v4();

        let deleted = storage.delete_local_log(job_id).await;
        assert!(!deleted);
    }

    // ─── MockArchive tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_upload_to_archive_with_mock() {
        let temp_dir = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(temp_dir.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn LogArchive>, "".to_string());

        let job_id = Uuid::new_v4();
        let meta = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let content = format!("{}\n", jsonl_line("build", "stdout", "hello"));
        storage.append_log(job_id, &content).await.unwrap();
        storage.close_log(job_id).await;

        storage.upload_to_archive(job_id, &meta).await.unwrap();

        // Verify the archive received compressed data
        let key = archive_key("", job_id, &meta);
        let store = mock.store.lock().await;
        assert!(store.contains_key(&key), "archive should have the key");

        // Decompress and verify
        let compressed = store.get(&key).unwrap();
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).unwrap();
        assert_eq!(decompressed, content);
    }

    #[tokio::test]
    async fn test_archive_read_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(temp_dir.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn LogArchive>, "pfx/".to_string());

        let job_id = Uuid::new_v4();
        let meta = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let content = format!("{}\n", jsonl_line("build", "stdout", "archived"));

        // Write, upload, then delete local
        storage.append_log(job_id, &content).await.unwrap();
        storage.close_log(job_id).await;
        storage.upload_to_archive(job_id, &meta).await.unwrap();

        // Delete local file
        let local_path = storage.log_path(job_id);
        tokio::fs::remove_file(&local_path).await.unwrap();
        assert!(!local_path.exists());

        // get_log should fall back to archive
        let log = storage.get_log(job_id, &meta).await.unwrap();
        assert_eq!(log, content);
    }

    #[tokio::test]
    async fn test_archive_step_log_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(temp_dir.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn LogArchive>, "".to_string());

        let job_id = Uuid::new_v4();
        let meta = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let build_line = jsonl_line("build", "stdout", "compiling...");
        let test_line = jsonl_line("test", "stdout", "testing...");
        let content = format!("{}\n{}\n", build_line, test_line);

        storage.append_log(job_id, &content).await.unwrap();
        storage.close_log(job_id).await;
        storage.upload_to_archive(job_id, &meta).await.unwrap();

        // Delete local
        tokio::fs::remove_file(storage.log_path(job_id))
            .await
            .unwrap();

        // Step log should filter from archived data
        let build_logs = storage.get_step_log(job_id, "build", &meta).await.unwrap();
        assert_eq!(build_logs, format!("{}\n", build_line));

        let test_logs = storage.get_step_log(job_id, "test", &meta).await.unwrap();
        assert_eq!(test_logs, format!("{}\n", test_line));
    }

    #[tokio::test]
    async fn test_delete_archive_log() {
        let temp_dir = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(temp_dir.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn LogArchive>, "".to_string());

        let job_id = Uuid::new_v4();
        let meta = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        // Upload then delete
        storage
            .append_log(job_id, &format!("{}\n", jsonl_line("s", "stdout", "x")))
            .await
            .unwrap();
        storage.close_log(job_id).await;
        storage.upload_to_archive(job_id, &meta).await.unwrap();

        let key = archive_key("", job_id, &meta);
        assert!(mock.store.lock().await.contains_key(&key));

        storage.delete_archive_log(job_id, &meta).await.unwrap();
        assert!(!mock.store.lock().await.contains_key(&key));
    }

    #[tokio::test]
    async fn test_delete_archive_log_noop_without_config() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        // No archive configured — should be a no-op
        storage
            .delete_archive_log(job_id, &test_meta())
            .await
            .unwrap();
    }

    // ─── LocalArchive tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_local_archive_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let archive = LocalArchive::new(dir.path());

        let data = b"hello world compressed";
        archive
            .upload("ws/task/2025/01/01/file.jsonl.gz", data)
            .await
            .unwrap();

        let downloaded = archive
            .download("ws/task/2025/01/01/file.jsonl.gz")
            .await
            .unwrap();
        assert_eq!(downloaded.unwrap(), data);
    }

    #[tokio::test]
    async fn test_local_archive_download_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let archive = LocalArchive::new(dir.path());

        let result = archive.download("nonexistent/key").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_local_archive_delete() {
        let dir = tempfile::tempdir().unwrap();
        let archive = LocalArchive::new(dir.path());

        archive.upload("key.gz", b"data").await.unwrap();
        assert!(archive.download("key.gz").await.unwrap().is_some());

        archive.delete("key.gz").await.unwrap();
        assert!(archive.download("key.gz").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_local_archive_delete_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let archive = LocalArchive::new(dir.path());

        // Should not error
        archive.delete("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn test_local_archive_creates_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let archive = LocalArchive::new(dir.path());

        archive.upload("a/b/c/d/file.gz", b"nested").await.unwrap();

        let result = archive.download("a/b/c/d/file.gz").await.unwrap();
        assert_eq!(result.unwrap(), b"nested");
    }

    // ─── LogStorageConfig::effective_archive tests ───────────────────────

    #[test]
    fn test_effective_archive_prefers_archive_over_s3() {
        use crate::config::{ArchiveConfig, LogStorageConfig, S3Config};
        let config = LogStorageConfig {
            local_dir: "/tmp/logs".to_string(),
            s3: Some(S3Config {
                bucket: "legacy-bucket".to_string(),
                region: "us-east-1".to_string(),
                prefix: "old/".to_string(),
                endpoint: None,
            }),
            archive: Some(ArchiveConfig {
                archive_type: "local".to_string(),
                bucket: None,
                region: None,
                endpoint: None,
                path: Some("/mnt/archive".to_string()),
                prefix: "new/".to_string(),
            }),
        };
        let effective = config.effective_archive().unwrap();
        assert_eq!(effective.archive_type, "local");
        assert_eq!(effective.prefix, "new/");
    }

    #[test]
    fn test_effective_archive_returns_none_when_neither_set() {
        use crate::config::LogStorageConfig;
        let config = LogStorageConfig {
            local_dir: "/tmp/logs".to_string(),
            s3: None,
            archive: None,
        };
        assert!(config.effective_archive().is_none());
    }

    // ─── Corrupt archive data ────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_log_returns_error_on_corrupt_archive_data() {
        let temp_dir = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(temp_dir.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn LogArchive>, "".to_string());

        let job_id = Uuid::new_v4();
        let meta = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        // Insert non-gzip bytes directly into the mock archive
        let key = archive_key("", job_id, &meta);
        mock.store
            .lock()
            .await
            .insert(key, b"this is not gzip data".to_vec());

        // get_log should return an error (not an empty string)
        let result = storage.get_log(job_id, &meta).await;
        assert!(result.is_err(), "corrupt gzip data should produce an error");
    }

    // ─── LocalArchive end-to-end wired into LogStorage ───────────────────

    #[tokio::test]
    async fn test_local_archive_end_to_end_with_log_storage() {
        let live_dir = tempfile::tempdir().unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        let archive = Arc::new(LocalArchive::new(archive_dir.path()));
        let storage = LogStorage::new(live_dir.path())
            .with_archive(archive as Arc<dyn LogArchive>, "".to_string());

        let job_id = Uuid::new_v4();
        let meta = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let content = format!(
            "{}\n",
            jsonl_line("build", "stdout", "hello from local archive")
        );

        // Write, close, archive
        storage.append_log(job_id, &content).await.unwrap();
        storage.close_log(job_id).await;
        storage.upload_to_archive(job_id, &meta).await.unwrap();

        // Delete local file
        storage.delete_local_log(job_id).await;
        assert!(!storage.log_path(job_id).exists());

        // Read should fall back to local archive
        let log = storage.get_log(job_id, &meta).await.unwrap();
        assert_eq!(log, content);

        // Step log should also work from archive
        let step_log = storage.get_step_log(job_id, "build", &meta).await.unwrap();
        assert_eq!(step_log, content);
    }

    // ─── S3Archive::from_config validation ───────────────────────────────

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn test_s3_archive_from_config_missing_region() {
        use super::S3Archive;
        use crate::config::ArchiveConfig;
        let config = ArchiveConfig {
            archive_type: "s3".to_string(),
            bucket: Some("my-bucket".to_string()),
            region: None,
            endpoint: None,
            path: None,
            prefix: "".to_string(),
        };
        let result = S3Archive::from_config(&config).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.err().expect("expected Err from from_config"));
        assert!(
            err_msg.contains("region"),
            "error should mention region: {}",
            err_msg
        );
    }

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn test_s3_archive_from_config_missing_bucket() {
        use super::S3Archive;
        use crate::config::ArchiveConfig;
        let config = ArchiveConfig {
            archive_type: "s3".to_string(),
            bucket: None,
            region: Some("us-east-1".to_string()),
            endpoint: None,
            path: None,
            prefix: "".to_string(),
        };
        let result = S3Archive::from_config(&config).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.err().expect("expected Err from from_config"));
        assert!(
            err_msg.contains("bucket"),
            "error should mention bucket: {}",
            err_msg
        );
    }

    // ─── line_matches_step substring false-positive guard ────────────────

    #[tokio::test]
    async fn test_step_log_no_substring_false_positive() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let build_line = jsonl_line("build", "stdout", "compiling...");
        let build_notify_line =
            jsonl_line("build-notify", "stdout", "sending notification about build");

        storage
            .append_log(job_id, &format!("{}\n{}\n", build_line, build_notify_line))
            .await
            .unwrap();
        storage.close_log(job_id).await;

        // "build" must NOT match "build-notify" lines
        let build_logs = storage
            .get_step_log(job_id, "build", &test_meta())
            .await
            .unwrap();
        assert_eq!(build_logs, format!("{}\n", build_line));

        // "build-notify" must NOT match "build" lines
        let notify_logs = storage
            .get_step_log(job_id, "build-notify", &test_meta())
            .await
            .unwrap();
        assert_eq!(notify_logs, format!("{}\n", build_notify_line));
    }

    // ─── archive_key uniqueness ───────────────────────────────────────────

    #[test]
    fn test_archive_key_different_dates_produce_distinct_keys() {
        let job_id = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let meta1 = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let meta2 = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-15T12:30:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let key1 = archive_key("", job_id, &meta1);
        let key2 = archive_key("", job_id, &meta2);
        assert_ne!(
            key1, key2,
            "same job with different timestamps must produce different keys"
        );
    }
}
