use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// Log storage handles writing and reading job logs
pub struct LogStorage {
    base_dir: PathBuf,
}

impl LogStorage {
    /// Create a new log storage
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
        }
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

        Ok(())
    }

    /// Get the full log contents for a job.
    /// Checks for .jsonl first, falls back to legacy .log file.
    pub async fn get_log(&self, job_id: Uuid) -> Result<String> {
        let path = self.log_path(job_id);

        if path.exists() {
            return Self::read_file(&path).await;
        }

        // Fallback to legacy .log file
        let legacy_path = self.legacy_log_path(job_id);
        if legacy_path.exists() {
            return Self::read_file(&legacy_path).await;
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
    pub async fn get_step_log(&self, job_id: Uuid, step_name: &str) -> Result<String> {
        let full_log = self.get_log(job_id).await?;
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

        let log = storage.get_log(job_id).await.unwrap();
        assert_eq!(log, format!("{}\n{}\n", line1, line2));
    }

    #[tokio::test]
    async fn test_read_nonexistent_log() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());

        let job_id = Uuid::new_v4();
        let log = storage.get_log(job_id).await.unwrap();
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

        let log1 = storage.get_log(job1).await.unwrap();
        let log2 = storage.get_log(job2).await.unwrap();

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

        let build_logs = storage.get_step_log(job_id, "build").await.unwrap();
        assert_eq!(build_logs, format!("{}\n{}\n", build1, build2));

        let test_logs = storage.get_step_log(job_id, "test").await.unwrap();
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

        let logs = storage.get_step_log(job_id, "deploy").await.unwrap();
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

        let logs = storage.get_step_log(job_id, "build").await.unwrap();
        assert_eq!(logs, format!("{}\n", valid));
    }

    #[tokio::test]
    async fn test_get_step_log_empty_file() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());
        let job_id = Uuid::new_v4();

        let logs = storage.get_step_log(job_id, "build").await.unwrap();
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

        let log = storage.get_log(job_id).await.unwrap();
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

        let log = storage.get_log(job_id).await.unwrap();
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
        let build_logs = storage.get_step_log(job_id, "build").await.unwrap();
        assert_eq!(build_logs, format!("{}\n{}\n", stdout_line, stderr_line));
    }
}
