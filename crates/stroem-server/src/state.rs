use crate::acl::AclEvaluator;
use crate::config::ServerConfig;
use crate::events::EventBus;
use crate::job_completion::JobCompletionNotifier;
use crate::leader::LeaderElection;
use crate::log_broadcast::LogBroadcast;
use crate::log_storage::LogStorage;
use crate::oidc::OidcProvider;
use crate::state_storage::StateStorage;
use crate::tarball_cache::TarballCache;
use crate::workspace::WorkspaceManager;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use uuid::Uuid;

/// Drop guard that sets the alive flag to `true` on creation and `false` on drop.
/// Handles both clean exit and panics.
pub(crate) struct AliveGuard(Arc<AtomicBool>);

impl AliveGuard {
    pub(crate) fn new(flag: Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Relaxed);
        Self(flag)
    }
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Tracks liveness of background tasks (scheduler, recovery sweeper, event source manager).
/// Each flag is `true` while the task is running, `false` after it exits.
#[derive(Clone)]
pub struct BackgroundTasks {
    pub scheduler_alive: Arc<AtomicBool>,
    pub recovery_alive: Arc<AtomicBool>,
    pub event_source_alive: Arc<AtomicBool>,
}

impl BackgroundTasks {
    pub fn new() -> Self {
        Self {
            scheduler_alive: Arc::new(AtomicBool::new(false)),
            recovery_alive: Arc::new(AtomicBool::new(false)),
            event_source_alive: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for BackgroundTasks {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub workspaces: Arc<WorkspaceManager>,
    pub config: Arc<ServerConfig>,
    pub acl: Arc<AclEvaluator>,
    pub log_broadcast: Arc<LogBroadcast>,
    pub log_storage: Arc<LogStorage>,
    pub tarball_cache: Arc<TarballCache>,
    pub oidc_providers: Arc<HashMap<String, OidcProvider>>,
    pub job_completion: Arc<JobCompletionNotifier>,
    /// Set of job IDs that have been cancelled. Workers poll this to detect
    /// cancellation and kill running processes.
    pub cancelled_jobs: Arc<std::sync::RwLock<HashSet<Uuid>>>,
    /// Liveness flags for background tasks (scheduler, recovery sweeper).
    pub background_tasks: BackgroundTasks,
    /// Unix timestamp (seconds) of the last retention cleanup run.
    /// Initialized to 0 so the first sweep always triggers a cleanup.
    pub last_retention_run: Arc<AtomicI64>,
    /// Optional task state snapshot storage (None when feature is not configured).
    pub state_storage: Option<Arc<StateStorage>>,
    /// Shared blob archive backend used by logs, state, and artifacts.
    /// `None` when no archive backend is configured.
    pub blob_archive: Option<Arc<dyn crate::blob_storage::BlobArchive>>,
    /// Dedicated blob archive backend for artifacts. Falls back to the shared
    /// `blob_archive` when no `artifact_storage.archive` override is set. `None`
    /// when no backend is configured at all.
    pub artifact_blob: Option<Arc<dyn crate::blob_storage::BlobArchive>>,
    /// Artifact storage config (size limits, prefix). Defaults are populated
    /// from `ArtifactStorageConfig::default()` when the operator omits the
    /// `artifact_storage` section entirely.
    pub artifact_config: crate::config::ArtifactStorageConfig,
    /// Leader-election handle. Background tasks gate themselves on
    /// `leader.is_leader()`. Defaults to an always-leader for tests and
    /// single-replica deployments; real HA wiring happens in `main.rs`.
    pub leader: Arc<LeaderElection>,
    /// Cross-replica event publisher (NOTIFY). Tests get a no-op bus that
    /// silently discards publishes.
    pub event_bus: EventBus,
}

impl AppState {
    /// Create a new app state
    pub fn new(
        pool: PgPool,
        workspaces: WorkspaceManager,
        config: ServerConfig,
        log_storage: LogStorage,
        oidc_providers: HashMap<String, OidcProvider>,
        state_storage: Option<StateStorage>,
    ) -> Self {
        let acl = AclEvaluator::new(config.acl.clone());
        // Place tarball cache alongside log storage directory
        let tarball_cache_dir = PathBuf::from(&config.log_storage.local_dir)
            .parent()
            .map(|p| p.join("tarball_cache"))
            .unwrap_or_else(|| PathBuf::from("tarball_cache"));
        let tarball_cache = TarballCache::new(tarball_cache_dir);
        let artifact_config = config.artifact_storage.clone().unwrap_or_default();
        Self {
            pool,
            workspaces: Arc::new(workspaces),
            config: Arc::new(config),
            acl: Arc::new(acl),
            log_broadcast: Arc::new(LogBroadcast::new()),
            log_storage: Arc::new(log_storage),
            tarball_cache: Arc::new(tarball_cache),
            oidc_providers: Arc::new(oidc_providers),
            job_completion: Arc::new(JobCompletionNotifier::new()),
            cancelled_jobs: Arc::new(std::sync::RwLock::new(HashSet::new())),
            background_tasks: BackgroundTasks::new(),
            last_retention_run: Arc::new(AtomicI64::new(0)),
            state_storage: state_storage.map(Arc::new),
            blob_archive: None,
            artifact_blob: None,
            artifact_config,
            leader: LeaderElection::always(),
            event_bus: EventBus::noop(),
        }
    }

    /// Replace the artifact blob backend. Used by `main.rs` after constructing
    /// either a dedicated backend from `artifact_storage.archive` or reusing
    /// the shared log archive. Tests/single-replica builds default to `None`.
    pub fn with_artifact_blob(
        mut self,
        archive: Option<Arc<dyn crate::blob_storage::BlobArchive>>,
    ) -> Self {
        self.artifact_blob = archive;
        self
    }

    /// Compose the storage key for an artifact:
    /// `{prefix}{workspace}/{job_id}/{step}/{name}`.
    pub fn artifact_storage_key(
        &self,
        workspace: &str,
        job_id: Uuid,
        step: &str,
        name: &str,
    ) -> String {
        format!(
            "{}{}/{}/{}/{}",
            self.artifact_config.prefix, workspace, job_id, step, name
        )
    }

    /// Replace the shared blob archive. Used by `main.rs` after constructing
    /// the backend from `log_storage.archive`. Tests/single-replica builds
    /// default to `None`.
    pub fn with_blob_archive(
        mut self,
        archive: Option<Arc<dyn crate::blob_storage::BlobArchive>>,
    ) -> Self {
        self.blob_archive = archive;
        self
    }

    /// Replace the leader handle. Used by `main.rs` after spawning the HA
    /// election loop. Test/single-replica builds use the default always-leader.
    pub fn with_leader(mut self, leader: Arc<LeaderElection>) -> Self {
        self.leader = leader;
        self
    }

    /// Replace the event bus. Used by `main.rs` after constructing the
    /// publisher with the shared `PgPool`. Tests use the default no-op bus.
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.event_bus = bus;
        self
    }

    /// Get the workspace config for a given workspace name
    pub async fn get_workspace(&self, name: &str) -> Option<Arc<WorkspaceConfig>> {
        self.workspaces.get_config(name).await
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
            tracing::warn!("Failed to write server log for job {}: {:#}", job_id, e);
            return;
        }
        self.log_broadcast.broadcast(job_id, chunk.clone()).await;
        // Forward to peer replicas so WS viewers on followers see server-side
        // errors (recovery, hooks, orchestration) live, not just on this replica.
        self.event_bus.publish_log_chunk(job_id, &chunk).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DbConfig, LogStorageConfig, RecoveryConfig, RetentionConfig};
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
                archive: None,
            },
            workspaces: HashMap::new(),
            libraries: HashMap::new(),
            git_auth: HashMap::new(),
            worker_token: "test".to_string(),
            auth: None,
            recovery: RecoveryConfig {
                heartbeat_timeout_secs: 120,
                sweep_interval_secs: 60,
                unmatched_step_timeout_secs: 30,
                ..Default::default()
            },
            retention: RetentionConfig::default(),
            acl: None,
            mcp: None,
            metrics: None,
            agents: None,
            state_storage: None,
            artifact_storage: None,
            default_step_timeout: None,
            default_job_timeout: None,
        };
        let mgr = WorkspaceManager::from_config("default", WorkspaceConfig::new());
        let log_storage = LogStorage::new(log_dir);
        let pool = PgPool::connect_lazy("postgres://invalid:5432/db").unwrap();
        AppState::new(pool, mgr, config, log_storage, HashMap::new(), None)
    }

    #[tokio::test]
    async fn test_cancelled_jobs_set_operations() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        // Initially empty
        assert!(!state.cancelled_jobs.read().unwrap().contains(&job_id));

        // Insert
        state.cancelled_jobs.write().unwrap().insert(job_id);
        assert!(state.cancelled_jobs.read().unwrap().contains(&job_id));

        // Remove
        state.cancelled_jobs.write().unwrap().remove(&job_id);
        assert!(!state.cancelled_jobs.read().unwrap().contains(&job_id));
    }

    #[tokio::test]
    async fn test_append_server_log_writes_jsonl() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        state
            .append_server_log(job_id, "Hook template rendering failed")
            .await;

        let meta = crate::log_storage::JobLogMeta {
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            created_at: chrono::Utc::now(),
        };
        let log = state.log_storage.get_log(job_id, &meta).await.unwrap();
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

        let meta = crate::log_storage::JobLogMeta {
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            created_at: chrono::Utc::now(),
        };

        // Regular step logs should not include _server entries
        let build_logs = state
            .log_storage
            .get_step_log(job_id, "build", &meta)
            .await
            .unwrap();
        assert!(build_logs.contains("compiling..."));
        assert!(!build_logs.contains("Hook failed"));

        // _server logs should be retrievable separately
        let server_logs = state
            .log_storage
            .get_step_log(job_id, "_server", &meta)
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

        let meta = crate::log_storage::JobLogMeta {
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            created_at: chrono::Utc::now(),
        };
        let server_logs = state
            .log_storage
            .get_step_log(job_id, "_server", &meta)
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

        let meta = crate::log_storage::JobLogMeta {
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            created_at: chrono::Utc::now(),
        };
        let logs1 = state
            .log_storage
            .get_step_log(job1, "_server", &meta)
            .await
            .unwrap();
        let logs2 = state
            .log_storage
            .get_step_log(job2, "_server", &meta)
            .await
            .unwrap();

        assert!(logs1.contains("error for job1"));
        assert!(!logs1.contains("error for job2"));
        assert!(logs2.contains("error for job2"));
        assert!(!logs2.contains("error for job1"));
    }
}
