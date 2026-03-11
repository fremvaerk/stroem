use crate::state::AppState;
use anyhow::{Context, Result};
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

/// Result of a cancel operation
#[derive(Debug)]
pub enum CancelResult {
    /// Job was cancelled successfully
    Cancelled,
    /// Job was not found
    NotFound,
    /// Job is already in a terminal state
    AlreadyTerminal,
}

/// Cancel a job and all its child jobs recursively.
///
/// 1. Mark the job as cancelled in the database
/// 2. Cancel all pending/ready steps
/// 3. Record running steps in `cancelled_jobs` set for worker polling
/// 4. Recurse into active child jobs
/// 5. If no running steps remain, trigger terminal handling immediately
///
/// **Race window**: There is a small window between the DB update (step 1) and
/// the in-memory set insertion (step 3). A worker polling `check_cancelled`
/// during this window will not yet see the cancellation, but will catch it on
/// the next poll cycle (typically 5s). This is acceptable since the DB is the
/// source of truth and the in-memory set is a best-effort optimisation.
///
/// **Restart behaviour**: The in-memory `cancelled_jobs` set is not persisted.
/// On server restart, any jobs that were cancelled but still had running steps
/// will be handled by the recovery sweeper: it detects stale workers, fails
/// their stuck steps, and orchestrates the job to terminal state.
#[tracing::instrument(skip(state))]
pub async fn cancel_job(state: &AppState, job_id: Uuid) -> Result<CancelResult> {
    // Check if job exists first
    if JobRepo::get(&state.pool, job_id).await?.is_none() {
        return Ok(CancelResult::NotFound);
    }

    // Try to cancel the job (only works for pending/running)
    let updated = JobRepo::cancel(&state.pool, job_id)
        .await
        .context("Failed to cancel job")?;

    if !updated {
        // Job is already terminal
        return Ok(CancelResult::AlreadyTerminal);
    }

    // Cancel all pending/ready steps
    let cancelled_count = JobStepRepo::cancel_pending_steps(&state.pool, job_id)
        .await
        .context("Failed to cancel pending steps")?;
    tracing::info!(
        "Cancelled {} pending/ready steps for job {}",
        cancelled_count,
        job_id
    );

    // Get running steps — these need active kill from the worker
    let running_steps = JobStepRepo::get_running_steps(&state.pool, job_id)
        .await
        .context("Failed to get running steps")?;

    let has_running_steps = !running_steps.is_empty();

    if has_running_steps {
        // Add to cancelled_jobs set so workers can detect it via polling
        state
            .cancelled_jobs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id);
        tracing::info!(
            "Added job {} to cancelled_jobs set ({} running steps to kill)",
            job_id,
            running_steps.len()
        );
    }

    // Log cancellation
    state
        .append_server_log(job_id, "Job cancelled by user")
        .await;

    // Recursively cancel child jobs
    let child_jobs = JobRepo::get_child_jobs(&state.pool, job_id)
        .await
        .context("Failed to get child jobs")?;

    for child in &child_jobs {
        if let Err(e) = Box::pin(cancel_job(state, child.job_id)).await {
            tracing::error!(
                "Failed to cancel child job {} of parent {}: {:#}",
                child.job_id,
                job_id,
                e
            );
        }
    }

    // If no running steps, we can finalize immediately
    if !has_running_steps {
        // All steps are now terminal — handle hooks, S3, parent propagation
        if let Err(e) = crate::job_recovery::handle_job_terminal(state, job_id).await {
            tracing::error!(
                "Failed to handle terminal state for cancelled job {}: {:#}",
                job_id,
                e
            );
        }
    }

    Ok(CancelResult::Cancelled)
}

/// Remove a job from the cancelled_jobs set. Called when a cancelled job
/// reaches terminal state (all steps done) to prevent unbounded memory growth.
pub fn clear_cancelled(state: &AppState, job_id: Uuid) {
    state
        .cancelled_jobs
        .write()
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
    use crate::config::{DbConfig, LogStorageConfig, RecoveryConfig, ServerConfig};
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
            acl: None,
        };
        let mgr = WorkspaceManager::from_config("default", WorkspaceConfig::new());
        let log_storage = LogStorage::new(log_dir);
        let pool = PgPool::connect_lazy("postgres://invalid:5432/db").unwrap();
        AppState::new(pool, mgr, config, log_storage, HashMap::new())
    }

    #[test]
    fn test_cancel_result_debug() {
        let result = CancelResult::Cancelled;
        assert!(format!("{:?}", result).contains("Cancelled"));

        let result = CancelResult::NotFound;
        assert!(format!("{:?}", result).contains("NotFound"));

        let result = CancelResult::AlreadyTerminal;
        assert!(format!("{:?}", result).contains("AlreadyTerminal"));
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
