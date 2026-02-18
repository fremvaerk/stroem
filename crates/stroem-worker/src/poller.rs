use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::client::ServerClient;
use crate::config::WorkerConfig;
use crate::executor::StepExecutor;
use crate::workspace_cache::WorkspaceCache;

/// Push a single error log line to the server (best-effort).
/// Uses stream "stderr" so the UI renders it in red.
async fn push_error_log(client: &ServerClient, job_id: Uuid, step_name: &str, message: &str) {
    let line = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "stream": "stderr",
        "line": message,
    });
    if let Err(e) = client.push_logs(job_id, step_name, vec![line]).await {
        tracing::warn!("Failed to push error log: {}", e);
    }
}

/// Extract a human-readable message from a JoinError (panic or cancellation).
fn extract_panic_message(join_err: tokio::task::JoinError) -> String {
    if join_err.is_panic() {
        let panic = join_err.into_panic();
        if let Some(msg) = panic.downcast_ref::<&str>() {
            format!("Runner panicked: {}", msg)
        } else if let Some(msg) = panic.downcast_ref::<String>() {
            format!("Runner panicked: {}", msg)
        } else {
            "Runner panicked (unknown payload)".to_string()
        }
    } else {
        "Step task was cancelled".to_string()
    }
}

/// Drain a mutex buffer, recovering from poisoning if needed.
fn drain_log_buffer(buffer: &Mutex<Vec<serde_json::Value>>) -> Vec<serde_json::Value> {
    let mut buf = match buffer.lock() {
        Ok(b) => b,
        Err(poisoned) => poisoned.into_inner(),
    };
    buf.drain(..).collect()
}

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
                            let err_msg = format!("Failed to download workspace: {}", e);
                            tracing::error!(
                                "Failed to download workspace '{}': {}",
                                step.workspace,
                                e
                            );
                            push_error_log(&client_clone, step.job_id, &step.step_name, &err_msg)
                                .await;
                            let _ = client_clone
                                .report_step_complete(
                                    step.job_id,
                                    &step.step_name,
                                    -1,
                                    None,
                                    Some(err_msg),
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
                        let err_msg = format!("Failed to report step start: {}", e);
                        tracing::error!("{}", err_msg);
                        push_error_log(&client_clone, step.job_id, &step.step_name, &err_msg).await;
                        let _ = client_clone
                            .report_step_complete(
                                step.job_id,
                                &step.step_name,
                                -1,
                                None,
                                Some(err_msg),
                            )
                            .await;
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
                            let lines = drain_log_buffer(&buffer_clone);
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

                    // Execute the step inside an inner spawn to catch panics
                    let inner_executor = executor_clone.clone();
                    let inner_step = step.clone();
                    let inner_ws = ws_dir_str.clone();
                    let inner_buffer = log_buffer.clone();
                    let exec_handle = tokio::spawn(async move {
                        inner_executor
                            .execute_step(&inner_step, &inner_ws, inner_buffer)
                            .await
                    });

                    let result = match exec_handle.await {
                        Ok(inner_result) => inner_result,
                        Err(join_err) => {
                            let msg = extract_panic_message(join_err);
                            Err(anyhow::anyhow!(msg))
                        }
                    };

                    // Stop log pusher, flush remaining logs
                    log_handle.abort();
                    let remaining = drain_log_buffer(&log_buffer);
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
                            let err_msg = e.to_string();
                            tracing::error!("Step execution failed: {}", err_msg);
                            push_error_log(&client_clone, step.job_id, &step.step_name, &err_msg)
                                .await;
                            if let Err(e) = client_clone
                                .report_step_complete(
                                    step.job_id,
                                    &step.step_name,
                                    -1,
                                    None,
                                    Some(err_msg),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;
    use std::panic::panic_any;
    use std::sync::Mutex;

    // --- extract_panic_message tests ---

    #[tokio::test]
    async fn test_extract_panic_message_str() {
        let handle = tokio::spawn(async {
            panic!("boom");
            #[allow(unreachable_code)]
            ()
        });
        let err = handle.await.unwrap_err();
        let msg = extract_panic_message(err);
        assert_eq!(msg, "Runner panicked: boom");
    }

    #[tokio::test]
    async fn test_extract_panic_message_string() {
        let handle = tokio::spawn(async {
            panic!("{}", String::from("kaboom"));
            #[allow(unreachable_code)]
            ()
        });
        let err = handle.await.unwrap_err();
        let msg = extract_panic_message(err);
        assert_eq!(msg, "Runner panicked: kaboom");
    }

    #[tokio::test]
    async fn test_extract_panic_message_format_args() {
        // panic! with format args is common in Rust code (e.g. `panic!("error: {}", val)`)
        // This produces a String payload via format!()
        let handle = tokio::spawn(async {
            let code = 42;
            panic!("crypto provider missing: error code {}", code);
            #[allow(unreachable_code)]
            ()
        });
        let err = handle.await.unwrap_err();
        let msg = extract_panic_message(err);
        assert_eq!(
            msg,
            "Runner panicked: crypto provider missing: error code 42"
        );
    }

    #[tokio::test]
    async fn test_extract_panic_message_unknown() {
        let handle = tokio::spawn(async {
            panic_any(42i32);
            #[allow(unreachable_code)]
            ()
        });
        let err = handle.await.unwrap_err();
        let msg = extract_panic_message(err);
        assert_eq!(msg, "Runner panicked (unknown payload)");
    }

    #[tokio::test]
    async fn test_extract_panic_message_cancelled() {
        // Simulate a cancelled task by aborting it
        let handle = tokio::spawn(async {
            // Sleep long enough to be aborted
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        handle.abort();
        let err = handle.await.unwrap_err();
        assert!(err.is_cancelled());
        let msg = extract_panic_message(err);
        assert_eq!(msg, "Step task was cancelled");
    }

    // --- Full panic-catching flow (mirrors the actual spawn+await pattern in run_worker) ---

    #[tokio::test]
    async fn test_inner_spawn_panic_yields_anyhow_err() {
        // This tests the exact pattern from the main code:
        // spawn inner → panic → JoinError → extract_panic_message → Err(anyhow)
        let exec_handle = tokio::spawn(async {
            panic!("rustls crypto provider not set");
            #[allow(unreachable_code)]
            Ok::<String, anyhow::Error>("done".to_string())
        });

        let result: anyhow::Result<String> = match exec_handle.await {
            Ok(inner_result) => inner_result,
            Err(join_err) => {
                let msg = extract_panic_message(join_err);
                Err(anyhow::anyhow!(msg))
            }
        };

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert_eq!(err_msg, "Runner panicked: rustls crypto provider not set");
    }

    #[tokio::test]
    async fn test_inner_spawn_success_passes_through() {
        // Verify normal (non-panic) results pass through correctly
        let exec_handle = tokio::spawn(async { Ok::<i32, anyhow::Error>(42) });

        let result: anyhow::Result<i32> = match exec_handle.await {
            Ok(inner_result) => inner_result,
            Err(join_err) => {
                let msg = extract_panic_message(join_err);
                Err(anyhow::anyhow!(msg))
            }
        };

        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_inner_spawn_error_passes_through() {
        // Verify normal errors (not panics) pass through correctly
        let exec_handle = tokio::spawn(async {
            Err::<i32, anyhow::Error>(anyhow::anyhow!("connection refused"))
        });

        let result: anyhow::Result<i32> = match exec_handle.await {
            Ok(inner_result) => inner_result,
            Err(join_err) => {
                let msg = extract_panic_message(join_err);
                Err(anyhow::anyhow!(msg))
            }
        };

        assert!(result.is_err());
        // Should be the original error, not a panic message
        assert_eq!(result.unwrap_err().to_string(), "connection refused");
    }

    // --- drain_log_buffer tests ---

    #[test]
    fn test_drain_log_buffer_normal() {
        let buffer = Mutex::new(vec![
            serde_json::json!({"stream": "stdout", "line": "hello"}),
            serde_json::json!({"stream": "stderr", "line": "warning"}),
        ]);
        let lines = drain_log_buffer(&buffer);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["line"], "hello");
        assert_eq!(lines[1]["line"], "warning");

        // Buffer should be empty after drain
        let lines = drain_log_buffer(&buffer);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_drain_log_buffer_empty() {
        let buffer = Mutex::new(Vec::<serde_json::Value>::new());
        let lines = drain_log_buffer(&buffer);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_drain_log_buffer_many_entries() {
        let entries: Vec<serde_json::Value> = (0..100)
            .map(|i| serde_json::json!({"line": format!("line-{}", i)}))
            .collect();
        let buffer = Mutex::new(entries);
        let lines = drain_log_buffer(&buffer);
        assert_eq!(lines.len(), 100);
        assert_eq!(lines[0]["line"], "line-0");
        assert_eq!(lines[99]["line"], "line-99");
    }

    #[test]
    fn test_poisoned_mutex_recovery() {
        let mutex = Arc::new(Mutex::new(vec![serde_json::json!({"line": "before"})]));

        // Poison the mutex by panicking while holding the lock
        let mutex_clone = mutex.clone();
        let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _guard = mutex_clone.lock().unwrap();
            panic!("poisoning the mutex");
        }));

        // Verify the mutex is poisoned
        assert!(mutex.lock().is_err());

        // drain_log_buffer should recover from the poisoned mutex
        let lines = drain_log_buffer(&mutex);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["line"], "before");

        // Buffer should now be empty (and still poisoned, but drain handles it)
        let lines = drain_log_buffer(&mutex);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_poisoned_mutex_repeated_drain_after_push() {
        let mutex = Arc::new(Mutex::new(vec![serde_json::json!({"line": "initial"})]));

        // Poison the mutex
        let mutex_clone = mutex.clone();
        let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _guard = mutex_clone.lock().unwrap();
            panic!("poison");
        }));

        // Drain the initial data
        let lines = drain_log_buffer(&mutex);
        assert_eq!(lines.len(), 1);

        // After poisoning, we can still push via into_inner and drain again
        // This simulates what happens when the log callback writes after a panic
        match mutex.lock() {
            Ok(mut buf) => buf.push(serde_json::json!({"line": "after"})),
            Err(poisoned) => poisoned
                .into_inner()
                .push(serde_json::json!({"line": "after"})),
        }

        let lines = drain_log_buffer(&mutex);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["line"], "after");
    }
}
