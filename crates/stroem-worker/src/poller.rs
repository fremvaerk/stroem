use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::client::ServerClient;
use crate::config::WorkerConfig;
use crate::executor::StepExecutor;
use crate::workspace_cache::WorkspaceCache;

/// How long to wait for in-flight steps to finish on shutdown before giving up.
const DRAIN_TIMEOUT_SECS: u64 = 30;

/// Push a single error log line to the server (best-effort).
/// Uses stream "stderr" so the UI renders it in red.
async fn push_error_log(client: &ServerClient, job_id: Uuid, step_name: &str, message: &str) {
    let line = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "stream": "stderr",
        "line": message,
    });
    if let Err(e) = client.push_logs(job_id, step_name, vec![line]).await {
        tracing::warn!("Failed to push error log: {:#}", e);
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

/// Extract a gzipped tarball into a destination directory.
fn extract_state_tarball(data: &[u8], dest: &std::path::Path) -> anyhow::Result<()> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    let decoder = GzDecoder::new(Cursor::new(data));
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest)
        .context("Failed to unpack state tarball")?;
    Ok(())
}

/// Build a gzipped tarball from a directory's contents.
fn build_state_tarball(dir: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut archive = tar::Builder::new(&mut encoder);
        archive
            .append_dir_all(".", dir)
            .context("Failed to create state tarball")?;
        archive.finish().context("Failed to finish state tarball")?;
    }
    encoder
        .finish()
        .context("Failed to finish gzip compression")
}

/// Returns true if the directory contains at least one entry (file or subdirectory).
fn dir_has_content(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut rd| rd.next().is_some())
        .unwrap_or(false)
}

