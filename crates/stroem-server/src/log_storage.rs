use crate::blob_storage::BlobArchive;
use anyhow::{Context, Result};
use bytes::Bytes;
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

/// Merge two JSONL log strings into the union of their unique lines, sorted
/// by the `"ts"` field embedded in each line. Blank lines are dropped.
/// Lines whose ts cannot be extracted sort to the start (preserving partial
/// data is better than dropping it).
///
/// This is the recovery primitive for HA mirror gaps: each replica may hold
/// only the subset of chunks worker pushes happened to route there, plus
/// whatever survived NOTIFY mirroring. The post-terminal archive — uploaded
/// by whichever replica orchestrated the final step — captures THAT
/// replica's local file at terminal time. Merging the two recovers the
/// union (provided no individual chunk was lost from both).
pub(crate) fn merge_jsonl_logs(a: &str, b: &str) -> String {
    use std::collections::HashSet;

    // Cheap allocation hint without a double-scan of inputs: average jsonl
    // line is ~150 bytes, so estimate line count from total bytes.
    let estimated_lines = (a.len() + b.len()) / 80 + 1;
    let mut seen = HashSet::with_capacity(estimated_lines);
    let mut lines: Vec<&str> = Vec::with_capacity(estimated_lines);
    for line in a.lines().chain(b.lines()) {
        if line.is_empty() {
            continue;
        }
        if seen.insert(line) {
            lines.push(line);
        }
    }

    lines.sort_by(|a, b| extract_ts(a).cmp(&extract_ts(b)));

    let mut out = String::with_capacity(a.len() + b.len());
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Extract the value of the top-level `"ts"` field from a JSONL line. Uses
/// the raw bytes to avoid full JSON parsing in the hot path of merging
/// thousands of lines.
fn extract_ts(line: &str) -> Option<&str> {
    const TS_KEY: &str = "\"ts\":\"";
    let start = line.find(TS_KEY)? + TS_KEY.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
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
    archive: Option<Arc<dyn BlobArchive>>,
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
    pub fn with_archive(mut self, archive: Arc<dyn BlobArchive>, prefix: String) -> Self {
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
            archive
                .put(&key, "application/gzip", Bytes::from(compressed))
                .await?;
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
            if let Some(blob) = archive.get(&key).await? {
                use flate2::read::GzDecoder;
                use std::io::Read;

                let mut decoder = GzDecoder::new(blob.bytes.as_ref());
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
    /// Strategy:
    /// * If `is_terminal` is `true` AND an archive is configured, fetch the
    ///   archive blob in addition to the local file and return their
    ///   line-level union sorted by timestamp. This recovers chunks that were
    ///   lost in HA cross-replica mirroring: each replica's local file may
    ///   only hold the subset of chunks worker calls happened to route there,
    ///   so for terminal jobs the union of (local on this replica) + (archive
    ///   uploaded by the orchestrating replica) is the most complete view.
    /// * If the archive read errors transiently (e.g. S3 5xx), fall through
    ///   to local-only rather than 500ing the caller — the local file alone
    ///   is strictly better than nothing.
    /// * Otherwise: local `.jsonl` → legacy `.log`, returning the first that
    ///   exists. Archive is consulted only for terminal jobs because the
    ///   upload happens at terminal time; pre-terminal archive reads are
    ///   guaranteed 404s.
    pub async fn get_log(
        &self,
        job_id: Uuid,
        meta: &JobLogMeta,
        is_terminal: bool,
    ) -> Result<String> {
        let local_content = self.read_local_log(job_id).await?;

        // Single archive read regardless of which downstream branch consumes
        // it — eliminates the previous double-fetch on the "terminal + key
        // missing + local missing" path. Archive errors degrade to None +
        // warning rather than propagating so the local-only fallback works
        // when S3 has a transient blip.
        let archive_content = if is_terminal && self.archive.is_some() {
            match self.get_log_from_archive(job_id, meta).await {
                Ok(opt) => opt,
                Err(e) => {
                    tracing::warn!(
                        "archive read failed for terminal job {}, returning local-only: {:#}",
                        job_id,
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        match (local_content, archive_content) {
            (local, Some(archive)) => {
                Ok(merge_jsonl_logs(local.as_deref().unwrap_or(""), &archive))
            }
            (Some(local), None) => Ok(local),
            (None, None) => Ok(String::new()),
        }
    }

    /// Read the local `.jsonl` (preferred) or legacy `.log` if either exists.
    /// Returns `None` when neither file is present. Uses error-kind detection
    /// rather than `path.exists()` so a TOCTOU race with `delete_local_log`
    /// (which can fire during retention sweeps mid-request) doesn't surface
    /// as a 500 — a vanished file becomes `Ok(None)` and the caller falls
    /// through to the archive.
    async fn read_local_log(&self, job_id: Uuid) -> Result<Option<String>> {
        if let Some(content) = Self::read_file_if_present(&self.log_path(job_id)).await? {
            return Ok(Some(content));
        }
        Self::read_file_if_present(&self.legacy_log_path(job_id)).await
    }

    /// `Some(content)` if the file exists and is readable, `None` if it does
    /// not exist (the only error kind translated to None). All other I/O
    /// errors — permission denied, mid-read I/O failure, invalid UTF-8 —
    /// propagate.
    async fn read_file_if_present(path: &Path) -> Result<Option<String>> {
        let mut file = match fs::File::open(path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to open log file: {path:?}"));
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .context("Failed to read log file")?;

        Ok(Some(contents))
    }

    /// Get log lines for a specific step within a job.
    ///
    /// Strategy mirrors [`Self::get_log`]: for terminal jobs with an archive
    /// configured, merge the step-filtered local and archive views to recover
    /// chunks lost in HA mirroring. Archive errors degrade to local-only
    /// rather than 500ing the caller. Pre-terminal jobs never touch the
    /// archive (no upload exists yet).
    pub async fn get_step_log(
        &self,
        job_id: Uuid,
        step_name: &str,
        meta: &JobLogMeta,
        is_terminal: bool,
    ) -> Result<String> {
        let local = self.read_local_step_log(job_id, step_name).await?;

        let archive_content = if is_terminal && self.archive.is_some() {
            match self
                .get_step_log_from_archive(job_id, step_name, meta)
                .await
            {
                Ok(opt) => opt,
                Err(e) => {
                    tracing::warn!(
                        "archive read failed for terminal job {} step '{}', returning local-only: {:#}",
                        job_id,
                        step_name,
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        match (local, archive_content) {
            (local, Some(archive)) => {
                Ok(merge_jsonl_logs(local.as_deref().unwrap_or(""), &archive))
            }
            (Some(local), None) => Ok(local),
            (None, None) => Ok(String::new()),
        }
    }

    /// Filter local `.jsonl` (preferred) or legacy `.log` for a step's lines.
    /// Returns `None` when neither file is present. Same TOCTOU-safe error
    /// handling as `read_local_log`.
    async fn read_local_step_log(&self, job_id: Uuid, step_name: &str) -> Result<Option<String>> {
        let path = self.log_path(job_id);
        if let Some(c) = self
            .filter_step_from_file_if_present(&path, step_name)
            .await?
        {
            return Ok(Some(c));
        }
        let legacy_path = self.legacy_log_path(job_id);
        self.filter_step_from_file_if_present(&legacy_path, step_name)
            .await
    }

    async fn filter_step_from_file_if_present(
        &self,
        path: &Path,
        step_name: &str,
    ) -> Result<Option<String>> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let file = match fs::File::open(path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to open log file: {path:?}"));
            }
        };

        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut result = String::new();

        while let Some(line) = lines.next_line().await.context("Failed to read log line")? {
            if Self::line_matches_step(&line, step_name) {
                result.push_str(&line);
                result.push('\n');
            }
        }

        Ok(Some(result))
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
        match archive.get(&key).await? {
            Some(blob) => {
                use flate2::read::GzDecoder;
                use std::io::{BufRead, BufReader};

                let decoder = GzDecoder::new(blob.bytes.as_ref());
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
        store: TokioMutex<HashMap<String, (String, Vec<u8>)>>,
        get_calls: std::sync::atomic::AtomicUsize,
    }

    impl MockArchive {
        fn new() -> Self {
            Self {
                store: TokioMutex::new(HashMap::new()),
                get_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        /// How many times `get()` has been called on this mock — used to
        /// assert the single-fetch behavior of the merge path.
        async fn get_call_count(&self) -> usize {
            self.get_calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl BlobArchive for MockArchive {
        async fn put(&self, key: &str, content_type: &str, data: Bytes) -> Result<()> {
            self.store
                .lock()
                .await
                .insert(key.to_string(), (content_type.to_string(), data.to_vec()));
            Ok(())
        }
        async fn get(&self, key: &str) -> Result<Option<crate::blob_storage::Blob>> {
            self.get_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self
                .store
                .lock()
                .await
                .get(key)
                .map(|(ct, data)| crate::blob_storage::Blob {
                    content_type: ct.clone(),
                    bytes: Bytes::from(data.clone()),
                }))
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.store.lock().await.remove(key);
            Ok(())
        }
        async fn delete_prefix(&self, prefix: &str) -> Result<()> {
            let mut store = self.store.lock().await;
            store.retain(|k, _| !k.starts_with(prefix));
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

        let log = storage.get_log(job_id, &test_meta(), false).await.unwrap();
        assert_eq!(log, format!("{}\n{}\n", line1, line2));
    }

    #[tokio::test]
    async fn test_read_nonexistent_log() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());

        let job_id = Uuid::new_v4();
        let log = storage.get_log(job_id, &test_meta(), false).await.unwrap();
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
        let log1 = storage.get_log(job1, &meta, false).await.unwrap();
        let log2 = storage.get_log(job2, &meta, false).await.unwrap();

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
        let build_logs = storage
            .get_step_log(job_id, "build", &meta, false)
            .await
            .unwrap();
        assert_eq!(build_logs, format!("{}\n{}\n", build1, build2));

        let test_logs = storage
            .get_step_log(job_id, "test", &meta, false)
            .await
            .unwrap();
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
            .get_step_log(job_id, "deploy", &test_meta(), false)
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
            .get_step_log(job_id, "build", &test_meta(), false)
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
            .get_step_log(job_id, "build", &test_meta(), false)
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

        let log = storage.get_log(job_id, &test_meta(), false).await.unwrap();
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

        let log = storage.get_log(job_id, &test_meta(), false).await.unwrap();
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
            .get_step_log(job_id, "build", &test_meta(), false)
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
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());

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
        let (ct, compressed) = store.get(&key).unwrap();
        assert_eq!(ct, "application/gzip");
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
        let storage = LogStorage::new(temp_dir.path()).with_archive(
            Arc::clone(&mock) as Arc<dyn BlobArchive>,
            "pfx/".to_string(),
        );

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

        // get_log should fall back to archive. Pass is_terminal=true because
        // archive is only consulted for terminal jobs (pre-terminal archive
        // reads are guaranteed 404s — the upload happens at terminal time).
        let log = storage.get_log(job_id, &meta, true).await.unwrap();
        assert_eq!(log, content);
    }

    #[tokio::test]
    async fn test_archive_step_log_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(temp_dir.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());

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

        // Step log should filter from archived data. is_terminal=true
        // because archive is gated to terminal jobs (see test_archive_read_fallback).
        let build_logs = storage
            .get_step_log(job_id, "build", &meta, true)
            .await
            .unwrap();
        assert_eq!(build_logs, format!("{}\n", build_line));

        let test_logs = storage
            .get_step_log(job_id, "test", &meta, true)
            .await
            .unwrap();
        assert_eq!(test_logs, format!("{}\n", test_line));
    }

    #[tokio::test]
    async fn test_delete_archive_log() {
        let temp_dir = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(temp_dir.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());

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
    async fn test_get_log_corrupt_archive_degrades_to_local_only() {
        // Updated contract (was: "returns Err on corrupt archive"): a
        // corrupt archive blob should NOT 500 the caller — the local file
        // is intact and serving it is strictly better than an error. The
        // archive failure is logged via tracing::warn! instead.
        let temp_dir = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(temp_dir.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());

        let job_id = Uuid::new_v4();
        let meta = JobLogMeta {
            workspace: "ws".to_string(),
            task_name: "task".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2025-06-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        // Write something locally so we can verify it survives the
        // corrupt-archive failure.
        let local = format!("{}\n", jsonl_line("build", "stdout", "local-bytes"));
        storage.append_log(job_id, &local).await.unwrap();

        // Insert non-gzip bytes directly into the mock archive
        let key = archive_key("", job_id, &meta);
        mock.store.lock().await.insert(
            key,
            (
                "application/gzip".to_string(),
                b"this is not gzip data".to_vec(),
            ),
        );

        // is_terminal=true to exercise the merge path. Corrupt archive
        // must not propagate — local content is returned instead.
        let result = storage
            .get_log(job_id, &meta, true)
            .await
            .expect("corrupt archive must degrade to local-only, not error");
        assert!(result.contains("local-bytes"));
    }

    // ─── LocalBlobArchive end-to-end wired into LogStorage ──────────────

    #[tokio::test]
    async fn test_local_archive_end_to_end_with_log_storage() {
        use crate::blob_storage::LocalBlobArchive;
        let live_dir = tempfile::tempdir().unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        let archive = Arc::new(LocalBlobArchive::new(archive_dir.path().to_path_buf()));
        let storage = LogStorage::new(live_dir.path())
            .with_archive(archive as Arc<dyn BlobArchive>, "".to_string());

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

        // Read should fall back to local archive. is_terminal=true because
        // archive is only consulted for terminal jobs.
        let log = storage.get_log(job_id, &meta, true).await.unwrap();
        assert_eq!(log, content);

        // Step log should also work from archive
        let step_log = storage
            .get_step_log(job_id, "build", &meta, true)
            .await
            .unwrap();
        assert_eq!(step_log, content);
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
            .get_step_log(job_id, "build", &test_meta(), false)
            .await
            .unwrap();
        assert_eq!(build_logs, format!("{}\n", build_line));

        // "build-notify" must NOT match "build" lines
        let notify_logs = storage
            .get_step_log(job_id, "build-notify", &test_meta(), false)
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

    // ─── merge_jsonl_logs: HA-gap recovery ────────────────────────────────

    fn ts_line(ts: &str, step: &str, msg: &str) -> String {
        format!(r#"{{"ts":"{ts}","stream":"stdout","step":"{step}","line":"{msg}"}}"#)
    }

    #[test]
    fn merge_jsonl_logs_unions_disjoint_inputs_sorted_by_ts() {
        // The production scenario: replica A's local file has steps a/b,
        // replica B's archive has steps c/d. Both are time-interleaved.
        let a = format!(
            "{}\n{}\n",
            ts_line("2026-06-10T05:00:01Z", "build[0]", "a1"),
            ts_line("2026-06-10T05:00:05Z", "build[0]", "a2"),
        );
        let b = format!(
            "{}\n{}\n",
            ts_line("2026-06-10T05:00:02Z", "build[1]", "b1"),
            ts_line("2026-06-10T05:00:03Z", "build[1]", "b2"),
        );

        let merged = merge_jsonl_logs(&a, &b);
        let lines: Vec<&str> = merged.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "expected 4 unique lines, got {}",
            lines.len()
        );
        assert!(lines[0].contains(r#""ts":"2026-06-10T05:00:01Z""#));
        assert!(lines[1].contains(r#""ts":"2026-06-10T05:00:02Z""#));
        assert!(lines[2].contains(r#""ts":"2026-06-10T05:00:03Z""#));
        assert!(lines[3].contains(r#""ts":"2026-06-10T05:00:05Z""#));
    }

    #[test]
    fn merge_jsonl_logs_dedupes_overlapping_chunks() {
        // Lines that survived NOTIFY mirror to BOTH replicas appear in both
        // local and archive. The merge must dedupe by exact line content.
        let shared = ts_line("2026-06-10T05:00:01Z", "build", "shared");
        let a = format!(
            "{}\n{}\n",
            shared,
            ts_line("2026-06-10T05:00:02Z", "build", "only-a")
        );
        let b = format!(
            "{}\n{}\n",
            shared,
            ts_line("2026-06-10T05:00:03Z", "build", "only-b")
        );

        let merged = merge_jsonl_logs(&a, &b);
        let count = merged.matches("shared").count();
        assert_eq!(count, 1, "shared line must appear exactly once after merge");
        assert!(merged.contains("only-a"));
        assert!(merged.contains("only-b"));
    }

    #[test]
    fn merge_jsonl_logs_handles_empty_inputs() {
        assert_eq!(merge_jsonl_logs("", ""), "");
        let only = format!("{}\n", ts_line("2026-06-10T05:00:01Z", "build", "x"));
        assert_eq!(merge_jsonl_logs(&only, ""), only);
        assert_eq!(merge_jsonl_logs("", &only), only);
    }

    #[test]
    fn merge_jsonl_logs_skips_blank_lines() {
        let a = format!("\n{}\n\n", ts_line("2026-06-10T05:00:01Z", "s", "x"));
        let merged = merge_jsonl_logs(&a, "");
        assert_eq!(merged.lines().count(), 1);
    }

    #[test]
    fn merge_jsonl_logs_all_lines_without_ts_preserves_input_order() {
        // Tie-breaker for the case where every line lacks `"ts"`. Rust's
        // `sort_by` is stable, so the insertion order (a then b) must
        // survive. Documents the contract; locks in current behaviour.
        let a = "{\"step\":\"a1\"}\n{\"step\":\"a2\"}\n";
        let b = "{\"step\":\"b1\"}\n{\"step\":\"b2\"}\n";
        let merged = merge_jsonl_logs(a, b);
        let lines: Vec<&str> = merged.lines().collect();
        assert_eq!(
            lines,
            vec![
                r#"{"step":"a1"}"#,
                r#"{"step":"a2"}"#,
                r#"{"step":"b1"}"#,
                r#"{"step":"b2"}"#,
            ]
        );
    }

    #[test]
    fn extract_ts_pulls_top_level_field() {
        let line = ts_line("2026-06-10T05:00:01Z", "build", "hello");
        assert_eq!(extract_ts(&line), Some("2026-06-10T05:00:01Z"));
        // No ts field → None.
        assert_eq!(extract_ts(r#"{"step":"x"}"#), None);
        // Malformed: closing quote missing → None (not a panic).
        assert_eq!(extract_ts(r#"{"ts":"unclosed"#), None);
    }

    #[test]
    fn extract_ts_finds_first_ts_in_serde_order() {
        // `serde_json::to_string(&LogEntry)` emits fields in struct
        // declaration order, putting "ts" first. So the fast-path scanner
        // sees the real ts before any literal "ts" substring embedded in
        // a later field like `line`. This locks that in: if `LogEntry` is
        // ever reordered to put `line` first, this test catches it.
        let line = serde_json::json!({
            "ts": "2026-06-10T05:00:01Z",
            "step": "s",
            "line": r#"user wrote: {"ts":"fake-2026"}"#
        })
        .to_string();
        assert_eq!(extract_ts(&line), Some("2026-06-10T05:00:01Z"));
    }

    // ─── get_log / get_step_log merge behavior for terminal jobs ──────────

    #[tokio::test]
    async fn get_log_merges_local_and_archive_when_terminal() {
        let tmp = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());
        let job_id = Uuid::new_v4();
        let meta = test_meta();

        // Local has step[0] only; archive has step[1] only — the canonical
        // HA-mirror-loss scenario.
        let local = format!(
            "{}\n",
            ts_line("2026-06-10T05:00:01Z", "step[0]", "from-local")
        );
        let archive = format!(
            "{}\n",
            ts_line("2026-06-10T05:00:02Z", "step[1]", "from-archive")
        );
        storage.append_log(job_id, &local).await.unwrap();
        // Seed mock archive at the structured key get_log will read.
        let key = archive_key("", job_id, &meta);
        mock.put(&key, "application/gzip", Bytes::from(gzip(&archive)))
            .await
            .unwrap();

        // Non-terminal: only local returned (back-compat path).
        let non_term = storage.get_log(job_id, &meta, false).await.unwrap();
        assert!(non_term.contains("from-local"));
        assert!(!non_term.contains("from-archive"));

        // Terminal: merged view contains both.
        let term = storage.get_log(job_id, &meta, true).await.unwrap();
        assert!(term.contains("from-local"));
        assert!(term.contains("from-archive"));
        // Sorted by ts: local's 05:00:01 comes before archive's 05:00:02.
        let local_pos = term.find("from-local").unwrap();
        let archive_pos = term.find("from-archive").unwrap();
        assert!(local_pos < archive_pos);
    }

    #[tokio::test]
    async fn get_step_log_merges_local_and_archive_when_terminal() {
        let tmp = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());
        let job_id = Uuid::new_v4();
        let meta = test_meta();

        // Both sources have lines for the SAME step but disjoint timestamps —
        // exactly what the per-step HA gap looks like.
        let local = format!(
            "{}\n",
            ts_line("2026-06-10T05:00:01Z", "build", "local-line")
        );
        let archive = format!(
            "{}\n",
            ts_line("2026-06-10T05:00:02Z", "build", "archive-line")
        );
        storage.append_log(job_id, &local).await.unwrap();
        let key = archive_key("", job_id, &meta);
        mock.put(&key, "application/gzip", Bytes::from(gzip(&archive)))
            .await
            .unwrap();

        let merged = storage
            .get_step_log(job_id, "build", &meta, true)
            .await
            .unwrap();
        assert!(merged.contains("local-line"));
        assert!(merged.contains("archive-line"));
    }

    #[tokio::test]
    async fn get_log_terminal_falls_back_to_archive_when_local_missing() {
        let tmp = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());
        let job_id = Uuid::new_v4();
        let meta = test_meta();

        // No local writes at all — only the archive has content.
        let archive = format!("{}\n", ts_line("2026-06-10T05:00:01Z", "s", "only-archive"));
        let key = archive_key("", job_id, &meta);
        mock.put(&key, "application/gzip", Bytes::from(gzip(&archive)))
            .await
            .unwrap();

        let term = storage.get_log(job_id, &meta, true).await.unwrap();
        assert!(term.contains("only-archive"));
    }

    #[tokio::test]
    async fn get_log_terminal_archive_returns_none_serves_local_only() {
        // Terminal job, archive configured, but the key is missing from the
        // archive (e.g. upload genuinely never happened). Local must still
        // be served — and there must be only ONE archive fetch attempt
        // (previously this path issued two GETs).
        let tmp = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());
        let job_id = Uuid::new_v4();
        let meta = test_meta();

        let local = format!("{}\n", ts_line("2026-06-10T05:00:01Z", "s", "only-local"));
        storage.append_log(job_id, &local).await.unwrap();
        // Mock archive is intentionally empty.

        let result = storage.get_log(job_id, &meta, true).await.unwrap();
        assert!(result.contains("only-local"));
        assert_eq!(
            mock.get_call_count().await,
            1,
            "archive should be fetched at most once"
        );
    }

    #[tokio::test]
    async fn get_log_terminal_archive_error_falls_back_to_local() {
        // Critical regression test: a transient archive failure on a
        // terminal job must NOT 500 the caller — the local file is intact
        // and serving it is strictly better than an error. Pre-fix, the
        // `?` on the archive fetch bubbled the error out.
        let tmp = TempDir::new().unwrap();
        let archive = Arc::new(ErroringArchive);
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&archive) as Arc<dyn BlobArchive>, "".to_string());
        let job_id = Uuid::new_v4();
        let meta = test_meta();

        let local = format!("{}\n", ts_line("2026-06-10T05:00:01Z", "s", "only-local"));
        storage.append_log(job_id, &local).await.unwrap();

        let result = storage
            .get_log(job_id, &meta, true)
            .await
            .expect("transient archive error must not propagate");
        assert!(result.contains("only-local"));
    }

    #[tokio::test]
    async fn get_step_log_terminal_archive_returns_none_serves_local_only() {
        let tmp = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());
        let job_id = Uuid::new_v4();
        let meta = test_meta();

        let local = format!(
            "{}\n",
            ts_line("2026-06-10T05:00:01Z", "build", "only-local")
        );
        storage.append_log(job_id, &local).await.unwrap();

        let result = storage
            .get_step_log(job_id, "build", &meta, true)
            .await
            .unwrap();
        assert!(result.contains("only-local"));
    }

    #[tokio::test]
    async fn get_step_log_terminal_archive_error_falls_back_to_local() {
        let tmp = TempDir::new().unwrap();
        let archive = Arc::new(ErroringArchive);
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&archive) as Arc<dyn BlobArchive>, "".to_string());
        let job_id = Uuid::new_v4();
        let meta = test_meta();

        let local = format!(
            "{}\n",
            ts_line("2026-06-10T05:00:01Z", "build", "only-local")
        );
        storage.append_log(job_id, &local).await.unwrap();

        let result = storage
            .get_step_log(job_id, "build", &meta, true)
            .await
            .expect("transient archive error must not propagate");
        assert!(result.contains("only-local"));
    }

    #[tokio::test]
    async fn get_step_log_terminal_merges_legacy_log_with_archive() {
        // Legacy `.log` file on this replica plus archive content from the
        // orchestrator — confirms the legacy fallback participates in the
        // merge path. Without this test, a deployment still serving from
        // pre-jsonl logs would silently miss the merge.
        let tmp = TempDir::new().unwrap();
        let mock = Arc::new(MockArchive::new());
        let storage = LogStorage::new(tmp.path())
            .with_archive(Arc::clone(&mock) as Arc<dyn BlobArchive>, "".to_string());
        let job_id = Uuid::new_v4();
        let meta = test_meta();

        // Write the legacy .log file directly (not via append_log, which
        // always writes .jsonl). filter_step_from_file_if_present sees it
        // because the .jsonl path doesn't exist.
        let legacy_path = tmp.path().join(format!("{job_id}.log"));
        tokio::fs::create_dir_all(tmp.path()).await.unwrap();
        let legacy_line = ts_line("2026-06-10T05:00:01Z", "build", "from-legacy");
        tokio::fs::write(&legacy_path, format!("{legacy_line}\n"))
            .await
            .unwrap();

        let archive = format!(
            "{}\n",
            ts_line("2026-06-10T05:00:02Z", "build", "from-archive")
        );
        let key = archive_key("", job_id, &meta);
        mock.put(&key, "application/gzip", Bytes::from(gzip(&archive)))
            .await
            .unwrap();

        let result = storage
            .get_step_log(job_id, "build", &meta, true)
            .await
            .unwrap();
        assert!(result.contains("from-legacy"));
        assert!(result.contains("from-archive"));
    }

    /// `BlobArchive` impl that always returns `Err` for `get`. Used to test
    /// that transient archive failures degrade to local-only rather than
    /// propagating as 5xx.
    struct ErroringArchive;

    #[async_trait::async_trait]
    impl BlobArchive for ErroringArchive {
        async fn put(&self, _: &str, _: &str, _: Bytes) -> Result<()> {
            Ok(())
        }
        async fn get(&self, _: &str) -> Result<Option<crate::blob_storage::Blob>> {
            anyhow::bail!("simulated archive failure")
        }
        async fn delete(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn delete_prefix(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn gzip(s: &str) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(s.as_bytes()).unwrap();
        enc.finish().unwrap()
    }
}
