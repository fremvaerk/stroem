use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::client::ServerClient;
use crate::config::WorkerConfig;
use crate::executor::StepExecutor;
use crate::workspace_cache::WorkspaceCache;

/// Main worker loop: register, heartbeat, and poll for jobs
#[tracing::instrument(skip(config, executor))]
pub async fn run_worker(config: WorkerConfig, executor: StepExecutor) -> Result<()> {
    tracing::info!("Starting worker '{}'", config.worker_name);

    // Create shared clients, executor, and workspace cache
    let client = ServerClient::new(&config.server_url, &config.worker_token);
    let executor = Arc::new(executor);
    let workspace_cache = Arc::new(WorkspaceCache::new(&config.workspace_cache_dir));

    // Ensure workspace cache base directory exists
    std::fs::create_dir_all(&config.workspace_cache_dir)
        .context("Failed to create workspace cache directory")?;

    // Register with server
    let tags = config.tags.as_deref();
    let worker_id = client
        .register(&config.worker_name, &config.capabilities, tags)
        .await
        .context("Failed to register worker")?;
    tracing::info!("Registered as worker {}", worker_id);

    // Spawn heartbeat task (every 30s)
    let heartbeat_client = client.clone();
    let heartbeat_worker_id = worker_id;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if let Err(e) = heartbeat_client.heartbeat(heartbeat_worker_id).await {
                tracing::warn!("Heartbeat failed: {}", e);
            } else {
                tracing::debug!("Heartbeat sent successfully");
            }
        }
    });

    // Main poll loop with semaphore for max_concurrent
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent));
    let poll_interval = Duration::from_secs(config.poll_interval_secs);

    tracing::info!(
        "Polling for jobs every {}s with max {} concurrent executions",
        config.poll_interval_secs,
        config.max_concurrent
    );

    loop {
        // Acquire semaphore permit
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .context("Failed to acquire semaphore permit")?;

        // Try to claim a step
        match client
            .claim_step(worker_id, &config.capabilities, tags)
            .await
        {
            Ok(Some(step)) => {
                tracing::info!(
                    "Claimed step '{}' for job {} (workspace: {}, action: {})",
                    step.step_name,
                    step.job_id,
                    step.workspace,
                    step.action_name
                );

                let client_clone = client.clone();
                let executor_clone = executor.clone();
                let ws_cache_clone = workspace_cache.clone();
                let step_worker_id = worker_id;

                // Spawn a task to execute the step
                tokio::spawn(async move {
                    let _permit = permit; // Hold permit until done

                    // Ensure workspace is downloaded and up-to-date
                    let ws_dir = match ws_cache_clone
                        .ensure_up_to_date(&client_clone, &step.workspace)
                        .await
                    {
                        Ok(dir) => dir,
                        Err(e) => {
                            tracing::error!(
                                "Failed to download workspace '{}': {}",
                                step.workspace,
                                e
                            );
                            // Report step failure
                            let _ = client_clone
                                .report_step_complete(
                                    step.job_id,
                                    &step.step_name,
                                    -1,
                                    None,
                                    Some(format!("Failed to download workspace: {}", e)),
                                )
                                .await;
                            return;
                        }
                    };
                    let ws_dir_str = ws_dir.to_string_lossy().to_string();

                    // Report step start
                    if let Err(e) = client_clone
                        .report_step_start(step.job_id, &step.step_name, step_worker_id)
                        .await
                    {
                        tracing::error!("Failed to report step start: {}", e);
                        return;
                    }

                    // Execute step with log buffering
                    let log_buffer = Arc::new(Mutex::new(Vec::new()));

                    // Spawn log pusher (flushes buffer every 1s)
                    let buffer_clone = log_buffer.clone();
                    let log_client = client_clone.clone();
                    let log_job_id = step.job_id;
                    let log_step_name = step.step_name.clone();
                    let log_handle = tokio::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(1));
                        loop {
                            interval.tick().await;
                            let lines: Vec<_> = {
                                let mut buf = buffer_clone.lock().unwrap();
                                buf.drain(..).collect()
                            };
                            if !lines.is_empty() {
                                if let Err(e) = log_client
                                    .push_logs(log_job_id, &log_step_name, lines)
                                    .await
                                {
                                    tracing::warn!("Failed to push logs: {}", e);
                                }
                            }
                        }
                    });

                    // Execute the step
                    let result = executor_clone
                        .execute_step(&step, &ws_dir_str, log_buffer.clone())
                        .await;

                    // Stop log pusher, flush remaining logs
                    log_handle.abort();
                    let remaining: Vec<_> = log_buffer.lock().unwrap().drain(..).collect();
                    if !remaining.is_empty() {
                        if let Err(e) = client_clone
                            .push_logs(step.job_id, &step.step_name, remaining)
                            .await
                        {
                            tracing::warn!("Failed to push final logs: {}", e);
                        }
                    }

                    // Report result
                    match result {
                        Ok(run_result) => {
                            let error = if run_result.exit_code != 0 {
                                Some(format!(
                                    "Exit code: {}\nStderr: {}",
                                    run_result.exit_code, run_result.stderr
                                ))
                            } else {
                                None
                            };
                            if let Err(e) = client_clone
                                .report_step_complete(
                                    step.job_id,
                                    &step.step_name,
                                    run_result.exit_code,
                                    run_result.output,
                                    error,
                                )
                                .await
                            {
                                tracing::error!("Failed to report step complete: {}", e);
                            } else {
                                tracing::info!(
                                    "Successfully completed step '{}' for job {}",
                                    step.step_name,
                                    step.job_id
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("Step execution failed: {}", e);
                            if let Err(e) = client_clone
                                .report_step_complete(
                                    step.job_id,
                                    &step.step_name,
                                    -1,
                                    None,
                                    Some(e.to_string()),
                                )
                                .await
                            {
                                tracing::error!("Failed to report step error: {}", e);
                            }
                        }
                    }
                });
            }
            Ok(None) => {
                // No work available, drop permit and sleep
                drop(permit);
                tracing::debug!("No work available, sleeping");
                tokio::time::sleep(poll_interval).await;
            }
            Err(e) => {
                tracing::warn!("Failed to claim step: {}", e);
                drop(permit);
                tokio::time::sleep(poll_interval).await;
            }
        }
    }
}
