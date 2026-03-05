use crate::job_recovery::orchestrate_after_step;
use crate::state::AppState;
use anyhow::Result;
use std::time::Duration;
use stroem_db::{JobRepo, JobStepRepo, WorkerRepo};
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

    // Phase 1: Mark stale workers as inactive and fail their steps
    let stale_workers = WorkerRepo::mark_stale_inactive(&state.pool, timeout).await?;
    if !stale_workers.is_empty() {
        tracing::warn!("Marked {} worker(s) as inactive", stale_workers.len());

        let stale_steps =
            JobStepRepo::get_running_steps_for_workers(&state.pool, &stale_workers).await?;
        if !stale_steps.is_empty() {
            tracing::warn!("Recovering {} stale step(s)", stale_steps.len());

            for step_info in &stale_steps {
                let worker_label = step_info
                    .worker_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let error_msg = format!(
                    "Worker heartbeat timeout (worker {} unresponsive)",
                    worker_label
                );

                state
                    .append_server_log(
                        step_info.job_id,
                        &format!(
                            "[recovery] Worker {} unresponsive (heartbeat timeout), failing step '{}'",
                            worker_label, step_info.step_name
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

                if let Err(e) =
                    orchestrate_after_step(state, step_info.job_id, &step_info.step_name).await
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
        }
    }

    // Phase 2: Fail steps that exceeded their timeout
    let timed_out_steps = JobStepRepo::get_timed_out_steps(&state.pool).await?;
    for step_info in &timed_out_steps {
        let error_msg = "Step timed out (server-side enforcement)".to_string();

        state
            .append_server_log(
                step_info.job_id,
                &format!(
                    "[recovery] Step '{}' timed out, marking as failed",
                    step_info.step_name
                ),
            )
            .await;

        tracing::warn!(
            "Step '{}/{}' timed out, failing",
            step_info.job_id,
            step_info.step_name
        );

        JobStepRepo::mark_failed(
            &state.pool,
            step_info.job_id,
            &step_info.step_name,
            &error_msg,
        )
        .await?;

        if let Err(e) = orchestrate_after_step(state, step_info.job_id, &step_info.step_name).await
        {
            tracing::error!(
                "Failed to orchestrate after step timeout '{}/{}': {:#}",
                step_info.job_id,
                step_info.step_name,
                e
            );
            state
                .append_server_log(
                    step_info.job_id,
                    &format!(
                        "[recovery] Failed to orchestrate after step timeout '{}': {:#}",
                        step_info.step_name, e
                    ),
                )
                .await;
        }
    }

    // Phase 3: Cancel jobs that exceeded their timeout
    let timed_out_jobs = JobRepo::get_timed_out_jobs(&state.pool).await?;
    for job_id in &timed_out_jobs {
        tracing::warn!("Job {} timed out, cancelling", job_id);

        state
            .append_server_log(*job_id, "[recovery] Job timed out, cancelling")
            .await;

        if let Err(e) = crate::cancellation::cancel_job(state, *job_id).await {
            tracing::error!("Failed to cancel timed-out job {}: {:#}", job_id, e);
        }
    }

    // Phase 4: Fail ready steps with no matching active workers
    let unmatched_timeout = state.config.recovery.unmatched_step_timeout_secs as f64;
    let unmatched = JobStepRepo::get_unmatched_ready_steps(&state.pool, unmatched_timeout).await?;
    for step_info in &unmatched {
        let error_msg = "No active worker with required tags to run this step";

        state
            .append_server_log(
                step_info.job_id,
                &format!(
                    "[recovery] Step '{}' has been ready for {}s with no matching worker, failing",
                    step_info.step_name, state.config.recovery.unmatched_step_timeout_secs
                ),
            )
            .await;

        tracing::warn!(
            "Step '{}/{}' has no matching worker, failing",
            step_info.job_id,
            step_info.step_name
        );

        JobStepRepo::mark_failed(
            &state.pool,
            step_info.job_id,
            &step_info.step_name,
            error_msg,
        )
        .await?;

        if let Err(e) = orchestrate_after_step(state, step_info.job_id, &step_info.step_name).await
        {
            tracing::error!(
                "Failed to orchestrate after unmatched step '{}/{}': {:#}",
                step_info.job_id,
                step_info.step_name,
                e
            );
            state
                .append_server_log(
                    step_info.job_id,
                    &format!(
                        "[recovery] Failed to orchestrate after unmatched step '{}': {:#}",
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
            libraries: HashMap::new(),
            git_auth: HashMap::new(),
            worker_token: "test".to_string(),
            auth: None,
            recovery: RecoveryConfig {
                heartbeat_timeout_secs: 120,
                sweep_interval_secs: 1,
                unmatched_step_timeout_secs: 30,
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
