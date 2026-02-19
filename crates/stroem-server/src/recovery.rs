use crate::job_recovery::orchestrate_after_step;
use crate::state::AppState;
use anyhow::Result;
use std::time::Duration;
use stroem_db::{JobStepRepo, WorkerRepo};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Spawn the recovery sweeper background task.
///
/// Periodically checks for stale workers (heartbeat timeout exceeded),
/// marks them inactive, fails their running steps, and triggers
/// orchestration to cascade failures and mark jobs as failed.
pub fn start(state: AppState, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_loop(state, cancel).await;
    })
}

async fn run_loop(state: AppState, cancel: CancellationToken) {
    let interval = Duration::from_secs(state.config.recovery.sweep_interval_secs);
    tracing::info!(
        "Recovery sweeper started (interval={:?}, timeout={}s)",
        interval,
        state.config.recovery.heartbeat_timeout_secs
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {},
            _ = cancel.cancelled() => {
                tracing::info!("Recovery sweeper shutting down");
                return;
            }
        }

        if let Err(e) = sweep(&state).await {
            tracing::error!("Recovery sweep error: {:#}", e);
        }
    }
}

/// Run a single recovery sweep. Exposed for integration tests.
pub async fn sweep_once(state: &AppState) -> Result<()> {
    sweep(state).await
}

async fn sweep(state: &AppState) -> Result<()> {
    let timeout = state.config.recovery.heartbeat_timeout_secs as i64;

    // 1. Mark stale workers as inactive
    let stale_workers = WorkerRepo::mark_stale_inactive(&state.pool, timeout).await?;
    if stale_workers.is_empty() {
        return Ok(());
    }
    tracing::warn!("Marked {} worker(s) as inactive", stale_workers.len());

    // 2. Find running steps assigned to stale workers
    let stale_steps =
        JobStepRepo::get_running_steps_for_workers(&state.pool, &stale_workers).await?;
    if stale_steps.is_empty() {
        return Ok(());
    }
    tracing::warn!("Recovering {} stale step(s)", stale_steps.len());

    // 3. Fail each stale step and orchestrate
    for step_info in &stale_steps {
        let error_msg = format!(
            "Worker heartbeat timeout (worker {} unresponsive)",
            step_info.worker_id
        );

        state
            .append_server_log(
                step_info.job_id,
                &format!(
                    "[recovery] Worker {} unresponsive (heartbeat timeout), failing step '{}'",
                    step_info.worker_id, step_info.step_name
                ),
            )
            .await;

        JobStepRepo::mark_failed(
            &state.pool,
            step_info.job_id,
            &step_info.step_name,
            &error_msg,
        )
        .await?;

        // Orchestrate the job (promotes steps, skips unreachable, checks terminal)
        if let Err(e) = orchestrate_after_step(state, step_info.job_id, &step_info.step_name).await
        {
            tracing::error!(
                "Failed to orchestrate after recovering step '{}/{}': {:#}",
                step_info.job_id,
                step_info.step_name,
                e
            );
            state
                .append_server_log(
                    step_info.job_id,
                    &format!(
                        "[recovery] Failed to orchestrate after recovering step '{}': {:#}",
                        step_info.step_name, e
                    ),
                )
                .await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_recovery_cancellation() {
        use crate::config::{DbConfig, LogStorageConfig, RecoveryConfig, ServerConfig};
        use crate::log_storage::LogStorage;
        use crate::workspace::WorkspaceManager;
        use std::collections::HashMap;
        use stroem_common::models::workflow::WorkspaceConfig;

        let config = ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            db: DbConfig {
                url: "postgres://invalid:5432/db".to_string(),
            },
            log_storage: LogStorageConfig {
                local_dir: "/tmp/test-logs".to_string(),
                s3: None,
            },
            workspaces: HashMap::new(),
            worker_token: "test".to_string(),
            auth: None,
            recovery: RecoveryConfig {
                heartbeat_timeout_secs: 120,
                sweep_interval_secs: 1,
            },
        };

        let ws_config = WorkspaceConfig::new();
        let mgr = WorkspaceManager::from_config("default", ws_config);
        let log_storage = LogStorage::new(&config.log_storage.local_dir);
        let pool = sqlx::PgPool::connect_lazy("postgres://invalid:5432/db").unwrap();
        let state = AppState::new(pool, mgr, config, log_storage, HashMap::new());

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = start(state, cancel.clone());

        // Cancel after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        // The sweeper should exit within a reasonable time
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "Recovery sweeper should have stopped after cancellation"
        );
    }
}
