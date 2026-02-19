use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

/// Log storage handles writing and reading job logs
#[derive(Clone)]
pub struct LogStorage {
    base_dir: PathBuf,
    #[cfg(feature = "s3")]
    s3: Option<S3Backend>,
}

impl LogStorage {
    /// Create a new log storage
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            #[cfg(feature = "s3")]
            s3: None,
        }
    }

    /// Configure S3 backend for log archival
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

    /// Configure S3 backend with a pre-built client (for testing)
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

    /// Get the JSONL log file path for a job
    fn log_path(&self, job_id: Uuid) -> PathBuf {
        self.base_dir.join(format!("{}.jsonl", job_id))
    }

    /// Get the legacy .log file path (for backward compatibility)
    fn legacy_log_path(&self, job_id: Uuid) -> PathBuf {
        self.base_dir.join(format!("{}.log", job_id))
    }

    /// Ensure the log directory exists
    async fn ensure_dir(&self) -> Result<()> {
        if !self.base_dir.exists() {
            fs::create_dir_all(&self.base_dir)
                .await
                .context("Failed to create log directory")?;
        }
        Ok(())
    }

    /// Append a log chunk to a job's log file
    pub async fn append_log(&self, job_id: Uuid, chunk: &str) -> Result<()> {
        self.ensure_dir().await?;

        let path = self.log_path(job_id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("Failed to open log file: {:?}", path))?;

        file.write_all(chunk.as_bytes())
            .await
            .context("Failed to write to log file")?;

        file.flush().await.context("Failed to flush log file")?;

        Ok(())
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

    /// Download a job's log from S3 (gzip-compressed). Returns None if the key doesn't exist.
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
    /// Checks for .jsonl first, falls back to legacy .log file, then S3.
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

    /// Read file contents
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
    /// Parses each line as JSON and filters by the `step` field.
    /// Non-JSON lines (legacy format) are skipped.
    pub async fn get_step_log(
        &self,
        job_id: Uuid,
        step_name: &str,
        meta: &JobLogMeta,
    ) -> Result<String> {
        let full_log = self.get_log(job_id, meta).await?;
        let filtered: Vec<&str> = full_log
            .lines()
            .filter(|line| {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) {
                    parsed.get("step").and_then(|s| s.as_str()) == Some(step_name)
                } else {
                    false
                }
            })
            .collect();
        Ok(if filtered.is_empty() {
            String::new()
        } else {
            filtered.join("\n") + "\n"
        })
    }

    /// Get the log file path as a string (for storing in database)
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