/// Shallow-merge parsed JSON entries into a single `serde_json::Map`.
///
/// Each entry should be a JSON object. Top-level keys from later entries
/// overwrite earlier ones (last writer wins). Non-object entries are skipped.
/// Returns `None` if the input is empty or no entries are objects.
fn merge_state_entries(
    entries: &[serde_json::Value],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if entries.is_empty() {
        return None;
    }
    let mut merged = serde_json::Map::new();
    for val in entries {
        if let Some(obj) = val.as_object() {
            for (k, v) in obj {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// Execute a claimed step: download workspace, report start, run with log buffering, report result.
///
/// This function owns the claimed step and drives the full execution lifecycle. It is spawned as
/// a `tokio::spawn` task by [`run_worker`] so each step runs concurrently.
pub(crate) async fn execute_claimed_step(
    client: Arc<ServerClient>,
    executor: Arc<StepExecutor>,
    ws_cache: Arc<WorkspaceCache>,
    step: crate::client::ClaimedStep,
    worker_id: Uuid,
    #[cfg(feature = "agent")] agents_config: Option<Arc<stroem_agent::config::AgentsConfig>>,
) {
    // ── Agent step routing ────────────────────────────────────────────────────
    // Agent steps are dispatched entirely on the worker without a workspace.
    // They make LLM API calls directly using the local provider config.
    #[cfg(feature = "agent")]
    if step.action_type == "agent" {
        // Report step start so the job transitions from pending to running
        if let Err(e) = client
            .report_step_start(step.job_id, &step.step_name, worker_id)
            .await
        {
            tracing::error!("Failed to report agent step start: {:#}", e);
        }

        // Create a per-step cancellation token (used by the dispatch loop)
        let step_cancel = CancellationToken::new();

        let result = crate::agent_executor::execute_agent_step(
            &client,
            &step,
            agents_config.as_deref(),
            step_cancel,
        )
        .await;

        match result {
            crate::agent_executor::AgentResult::Completed { output } => {
                if let Err(e) = client
                    .report_step_complete(step.job_id, &step.step_name, 0, Some(output), None)
                    .await
                {
                    tracing::error!("Failed to report agent step complete: {:#}", e);
                }
            }
            crate::agent_executor::AgentResult::WaitingForTools
            | crate::agent_executor::AgentResult::Suspended => {
                // Worker's work is done — step stays running/suspended on server.
                // It will be re-claimed once child jobs finish or user approves.
            }
            crate::agent_executor::AgentResult::Failed { error } => {
                push_error_log(&client, step.job_id, &step.step_name, &error).await;
                if let Err(e) = client
                    .report_step_complete(step.job_id, &step.step_name, 1, None, Some(error))
                    .await
                {
                    tracing::error!("Failed to report agent step failure: {:#}", e);
                }
            }
        }
        return;
    }

    // Ensure workspace is downloaded and up-to-date (or pinned to a specific
    // revision). The guard keeps the revision directory alive (ref-counted) for
    // the duration of step execution, preventing cleanup from deleting it.
    let ws_result = if let Some(ref rev) = step.revision {
        ws_cache
            .ensure_revision(&client, &step.workspace, rev)
            .await
    } else {
        ws_cache.ensure_up_to_date(&client, &step.workspace).await
    };
    let ws_guard = match ws_result {
        Ok(guard) => guard,
        Err(e) => {
            let err_msg = format!("Failed to download workspace: {:#}", e);
            tracing::error!("Failed to download workspace '{}': {:#}", step.workspace, e);
            push_error_log(&client, step.job_id, &step.step_name, &err_msg).await;
            let _ = client
                .report_step_complete(step.job_id, &step.step_name, -1, None, Some(err_msg))
                .await;
            return;
        }
    };
    let ws_dir_str = ws_guard.path().to_string_lossy().to_string();

    // Report step start
    if let Err(e) = client
        .report_step_start(step.job_id, &step.step_name, worker_id)
        .await
    {
        let err_msg = format!("Failed to report step start: {:#}", e);
        tracing::error!("{}", err_msg);
        push_error_log(&client, step.job_id, &step.step_name, &err_msg).await;
        let _ = client
            .report_step_complete(step.job_id, &step.step_name, -1, None, Some(err_msg))
            .await;
        return;
    }

    // Execute step with log buffering
    let log_buffer = Arc::new(Mutex::new(Vec::new()));

    // Spawn log pusher (flushes buffer every 1s)
    let log_cancel = CancellationToken::new();
    let buffer_clone = log_buffer.clone();
    let log_client = client.clone();
    let log_job_id = step.job_id;
    let log_step_name = step.step_name.clone();
    let log_cancel_clone = log_cancel.clone();
    let log_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                () = log_cancel_clone.cancelled() => break,
            }
            let lines = drain_log_buffer(&buffer_clone);
            if !lines.is_empty() {
                if let Err(e) = log_client
                    .push_logs(log_job_id, &log_step_name, lines)
                    .await
                {
                    tracing::warn!("Failed to push logs: {:#}", e);
                }
            }
        }
    });

    // Create a cancellation token for this step and spawn a cancel-checker
    let step_cancel = CancellationToken::new();
    let cancel_client = client.clone();
    let cancel_job_id = step.job_id;
    let cancel_token_clone = step_cancel.clone();
    let cancel_checker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            match cancel_client.check_job_cancelled(cancel_job_id).await {
                Ok(true) => {
                    tracing::info!(
                        "Job {} cancelled by server, signalling runner",
                        cancel_job_id
                    );
                    cancel_token_clone.cancel();
                    break;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        "Failed to check cancellation for job {}: {:#}",
                        cancel_job_id,
                        e
                    );
                }
            }
        }
    });

    // ── Task state snapshot: create temp dirs and download previous state ────
    let state_temp = match tempfile::TempDir::new() {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(
                "Failed to create state temp dir, state will be unavailable: {:#}",
                e
            );
            None
        }
    };
    let (state_dir_path, state_out_dir_path) = if let Some(ref temp) = state_temp {
        let sd = temp.path().join("state");
        let sod = temp.path().join("state-out");
        let _ = std::fs::create_dir_all(&sd);
        let _ = std::fs::create_dir_all(&sod);
        (Some(sd), Some(sod))
    } else {
        (None, None)
    };

    // Download previous state snapshot if the server indicates one exists
    if step.state_storage_key.is_some() {
        if let Some(ref sd) = state_dir_path {
            match client
                .download_state_tarball(&step.workspace, &step.task_name)
                .await
            {
                Ok(Some(bytes)) => {
                    if let Err(e) = extract_state_tarball(&bytes, sd) {
                        tracing::warn!("Failed to extract state tarball: {:#}", e);
                    } else {
                        tracing::debug!(
                            "Extracted state snapshot for {}/{}",
                            step.workspace,
                            step.task_name
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        "Failed to download state for {}/{}, continuing with empty: {:#}",
                        step.workspace,
                        step.task_name,
                        e
                    );
                }
            }
        }
    }

    let state_dir_str = state_dir_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let state_out_dir_str = state_out_dir_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());

    // ── Global workspace state: create temp dirs and download ─────────────────
    let global_state_temp = match tempfile::TempDir::new() {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(
                "Failed to create global state temp dir, global state will be unavailable: {:#}",
                e
            );
            None
        }
    };
    let (global_state_dir_path, global_state_out_dir_path) =
        if let Some(ref temp) = global_state_temp {
            let gsd = temp.path().join("global-state");
            let gsod = temp.path().join("global-state-out");
            let _ = std::fs::create_dir_all(&gsd);
            let _ = std::fs::create_dir_all(&gsod);
            (Some(gsd), Some(gsod))
        } else {
            (None, None)
        };

    // Download previous global state snapshot if the server indicates one exists
    if step.global_state_storage_key.is_some() {
        if let Some(ref gsd) = global_state_dir_path {
            match client.download_global_state_tarball(&step.workspace).await {
                Ok(Some(bytes)) => {
                    if let Err(e) = extract_state_tarball(&bytes, gsd) {
                        tracing::warn!("Failed to extract global state tarball: {:#}", e);
                    } else {
                        tracing::debug!("Extracted global state snapshot for {}", step.workspace);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        "Failed to download global state for {}, continuing with empty: {:#}",
                        step.workspace,
                        e
                    );
                }
            }
        }
    }

    let global_state_dir_str = global_state_dir_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let global_state_out_dir_str = global_state_out_dir_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());

    // Execute the step inside an inner spawn to catch panics
    let inner_executor = executor.clone();
    let inner_step = step.clone();
    let inner_ws = ws_dir_str.clone();
    let inner_buffer = log_buffer.clone();
    let inner_cancel = step_cancel.clone();
    let inner_client = client.clone();
    let inner_state_dir = state_dir_str.clone();
    let inner_state_out_dir = state_out_dir_str.clone();
    let inner_global_state_dir = global_state_dir_str.clone();
    let inner_global_state_out_dir = global_state_out_dir_str.clone();
    let exec_handle = tokio::spawn(async move {
        inner_executor
            .execute_step(
                &inner_step,
                &inner_ws,
                inner_state_dir.as_deref(),
                inner_state_out_dir.as_deref(),
                inner_global_state_dir.as_deref(),
                inner_global_state_out_dir.as_deref(),
                inner_buffer,
                inner_cancel,
                &inner_client,
            )
            .await
    });

    let result = if let Some(timeout_secs) = step
        .timeout_secs
        .and_then(|t| u64::try_from(t).ok())
        .filter(|&t| t > 0)
    {
        // Save an abort handle before consuming exec_handle with the timeout future.
        // If the timeout fires the JoinHandle is dropped (detached, not cancelled), so we
        // interact with the still-running task via abort_handle only.
        let abort_handle = exec_handle.abort_handle();
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), exec_handle).await
        {
            Ok(join_result) => match join_result {
                Ok(r) => r,
                Err(join_err) => {
                    let msg = extract_panic_message(join_err);
                    Err(anyhow::anyhow!(msg))
                }
            },
            Err(_elapsed) => {
                tracing::warn!(
                    step = %step.step_name,
                    timeout_secs,
                    "Step timed out, cancelling"
                );
                step_cancel.cancel(); // signal runner to stop gracefully
                                      // Give runner up to 10s to clean up (stop containers, delete pods) before
                                      // hard-aborting the task.
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                if !abort_handle.is_finished() {
                    tracing::warn!(
                        step = %step.step_name,
                        "Runner did not stop within grace period, aborting"
                    );
                    abort_handle.abort();
                }
                Err(anyhow::anyhow!("Step timed out after {}s", timeout_secs))
            }
        }
    } else {
        match exec_handle.await {
            Ok(inner_result) => inner_result,
            Err(join_err) => {
                let msg = extract_panic_message(join_err);
                Err(anyhow::anyhow!(msg))
            }
        }
    };

    // Stop the cancel-checker task
    cancel_checker.abort();

    // Signal the log pusher to stop after its current flush completes
    log_cancel.cancel();
    // Wait for log pusher to finish (timeout prevents hanging if something goes wrong)
    let _ = tokio::time::timeout(Duration::from_secs(5), log_handle).await;

    // ── Collect STATE: and GLOBAL_STATE: lines BEFORE draining the log buffer ──
    // The drain below empties the buffer, so we must extract entries first.
    // Both collections are gathered in a single lock acquisition.
    let (state_entries, global_state_entries): (Vec<serde_json::Value>, Vec<serde_json::Value>) = {
        match log_buffer.lock() {
            Ok(buffer) => {
                let mut state = Vec::new();
                let mut global = Vec::new();
                for entry in buffer.iter() {
                    if entry.get("stream").and_then(|s| s.as_str()) != Some("stdout") {
                        continue;
                    }
                    if let Some(line) = entry.get("line").and_then(|l| l.as_str()) {
                        if let Some(v) = stroem_runner::parse_state_line(line) {
                            state.push(v);
                        }
                        if let Some(v) = stroem_runner::parse_global_state_line(line) {
                            global.push(v);
                        }
                    }
                }
                (state, global)
            }
            Err(_) => (vec![], vec![]),
        }
    };

    // Push any lines that accumulated between the last flush and shutdown
    let remaining = drain_log_buffer(&log_buffer);
    if !remaining.is_empty() {
        if let Err(e) = client
            .push_logs(step.job_id, &step.step_name, remaining)
            .await
        {
            tracing::warn!("Failed to push final logs: {:#}", e);
        }
    }

    // ── Upload new state if /state-out has content ───────────────────────────
    if result.is_ok() {
        if let Some(ref sod) = state_out_dir_path {
            if let Some(merged) = merge_state_entries(&state_entries) {
                let state_json_path = sod.join("state.json");
                match serde_json::to_vec_pretty(&serde_json::Value::Object(merged)) {
                    Ok(json_bytes) => {
                        if let Err(e) = std::fs::write(&state_json_path, json_bytes) {
                            tracing::warn!("Failed to write state.json: {:#}", e);
                        }
                    }
                    Err(e) => tracing::warn!("Failed to serialize state.json: {:#}", e),
                }
            }

            if dir_has_content(sod) {
                match build_state_tarball(sod) {
                    Ok(tarball_bytes) => {
                        let has_json = sod.join("state.json").exists();
                        if let Err(e) = client
                            .upload_state_tarball(
                                &step.workspace,
                                &step.task_name,
                                step.job_id,
                                tarball_bytes,
                                has_json,
                            )
                            .await
                        {
                            tracing::warn!("Failed to upload state snapshot: {:#}", e);
                        } else {
                            tracing::info!(
                                "Uploaded state snapshot for {}/{} (job {})",
                                step.workspace,
                                step.task_name,
                                step.job_id
                            );
                        }
                    }
                    Err(e) => tracing::warn!("Failed to build state tarball: {:#}", e),
                }
            }
        }

        // ── Upload global state if /global-state-out has content ─────────────
        if let Some(ref gsod) = global_state_out_dir_path {
            if let Some(merged) = merge_state_entries(&global_state_entries) {
                let state_json_path = gsod.join("state.json");
                match serde_json::to_vec_pretty(&serde_json::Value::Object(merged)) {
                    Ok(json_bytes) => {
                        if let Err(e) = std::fs::write(&state_json_path, json_bytes) {
                            tracing::warn!("Failed to write global state.json: {:#}", e);
                        }
                    }
                    Err(e) => tracing::warn!("Failed to serialize global state.json: {:#}", e),
                }
            }

            if dir_has_content(gsod) {
                match build_state_tarball(gsod) {
                    Ok(tarball_bytes) => {
                        let has_json = gsod.join("state.json").exists();
                        if let Err(e) = client
                            .upload_global_state_tarball(
                                &step.workspace,
                                step.job_id,
                                tarball_bytes,
                                has_json,
                            )
                            .await
                        {
                            tracing::warn!("Failed to upload global state snapshot: {:#}", e);
                        } else {
                            tracing::info!(
                                "Uploaded global state snapshot for {} (job {})",
                                step.workspace,
                                step.job_id
                            );
                        }
                    }
                    Err(e) => tracing::warn!("Failed to build global state tarball: {:#}", e),
                }
            }
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
            if let Err(e) = client
                .report_step_complete(
                    step.job_id,
                    &step.step_name,
                    run_result.exit_code,
                    run_result.output,
                    error,
                )
                .await
            {
                tracing::error!("Failed to report step complete: {:#}", e);
            } else {
                tracing::info!(
                    "Successfully completed step '{}' for job {}",
                    step.step_name,
                    step.job_id
                );
            }
        }
        Err(e) => {
            let err_msg = format!("{:#}", e);
            tracing::error!("Step execution failed: {}", err_msg);
            push_error_log(&client, step.job_id, &step.step_name, &err_msg).await;
            if let Err(e) = client
                .report_step_complete(step.job_id, &step.step_name, -1, None, Some(err_msg))
                .await
            {
                tracing::error!("Failed to report step error: {:#}", e);
            }
        }
    }
}

