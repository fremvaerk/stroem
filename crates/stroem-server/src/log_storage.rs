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

    /// Get the log file path for a job
    fn log_path(&self, job_id: Uuid) -> PathBuf {
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

    /// Get the full log contents for a job
    pub async fn get_log(&self, job_id: Uuid) -> Result<String> {
        let path = self.log_path(job_id);

        if !path.exists() {
            return Ok(String::new());
        }

        let mut file = fs::File::open(&path)
            .await
            .with_context(|| format!("Failed to open log file: {:?}", path))?;

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .context("Failed to read log file")?;

        Ok(contents)
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

    #[tokio::test]
    async fn test_append_and_read_log() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LogStorage::new(temp_dir.path());

        let job_id = Uuid::new_v4();

        // Append first chunk
        storage.append_log(job_id, "Line 1\n").await.unwrap();

        // Append second chunk
        storage.append_log(job_id, "Line 2\n").await.unwrap();

        // Read back
        let log = storage.get_log(job_id).await.unwrap();
        assert_eq!(log, "Line 1\nLine 2\n");
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

        storage.append_log(job1, "Job 1 log\n").await.unwrap();
        storage.append_log(job2, "Job 2 log\n").await.unwrap();

        let log1 = storage.get_log(job1).await.unwrap();
        let log2 = storage.get_log(job2).await.unwrap();

        assert_eq!(log1, "Job 1 log\n");
        assert_eq!(log2, "Job 2 log\n");
    }

    #[tokio::test]
    async fn test_auto_create_directory() {
        let temp_dir = TempDir::new().unwrap();
        let log_dir = temp_dir.path().join("logs");

        // Directory doesn't exist yet
        assert!(!log_dir.exists());

        let storage = LogStorage::new(&log_dir);
        let job_id = Uuid::new_v4();

        storage.append_log(job_id, "test\n").await.unwrap();

        // Directory should now exist
        assert!(log_dir.exists());
    }
}
