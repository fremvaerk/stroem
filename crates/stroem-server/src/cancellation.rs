use crate::state::AppState;
use uuid::Uuid;

/// Remove a job from the cancelled_jobs set. Called when a cancelled job
/// reaches terminal state (all steps done) to prevent unbounded memory growth.
pub fn clear_cancelled(state: &AppState, job_id: Uuid) {
    clear_cancelled_in(&state.cancelled_jobs, job_id);
}

/// [`clear_cancelled`] against the bare set, for callers that hold the shared
/// `cancelled_jobs` handle rather than the whole `AppState`.
pub fn clear_cancelled_in(set: &std::sync::RwLock<std::collections::HashSet<Uuid>>, job_id: Uuid) {
    set.write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&job_id);
}

/// Check if a job is in the cancelled set.
pub fn is_cancelled(state: &AppState, job_id: Uuid) -> bool {
    state
        .cancelled_jobs
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&job_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DbConfig, LogStorageConfig, RecoveryConfig, RetentionConfig, ServerConfig,
    };
    use crate::log_storage::LogStorage;
    use crate::workspace::WorkspaceManager;
    use sqlx::PgPool;
    use std::collections::HashMap;
    use stroem_common::models::workflow::WorkspaceConfig;
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
    async fn test_is_cancelled_returns_false_for_unknown_job() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        assert!(!is_cancelled(&state, Uuid::new_v4()));
    }

    #[tokio::test]
    async fn test_is_cancelled_returns_true_after_insert() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        state.cancelled_jobs.write().unwrap().insert(job_id);
        assert!(is_cancelled(&state, job_id));
    }

    #[tokio::test]
    async fn test_clear_cancelled_removes_from_set() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        state.cancelled_jobs.write().unwrap().insert(job_id);
        assert!(is_cancelled(&state, job_id));

        clear_cancelled(&state, job_id);
        assert!(!is_cancelled(&state, job_id));
    }

    #[tokio::test]
    async fn test_clear_cancelled_noop_for_unknown_job() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        // Should not panic
        clear_cancelled(&state, Uuid::new_v4());
    }

    #[tokio::test]
    async fn test_multiple_jobs_in_cancelled_set() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job1 = Uuid::new_v4();
        let job2 = Uuid::new_v4();

        state.cancelled_jobs.write().unwrap().insert(job1);
        state.cancelled_jobs.write().unwrap().insert(job2);

        assert!(is_cancelled(&state, job1));
        assert!(is_cancelled(&state, job2));

        clear_cancelled(&state, job1);
        assert!(!is_cancelled(&state, job1));
        assert!(is_cancelled(&state, job2));
    }

    #[tokio::test]
    async fn test_clear_cancelled_is_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        state.cancelled_jobs.write().unwrap().insert(job_id);

        // First call removes the entry
        clear_cancelled(&state, job_id);
        // Second call on an already-absent entry must not panic
        clear_cancelled(&state, job_id);

        assert!(!is_cancelled(&state, job_id));
    }

    #[test]
    fn test_cancelled_set_concurrent_read_write() {
        use std::collections::HashSet;
        use std::sync::{Arc, RwLock};

        let set: Arc<RwLock<HashSet<Uuid>>> = Arc::new(RwLock::new(HashSet::new()));
        let mut handles = Vec::new();

        // Spawn writer threads — each inserts a unique job_id
        for _ in 0..8 {
            let set_clone = Arc::clone(&set);
            handles.push(std::thread::spawn(move || {
                let id = Uuid::new_v4();
                set_clone
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id);
                id
            }));
        }

        // Collect inserted IDs and verify none of the accesses panicked
        let mut inserted_ids = Vec::new();
        for handle in handles {
            inserted_ids.push(handle.join().expect("writer thread panicked"));
        }

        // Spawn reader threads — each checks a random inserted id
        let mut read_handles = Vec::new();
        for id in &inserted_ids {
            let set_clone = Arc::clone(&set);
            let id = *id;
            read_handles.push(std::thread::spawn(move || {
                set_clone
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&id)
            }));
        }

        for handle in read_handles {
            // Every id that was written must be visible after all writers finished
            assert!(handle.join().expect("reader thread panicked"));
        }
    }

    #[tokio::test]
    async fn test_is_cancelled_after_clear_returns_false() {
        let temp_dir = TempDir::new().unwrap();
        let state = test_state(temp_dir.path());
        let job_id = Uuid::new_v4();

        // Insert and confirm presence
        state.cancelled_jobs.write().unwrap().insert(job_id);
        assert!(
            is_cancelled(&state, job_id),
            "job should be cancelled after insert"
        );

        // Remove and confirm absence
        clear_cancelled(&state, job_id);
        assert!(
            !is_cancelled(&state, job_id),
            "job should not be cancelled after clear"
        );
    }
}