/// Compute the poll interval with exponential backoff.
///
/// Returns `base * 2^(min(consecutive_idle - 1, 2))`, capped at 4× base.
/// When `consecutive_idle` is 0 (work was just found), returns `base`.
fn compute_poll_backoff(base: Duration, consecutive_idle: u32) -> Duration {
    if consecutive_idle <= 1 {
        return base;
    }
    let exp = (consecutive_idle - 1).min(2);
    base * 2u32.pow(exp)
}

/// Main worker loop: register, heartbeat, and poll for jobs.
///
/// The `cancel_token` is used for graceful shutdown. When cancelled, the loop stops
/// accepting new work and waits up to `DRAIN_TIMEOUT_SECS` for in-flight steps to finish.
#[tracing::instrument(skip(config, executor, cancel_token))]
pub async fn run_worker(
    config: WorkerConfig,
    executor: StepExecutor,
    cancel_token: CancellationToken,
) -> Result<()> {
    tracing::info!("Starting worker '{}'", config.worker_name);

    // Create shared clients, executor, and workspace cache
    let client = Arc::new(ServerClient::new(
        &config.server_url,
        &config.worker_token,
        config.connect_timeout_secs,
        config.request_timeout_secs,
    ));
    let executor = Arc::new(executor);
    let workspace_cache = Arc::new(WorkspaceCache::new(
        &config.workspace_cache_dir,
        config.max_retained_revisions,
    ));
    #[cfg(feature = "agent")]
    let agents_config: Option<Arc<stroem_agent::config::AgentsConfig>> =
        config.agents.map(Arc::new);

    // Ensure workspace cache base directory exists
    std::fs::create_dir_all(&config.workspace_cache_dir)
        .context("Failed to create workspace cache directory")?;

    // Register with server (retry with exponential backoff, but stop if cancelled)
    let worker_id = {
        let mut attempt = 0u32;
        loop {
            tokio::select! {
                result = client.register(&config.worker_name, &config.tags, Some(env!("CARGO_PKG_VERSION"))) => {
                    match result {
                        Ok(id) => break id,
                        Err(e) => {
                            attempt += 1;
                            let delay = Duration::from_secs(2u64.saturating_pow(attempt).min(60));
                            tracing::warn!(
                                "Failed to register worker (attempt {attempt}), retrying in {}s: {:#}",
                                delay.as_secs(),
                                e
                            );
                            tokio::select! {
                                () = tokio::time::sleep(delay) => {},
                                () = cancel_token.cancelled() => {
                                    tracing::info!("Shutdown requested during registration, exiting");
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                () = cancel_token.cancelled() => {
                    tracing::info!("Shutdown requested during registration, exiting");
                    return Ok(());
                }
            }
        }
    };
    tracing::info!("Registered as worker {}", worker_id);

    // Spawn heartbeat task (every 30s), stops when cancel_token is cancelled
    let heartbeat_client = client.clone();
    let heartbeat_worker_id = worker_id;
    let heartbeat_cancel = cancel_token.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = heartbeat_client.heartbeat(heartbeat_worker_id).await {
                        tracing::warn!("Heartbeat failed: {:#}", e);
                    } else {
                        tracing::debug!("Heartbeat sent successfully");
                    }
                }
                () = heartbeat_cancel.cancelled() => {
                    tracing::debug!("Heartbeat task stopping (shutdown)");
                    break;
                }
            }
        }
    });

    // Main poll loop with semaphore for max_concurrent
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent));
    let poll_interval = Duration::from_secs(config.poll_interval_secs);

    tracing::info!(
        "Polling for jobs every {}s with max {} concurrent executions",
        config.poll_interval_secs,
        config.max_concurrent,
    );

    let mut consecutive_idle: u32 = 0;
    loop {
        // Acquire semaphore permit, or stop if cancelled
        let permit = tokio::select! {
            result = semaphore.clone().acquire_owned() => {
                result.context("Failed to acquire semaphore permit")?
            }
            () = cancel_token.cancelled() => {
                tracing::info!("Shutdown requested, stopping poll loop");
                break;
            }
        };

        // Try to claim a step, or stop if cancelled
        let claim_result = tokio::select! {
            result = client.claim_step(worker_id, &config.tags) => result,
            () = cancel_token.cancelled() => {
                tracing::info!("Shutdown requested, stopping poll loop");
                drop(permit);
                break;
            }
        };

        match claim_result {
            Ok(Some(step)) => {
                consecutive_idle = 0;
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
                #[cfg(feature = "agent")]
                let agents_config_clone = agents_config.clone();

                // Spawn a task to execute the step, releasing the permit when done
                tokio::spawn(async move {
                    let _permit = permit; // Hold permit until done
                    execute_claimed_step(
                        client_clone,
                        executor_clone,
                        ws_cache_clone,
                        step,
                        worker_id,
                        #[cfg(feature = "agent")]
                        agents_config_clone,
                    )
                    .await;
                });
            }
            Ok(None) => {
                // No work available, drop permit and sleep with backoff (or exit if cancelled)
                drop(permit);
                consecutive_idle = consecutive_idle.saturating_add(1);
                let backoff = compute_poll_backoff(poll_interval, consecutive_idle);
                tracing::debug!("No work available, sleeping {}ms", backoff.as_millis());
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {},
                    () = cancel_token.cancelled() => {
                        tracing::info!("Shutdown requested, stopping poll loop");
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to claim step: {:#}", e);
                drop(permit);
                consecutive_idle = consecutive_idle.saturating_add(1);
                let backoff = compute_poll_backoff(poll_interval, consecutive_idle);
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {},
                    () = cancel_token.cancelled() => {
                        tracing::info!("Shutdown requested, stopping poll loop");
                        break;
                    }
                }
            }
        }
    }

    // Drain: wait for all in-flight steps to complete
    tracing::info!(
        "Waiting up to {}s for in-flight steps to complete...",
        DRAIN_TIMEOUT_SECS
    );

    // Acquiring all permits means all spawned step tasks have released theirs (i.e., finished).
    let drain_result = tokio::time::timeout(
        Duration::from_secs(DRAIN_TIMEOUT_SECS),
        semaphore
            .acquire_many(u32::try_from(config.max_concurrent).expect("validated at config load")),
    )
    .await;

    match drain_result {
        Ok(Ok(_)) => {
            tracing::info!("All in-flight steps completed, worker shutting down cleanly");
        }
        Ok(Err(e)) => {
            tracing::warn!("Semaphore error during drain: {:#}", e);
        }
        Err(_) => {
            tracing::warn!(
                "Drain timeout ({}s) exceeded — some steps may not have finished",
                DRAIN_TIMEOUT_SECS
            );
        }
    }

    Ok(())
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

    // ─── State tarball tests ──────────────────────────────────────────

    #[test]
    fn test_dir_has_content_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(!super::dir_has_content(dir.path()));
    }

    #[test]
    fn test_dir_has_content_with_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), "hello").unwrap();
        assert!(super::dir_has_content(dir.path()));
    }

    #[test]
    fn test_state_tarball_roundtrip() {
        let src = tempfile::TempDir::new().unwrap();
        std::fs::write(src.path().join("state.json"), r#"{"key":"val"}"#).unwrap();
        std::fs::write(src.path().join("cert.pem"), "cert-data").unwrap();

        let tarball = super::build_state_tarball(src.path()).unwrap();

        let dst = tempfile::TempDir::new().unwrap();
        super::extract_state_tarball(&tarball, dst.path()).unwrap();

        let json = std::fs::read_to_string(dst.path().join("state.json")).unwrap();
        assert_eq!(json, r#"{"key":"val"}"#);
        let cert = std::fs::read_to_string(dst.path().join("cert.pem")).unwrap();
        assert_eq!(cert, "cert-data");
    }

    #[test]
    fn test_extract_state_tarball_malformed() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = super::extract_state_tarball(&[0x00, 0x01, 0x02], dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_build_state_tarball_empty_dir() {
        // An empty directory should still produce a valid (empty) tarball.
        let src = tempfile::TempDir::new().unwrap();
        let tarball = super::build_state_tarball(src.path()).unwrap();
        // A valid gzip stream starts with the magic bytes 0x1f 0x8b.
        assert!(tarball.len() >= 2);
        assert_eq!(tarball[0], 0x1f);
        assert_eq!(tarball[1], 0x8b);
    }

    // --- merge_state_entries tests ---

    #[test]
    fn test_merge_state_entries_empty_returns_none() {
        assert!(merge_state_entries(&[]).is_none());
    }

    #[test]
    fn test_merge_state_entries_single_object() {
        let entries = vec![serde_json::json!({"cursor": "abc", "count": 42})];
        let merged = merge_state_entries(&entries).unwrap();
        assert_eq!(merged["cursor"], "abc");
        assert_eq!(merged["count"], 42);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_merge_state_entries_multiple_distinct_keys() {
        let entries = vec![
            serde_json::json!({"cursor": "abc"}),
            serde_json::json!({"count": 42}),
            serde_json::json!({"last_run": "2026-01-01"}),
        ];
        let merged = merge_state_entries(&entries).unwrap();
        assert_eq!(merged["cursor"], "abc");
        assert_eq!(merged["count"], 42);
        assert_eq!(merged["last_run"], "2026-01-01");
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn test_merge_state_entries_last_writer_wins() {
        let entries = vec![
            serde_json::json!({"cursor": "first", "shared": "old"}),
            serde_json::json!({"cursor": "second"}),
            serde_json::json!({"cursor": "final", "shared": "new"}),
        ];
        let merged = merge_state_entries(&entries).unwrap();
        assert_eq!(merged["cursor"], "final");
        assert_eq!(merged["shared"], "new");
    }

    #[test]
    fn test_merge_state_entries_non_objects_skipped() {
        let entries = vec![
            serde_json::json!({"key": "value"}),
            serde_json::json!([1, 2, 3]),       // array — skipped
            serde_json::json!("just a string"), // string — skipped
            serde_json::json!(42),              // number — skipped
            serde_json::json!({"other": true}),
        ];
        let merged = merge_state_entries(&entries).unwrap();
        assert_eq!(merged["key"], "value");
        assert_eq!(merged["other"], true);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_merge_state_entries_all_non_objects_returns_none() {
        let entries = vec![serde_json::json!([1, 2]), serde_json::json!("string")];
        assert!(merge_state_entries(&entries).is_none());
    }

    // --- compute_poll_backoff tests ---

    #[test]
    fn test_poll_backoff_zero_returns_base() {
        let base = Duration::from_secs(2);
        assert_eq!(compute_poll_backoff(base, 0), base);
    }

    #[test]
    fn test_poll_backoff_one_returns_base() {
        let base = Duration::from_secs(2);
        assert_eq!(compute_poll_backoff(base, 1), base);
    }

    #[test]
    fn test_poll_backoff_two_doubles() {
        let base = Duration::from_secs(2);
        assert_eq!(compute_poll_backoff(base, 2), Duration::from_secs(4));
    }

    #[test]
    fn test_poll_backoff_three_quadruples() {
        let base = Duration::from_secs(2);
        assert_eq!(compute_poll_backoff(base, 3), Duration::from_secs(8));
    }

    #[test]
    fn test_poll_backoff_capped_at_4x() {
        let base = Duration::from_secs(2);
        assert_eq!(compute_poll_backoff(base, 100), Duration::from_secs(8));
        assert_eq!(compute_poll_backoff(base, 4), Duration::from_secs(8));
        assert_eq!(compute_poll_backoff(base, u32::MAX), Duration::from_secs(8));
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

    // --- execute_claimed_step integration tests (using wiremock) ---

    #[cfg(test)]
    mod execute_step_tests {
        use super::super::*;
        use crate::client::{ClaimedStep, ServerClient};
        use crate::executor::StepExecutor;
        use crate::workspace_cache::WorkspaceCache;
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use uuid::Uuid;
        use wiremock::matchers::{method, path, path_regex, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// Build a minimal valid gzipped tarball containing a single shell script.
        fn create_test_tarball() -> Vec<u8> {
            let buf = Vec::new();
            let encoder = GzEncoder::new(buf, Compression::default());
            let mut archive = tar::Builder::new(encoder);

            let data = b"#!/bin/sh\necho hello-from-workspace\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, "run.sh", &data[..])
                .unwrap();

            let encoder = archive.into_inner().unwrap();
            encoder.finish().unwrap()
        }

        /// Mount common server mocks (start, logs, complete, cancel check).
        async fn mount_common_mocks(mock_server: &MockServer) {
            Mock::given(method("POST"))
                .and(path_regex(r"/worker/jobs/.+/steps/.+/start"))
                .respond_with(ResponseTemplate::new(200))
                .mount(mock_server)
                .await;

            Mock::given(method("POST"))
                .and(path("/worker/jobs"))
                .respond_with(ResponseTemplate::new(200))
                .mount(mock_server)
                .await;

            // Catch-all for log push (path: /worker/jobs/{id}/logs)
            Mock::given(method("POST"))
                .and(path_regex(r"/worker/jobs/.+/logs"))
                .respond_with(ResponseTemplate::new(200))
                .mount(mock_server)
                .await;

            Mock::given(method("POST"))
                .and(path_regex(r"/worker/jobs/.+/steps/.+/complete"))
                .respond_with(ResponseTemplate::new(200))
                .mount(mock_server)
                .await;

            Mock::given(method("GET"))
                .and(path_regex(r"/worker/jobs/.+/cancelled"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"cancelled": false})),
                )
                .mount(mock_server)
                .await;
        }

        fn make_step(job_id: Uuid, cmd: &str) -> ClaimedStep {
            ClaimedStep {
                job_id,
                workspace: "default".to_string(),
                task_name: "test-task".to_string(),
                step_name: "test-step".to_string(),
                action_name: "test-action".to_string(),
                action_type: "script".to_string(),
                action_image: None,
                action_spec: Some(serde_json::json!({"script": cmd})),
                input: None,
                runner: Some("local".to_string()),
                timeout_secs: None,
                revision: None,
                agent_provider_name: None,
                agent_prompt: None,
                agent_system_prompt: None,
                mcp_servers: None,
                agent_state: None,
                agent_tool_tasks: None,
                event_source_config: None,
                state_storage_key: None,
                state_has_json: None,
                global_state_storage_key: None,
                global_state_has_json: None,
            }
        }

        #[tokio::test]
        async fn test_execute_claimed_step_happy_path() {
            let mock_server = MockServer::start().await;

            // Workspace tarball endpoint
            Mock::given(method("GET"))
                .and(path("/worker/workspace/default.tar.gz"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(create_test_tarball())
                        .insert_header("X-Revision", "test-rev-1"),
                )
                .mount(&mock_server)
                .await;

            mount_common_mocks(&mock_server).await;

            let client = Arc::new(ServerClient::new(
                &mock_server.uri(),
                "test-token",
                Some(5),
                Some(30),
            ));

            let temp_dir = tempfile::tempdir().unwrap();
            // Create the cache base dir so workspace extraction works
            std::fs::create_dir_all(temp_dir.path()).unwrap();
            let executor = Arc::new(StepExecutor::new());
            let ws_cache = Arc::new(WorkspaceCache::new(temp_dir.path().to_str().unwrap(), None));

            let job_id = Uuid::new_v4();
            let step = make_step(job_id, "echo hello");

            // Should complete without panicking
            execute_claimed_step(
                client,
                executor,
                ws_cache,
                step,
                Uuid::new_v4(),
                #[cfg(feature = "agent")]
                None,
            )
            .await;

            // Verify the complete endpoint was called (mock will fail test if not matched
            // when verify_received_requests is configured; here we rely on no panic)
        }

        #[tokio::test]
        async fn test_execute_claimed_step_workspace_download_failure() {
            let mock_server = MockServer::start().await;

            // Workspace endpoint returns 500
            Mock::given(method("GET"))
                .and(path("/worker/workspace/default.tar.gz"))
                .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
                .mount(&mock_server)
                .await;

            // Log push (error message pushed before complete)
            Mock::given(method("POST"))
                .and(path_regex(r"/worker/jobs/.+/logs"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&mock_server)
                .await;

            // Complete endpoint must be called with error
            Mock::given(method("POST"))
                .and(path_regex(r"/worker/jobs/.+/steps/.+/complete"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&mock_server)
                .await;

            let client = Arc::new(ServerClient::new(
                &mock_server.uri(),
                "test-token",
                Some(5),
                Some(30),
            ));

            let temp_dir = tempfile::tempdir().unwrap();
            let executor = Arc::new(StepExecutor::new());
            let ws_cache = Arc::new(WorkspaceCache::new(temp_dir.path().to_str().unwrap(), None));

            let job_id = Uuid::new_v4();
            let step = make_step(job_id, "echo hello");

            execute_claimed_step(
                client,
                executor,
                ws_cache,
                step,
                Uuid::new_v4(),
                #[cfg(feature = "agent")]
                None,
            )
            .await;

            // wiremock asserts `expect(1)` is satisfied on drop
        }

        #[tokio::test]
        async fn test_execute_claimed_step_command_failure() {
            let mock_server = MockServer::start().await;

            // Workspace tarball
            Mock::given(method("GET"))
                .and(path("/worker/workspace/default.tar.gz"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(create_test_tarball())
                        .insert_header("X-Revision", "test-rev-2"),
                )
                .mount(&mock_server)
                .await;

            mount_common_mocks(&mock_server).await;

            let client = Arc::new(ServerClient::new(
                &mock_server.uri(),
                "test-token",
                Some(5),
                Some(30),
            ));

            let temp_dir = tempfile::tempdir().unwrap();
            let executor = Arc::new(StepExecutor::new());
            let ws_cache = Arc::new(WorkspaceCache::new(temp_dir.path().to_str().unwrap(), None));

            let job_id = Uuid::new_v4();
            // "exit 1" causes non-zero exit code
            let step = make_step(job_id, "exit 1");

            // Should complete without panicking even on command failure
            execute_claimed_step(
                client,
                executor,
                ws_cache,
                step,
                Uuid::new_v4(),
                #[cfg(feature = "agent")]
                None,
            )
            .await;
        }

        #[tokio::test]
        async fn test_execute_claimed_step_with_pinned_revision() {
            let mock_server = MockServer::start().await;

            // Workspace tarball endpoint — must accept ?revision= query param
            Mock::given(method("GET"))
                .and(path("/worker/workspace/default.tar.gz"))
                .and(query_param("revision", "pinned-rev-123"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(create_test_tarball())
                        .insert_header("X-Revision", "pinned-rev-123"),
                )
                .expect(1)
                .mount(&mock_server)
                .await;

            mount_common_mocks(&mock_server).await;

            let client = Arc::new(ServerClient::new(
                &mock_server.uri(),
                "test-token",
                Some(5),
                Some(30),
            ));

            let temp_dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(temp_dir.path()).unwrap();
            let executor = Arc::new(StepExecutor::new());
            let ws_cache = Arc::new(WorkspaceCache::new(temp_dir.path().to_str().unwrap(), None));

            let job_id = Uuid::new_v4();
            let mut step = make_step(job_id, "echo hello");
            step.revision = Some("pinned-rev-123".to_string());

            // Should complete successfully, using the pinned revision endpoint
            execute_claimed_step(
                client,
                executor,
                ws_cache,
                step,
                Uuid::new_v4(),
                #[cfg(feature = "agent")]
                None,
            )
            .await;

            // wiremock verifies the ?revision= query param was sent (via expect(1))
        }
    }
}
