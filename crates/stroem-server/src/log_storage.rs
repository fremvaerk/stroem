use anyhow::{Context, Result};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use uuid::Uuid;

#[cfg(feature = "s3")]
use crate::config::S3Config;

#[cfg(feature = "s3")]
#[derive(Clone)]
struct S3Backend {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

/// Metadata needed to construct structured S3 keys for job logs.
pub struct JobLogMeta {
    pub workspace: String,
    pub task_name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A cached, buffered file handle for a single job's log file.
type CachedHandle = Arc<Mutex<BufWriter<File>>>;

/// Log storage handles writing and reading job logs.
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
    #[cfg(feature = "s3")]
    s3: Option<S3Backend>,
}

impl LogStorage {
    /// Create a new log storage backed by `base_dir`.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            dir_created: Arc::new(AtomicBool::new(false)),
            file_cache: Arc::new(DashMap::new()),
            #[cfg(feature = "s3")]
            s3: None,
        }
    }

    /// Configure S3 backend for log archival.
    #[cfg(feature = "s3")]
    pub async fn with_s3(mut self, s3_config: &S3Config) -> Result<Self> {
        let mut aws_config_builder = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(s3_config.region.clone()));

        if let Some(ref endpoint) = s3_config.endpoint {
            aws_config_builder = aws_config_builder.endpoint_url(endpoint);
        }

        let aws_config = aws_config_builder.load().await;
        let s3_sdk_config = aws_sdk_s3::config::Builder::from(&aws_config)
            .force_path_style(s3_config.endpoint.is_some())
            .build();
        let client = aws_sdk_s3::Client::from_conf(s3_sdk_config);

        self.s3 = Some(S3Backend {
            client,
            bucket: s3_config.bucket.clone(),
            prefix: s3_config.prefix.clone(),
        });

        tracing::info!(
            "S3 log archival enabled: bucket={}, prefix={}",
            s3_config.bucket,
            s3_config.prefix
        );

        Ok(self)
    }

    /// Configure S3 backend with a pre-built client (for testing).
    #[cfg(feature = "s3")]
    pub fn with_s3_client(
        mut self,
        client: aws_sdk_s3::Client,
        bucket: String,
        prefix: String,
    ) -> Self {
        self.s3 = Some(S3Backend {
            client,
            bucket,
            prefix,
        });
        self
    }

    /// Build a structured S3 key from job metadata.
    ///
    /// Format: `{prefix}{workspace}/{task}/YYYY/MM/DD/YYYY-MM-DDTHH-MM-SS_{job_id}.jsonl.gz`
    #[cfg(feature = "s3")]
    fn s3_key(&self, job_id: Uuid, meta: &JobLogMeta) -> String {
        let prefix = self.s3.as_ref().map(|s| s.prefix.as_str()).unwrap_or("");
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

    /// Upload a job's log file to S3 (gzip-compressed). No-op if S3 is not configured.
    #[allow(unused_variables)]
    pub async fn upload_to_s3(&self, job_id: Uuid, meta: &JobLogMeta) -> Result<()> {
        #[cfg(feature = "s3")]
        if let Some(ref s3) = self.s3 {
            let path = self.log_path(job_id);
            if !path.exists() {
                tracing::debug!("No local log file for job {}, skipping S3 upload", job_id);
                return Ok(());
            }

            let raw = fs::read(&path)
                .await
                .with_context(|| format!("Failed to read log file for S3 upload: {:?}", path))?;

            // Gzip compress
            use flate2::write::GzEncoder;
            use flate2::Compression;
            use std::io::Write;

            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(&raw)
                .context("Failed to gzip-compress log data")?;
            let compressed = encoder
                .finish()
                .context("Failed to finish gzip compression")?;

            let key = self.s3_key(job_id, meta);
            s3.client
                .put_object()
                .bucket(&s3.bucket)
                .key(&key)
                .content_type("application/gzip")
                .body(compressed.into())
                .send()
                .await
                .with_context(|| format!("Failed to upload log to S3: {}", key))?;

            tracing::info!("Uploaded logs to S3: s3://{}/{}", s3.bucket, key);
            return Ok(());
        }

        Ok(())
    }

    /// Download a job's log from S3 (gzip-compressed). Returns `None` if the key doesn't exist.
    #[cfg(feature = "s3")]
    async fn get_log_from_s3(&self, job_id: Uuid, meta: &JobLogMeta) -> Result<Option<String>> {
        if let Some(ref s3) = self.s3 {
            let key = self.s3_key(job_id, meta);
            match s3
                .client
                .get_object()
                .bucket(&s3.bucket)
                .key(&key)
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

                    // Gzip decompress
                    use flate2::read::GzDecoder;
                    use std::io::Read;

                    let mut decoder = GzDecoder::new(&bytes[..]);
                    let mut content = String::new();
                    decoder
                        .read_to_string(&mut content)
                        .context("Failed to gzip-decompress S3 log content")?;

                    Ok(Some(content))
                }
                Err(sdk_err) => {
                    if let aws_sdk_s3::error::SdkError::ServiceError(ref service_err) = sdk_err {
                        if service_err.err().is_no_such_key() {
                            return Ok(None);
                        }
                    }
                    Err(anyhow::anyhow!("Failed to get log from S3: {}", sdk_err))
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Get the full log contents for a job.
    ///
    /// Checks for `.jsonl` first, falls back to legacy `.log` file, then S3.
    #[allow(unused_variables)]
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

        // Fallback to S3
        #[cfg(feature = "s3")]
        if let Some(content) = self.get_log_from_s3(job_id, meta).await? {
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
    /// memory. Falls back to S3 when no local file is found.
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

        #[cfg(feature = "s3")]
        if let Some(result) = self.get_step_log_from_s3(job_id, step_name, meta).await? {
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

    /// Stream-decompress an S3 gzip object and return only lines matching `step_name`.
    #[cfg(feature = "s3")]
    async fn get_step_log_from_s3(
        &self,
        job_id: Uuid,
        step_name: &str,
        meta: &JobLogMeta,
    ) -> Result<Option<String>> {
        let Some(ref s3) = self.s3 else {
            return Ok(None);
        };

        let key = self.s3_key(job_id, meta);
        match s3
            .client
            .get_object()
            .bucket(&s3.bucket)
            .key(&key)
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
            Err(sdk_err) => {
                if let aws_sdk_s3::error::SdkError::ServiceError(ref service_err) = sdk_err {
                    if service_err.err().is_no_such_key() {
                        return Ok(None);
                    }
                }
                Err(anyhow::anyhow!(
                    "Failed to get step log from S3: {}",
                    sdk_err
                ))
            }
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
    use tempfile::TempDir;

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
    async fn test_upload_to_s3_noop_without_config() {
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

        // upload_to_s3 should be a no-op (no S3 configured) and return Ok
        storage.upload_to_s3(job_id, &test_meta()).await.unwrap();
    }

    #[tokio::test]
    async fn test_upload_to_s3_no_local_file() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        // No local file written — upload_to_s3 should return Ok (graceful skip)
        storage.upload_to_s3(job_id, &test_meta()).await.unwrap();
    }

    // --- New tests for the file handle cache ---

    /// Verify that appending multiple chunks to the same job produces the
    /// correct concatenated content (i.e. the cached handle is reused and no
    /// data is lost between calls).
    #[tokio::test]
    async fn test_file_handle_reused_across_appends() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let chunks = ["alpha\n", "beta\n", "gamma\n", "delta\n"];

        for chunk in &chunks {
            storage.append_log(job_id, chunk).await.unwrap();
        }

        // Confirm the cache contains exactly one entry for this job.
        assert!(
            storage.file_cache.contains_key(&job_id),
            "handle should still be cached before close_log"
        );

        storage.close_log(job_id).await;

        // The handle must be gone after close.
        assert!(
            !storage.file_cache.contains_key(&job_id),
            "handle should be evicted after close_log"
        );

        // Read back and verify all chunks are present in order.
        let content = tokio::fs::read_to_string(storage.log_path(job_id))
            .await
            .unwrap();
        assert_eq!(content, "alpha\nbeta\ngamma\ndelta\n");
    }

    /// Verify that `close_log` flushes buffered data to disk and removes the
    /// entry from the cache.
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

        // The file must be readable and contain the expected content.
        let on_disk = tokio::fs::read_to_string(storage.log_path(job_id))
            .await
            .unwrap();
        assert!(on_disk.contains("important data"));
    }

    /// Verify that concurrent appends to *different* jobs are independent and
    /// produce correct content for each job.
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

        // Each job should have exactly 20 lines.
        assert_eq!(content_a.lines().count(), 20, "job-a should have 20 lines");
        assert_eq!(content_b.lines().count(), 20, "job-b should have 20 lines");

        // No cross-contamination.
        assert!(
            !content_a.contains("job-b"),
            "job-a must not contain job-b data"
        );
        assert!(
            !content_b.contains("job-a"),
            "job-b must not contain job-a data"
        );

        // All lines for each job are present.
        for i in 0_u32..20 {
            assert!(content_a.contains(&format!("job-a line {i}")));
            assert!(content_b.contains(&format!("job-b line {i}")));
        }
    }

    #[cfg(feature = "s3")]
    #[test]
    fn test_s3_key_format() {
        let storage = LogStorage::new("/tmp/unused");
        // Give it a fake S3 backend so prefix is used
        let storage = storage.with_s3_client(
            {
                let creds = aws_sdk_s3::config::Credentials::new("x", "x", None, None, "t");
                let config = aws_sdk_s3::Config::builder()
                    .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                    .region(aws_sdk_s3::config::Region::new("us-east-1"))
                    .credentials_provider(creds)
                    .build();
                aws_sdk_s3::Client::from_conf(config)
            },
            "bucket".to_string(),
            "logs/".to_string(),
        );

        let job_id = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let meta = JobLogMeta {
            workspace: "production".to_string(),
            task_name: "deploy".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-03-15T14:30:45Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let key = storage.s3_key(job_id, &meta);
        assert_eq!(
            key,
            "logs/production/deploy/2025/03/15/2025-03-15T14-30-45_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl.gz"
        );
    }

    #[cfg(feature = "s3")]
    #[test]
    fn test_s3_key_no_prefix() {
        let storage = LogStorage::new("/tmp/unused");
        let storage = storage.with_s3_client(
            {
                let creds = aws_sdk_s3::config::Credentials::new("x", "x", None, None, "t");
                let config = aws_sdk_s3::Config::builder()
                    .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                    .region(aws_sdk_s3::config::Region::new("us-east-1"))
                    .credentials_provider(creds)
                    .build();
                aws_sdk_s3::Client::from_conf(config)
            },
            "bucket".to_string(),
            "".to_string(),
        );

        let job_id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let meta = JobLogMeta {
            workspace: "main".to_string(),
            task_name: "build".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let key = storage.s3_key(job_id, &meta);
        assert_eq!(
            key,
            "main/build/2025/01/02/2025-01-02T03-04-05_11111111-2222-3333-4444-555555555555.jsonl.gz"
        );
    }

    /// Verify that multiple async tasks appending to the *same* job concurrently
    /// all succeed and that no lines are lost or corrupted.
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

        // Verify every line from every task is present.
        for t in 0..5_u32 {
            for i in 0..20_u32 {
                assert!(
                    content.contains(&format!("t{t}-line{i}")),
                    "missing t{t}-line{i}"
                );
            }
        }
    }

    #[cfg(feature = "s3")]
    #[test]
    fn test_s3_key_with_slash_in_task_name() {
        let storage = LogStorage::new("/tmp/unused");
        let storage = storage.with_s3_client(
            {
                let creds = aws_sdk_s3::config::Credentials::new("x", "x", None, None, "t");
                let config = aws_sdk_s3::Config::builder()
                    .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                    .region(aws_sdk_s3::config::Region::new("us-east-1"))
                    .credentials_provider(creds)
                    .build();
                aws_sdk_s3::Client::from_conf(config)
            },
            "bucket".to_string(),
            "logs/".to_string(),
        );

        let job_id = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        // Hook task names contain colons and may reference actions with slashes
        let meta = JobLogMeta {
            workspace: "main".to_string(),
            task_name: "_hook:deploy/notify".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let key = storage.s3_key(job_id, &meta);
        // Slashes in task name create extra path segments — this is fine for S3
        assert_eq!(
            key,
            "logs/main/_hook:deploy/notify/2025/06/01/2025-06-01T12-00-00_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl.gz"
        );
    }
}
