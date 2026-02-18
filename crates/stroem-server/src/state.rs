use crate::config::ServerConfig;
use crate::log_broadcast::LogBroadcast;
use crate::log_storage::LogStorage;
use crate::oidc::OidcProvider;
use crate::workspace::WorkspaceManager;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use uuid::Uuid;

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub workspaces: Arc<WorkspaceManager>,
    pub config: Arc<ServerConfig>,
    pub log_broadcast: Arc<LogBroadcast>,
    pub log_storage: Arc<LogStorage>,
    pub oidc_providers: Arc<HashMap<String, OidcProvider>>,
}

impl AppState {
    /// Create a new app state
    pub fn new(
        pool: PgPool,
        workspaces: WorkspaceManager,
        config: ServerConfig,
        log_storage: LogStorage,
        oidc_providers: HashMap<String, OidcProvider>,
    ) -> Self {
        Self {
            pool,
            workspaces: Arc::new(workspaces),
            config: Arc::new(config),
            log_broadcast: Arc::new(LogBroadcast::new()),
            log_storage: Arc::new(log_storage),
            oidc_providers: Arc::new(oidc_providers),
        }
    }

    /// Get the workspace config for a given workspace name
    pub async fn get_workspace(&self, name: &str) -> Option<WorkspaceConfig> {
        self.workspaces.get_config_async(name).await
    }

    /// Append a server-side log entry to a job's log stream.
    ///
    /// Writes a JSONL line with `step: "_server"` and `stream: "stderr"`,
    /// then broadcasts it via WebSocket so live viewers see it immediately.
    /// Failures are logged but never propagated (best-effort).
    pub async fn append_server_log(&self, job_id: Uuid, message: &str) {
        let line = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "stream": "stderr",
            "step": "_server",
            "line": message,
        });
        let chunk = format!("{}\n", line);
        if let Err(e) = self.log_storage.append_log(job_id, &chunk).await {
            tracing::warn!("Failed to write server log for job {}: {}", job_id, e);
            return;
        }
        self.log_broadcast.broadcast(job_id, chunk).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DbConfig, LogStorageConfig, RecoveryConfig};
    use crate::workspace::WorkspaceManager;
    use tempfile::TempDir;

    fn test_state(log_dir: &std::path::Path) -> AppState {
        let config = ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            db: DbConfig {
                url: "postgres://invalid:5432/db".to_string(),
            },
            log_storage: LogStorageConfig {
                local_dir: log_dir.to_string_lossy().to_string(),
                s3: None,
            },
            workspaces: HashMap::new(),
            worker_token: "test".to_string(),
            auth: None,
            recovery: RecoveryConfig {
                heartbeat_timeout_secs: 120,
                sweep_interval_secs: 60,
            },
        };
        let mgr = WorkspaceManager::from_config("default", WorkspaceConfig::new());
        let log_storage = LogStorage::new(log_dir);
        let pool = PgPool::connect_lazy("postgres://invalid:5432/db").unwrap();
        AppState::new(pool, mgr, config, log_storage, HashMap::new())
    }

    #[tokio::test]
    async fn test_append_server_log_writes_jsonl() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        state
            .append_server_log(job_id, "Hook template rendering failed")
            .await;

        let log = state.log_storage.get_log(job_id).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(parsed["step"], "_server");
        assert_eq!(parsed["stream"], "stderr");
        assert_eq!(parsed["line"], "Hook template rendering failed");
        assert!(parsed["ts"].as_str().is_some());
    }

    #[tokio::test]
    async fn test_append_server_log_broadcasts() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        let mut rx = state.log_broadcast.subscribe(job_id).await;

        state.append_server_log(job_id, "Recovery timeout").await;

        let msg = rx.recv().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(msg.trim()).unwrap();
        assert_eq!(parsed["step"], "_server");
        assert_eq!(parsed["line"], "Recovery timeout");
    }

    #[tokio::test]
    async fn test_server_logs_filtered_from_step_logs() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        // Write a regular step log
        let regular_line = serde_json::json!({
            "ts": "2025-01-01T00:00:00Z",
            "stream": "stdout",
            "step": "build",
            "line": "compiling...",
        });
        state
            .log_storage
            .append_log(job_id, &format!("{}\n", regular_line))
            .await
            .unwrap();

        // Write a server log
        state.append_server_log(job_id, "Hook failed").await;

        // Regular step logs should not include _server entries
        let build_logs = state
            .log_storage
            .get_step_log(job_id, "build")
            .await
            .unwrap();
        assert!(build_logs.contains("compiling..."));
        assert!(!build_logs.contains("Hook failed"));

        // _server logs should be retrievable separately
        let server_logs = state
            .log_storage
            .get_step_log(job_id, "_server")
            .await
            .unwrap();
        assert!(server_logs.contains("Hook failed"));
        assert!(!server_logs.contains("compiling..."));
    }

    #[tokio::test]
    async fn test_append_server_log_multiple_entries() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        state
            .append_server_log(job_id, "[hooks] Failed to fire hook on_success[0]")
            .await;
        state
            .append_server_log(job_id, "[recovery] Worker abc unresponsive")
            .await;

        let server_logs = state
            .log_storage
            .get_step_log(job_id, "_server")
            .await
            .unwrap();
        let lines: Vec<&str> = server_logs.trim().lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(first["line"].as_str().unwrap().contains("hook on_success"));
        assert!(second["line"].as_str().unwrap().contains("Worker abc"));
    }

    #[tokio::test]
    async fn test_append_server_log_no_interference_between_jobs() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job1 = Uuid::new_v4();
        let job2 = Uuid::new_v4();

        state.append_server_log(job1, "error for job1").await;
        state.append_server_log(job2, "error for job2").await;

        let logs1 = state
            .log_storage
            .get_step_log(job1, "_server")
            .await
            .unwrap();
        let logs2 = state
            .log_storage
            .get_step_log(job2, "_server")
            .await
            .unwrap();

        assert!(logs1.contains("error for job1"));
        assert!(!logs1.contains("error for job2"));
        assert!(logs2.contains("error for job2"));
        assert!(!logs2.contains("error for job1"));
    }
}
