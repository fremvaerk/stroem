use crate::state::{AliveGuard, AppState};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use stroem_common::models::workflow::{RestartPolicy, TriggerDef};
use stroem_common::template::render_env_map;
use stroem_db::{JobRepo, JobStepRepo, NewJobStep};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Interval between reconciliation passes (seconds).
const RECONCILE_INTERVAL_SECS: u64 = 30;

/// Spawn the event source manager background task.
///
/// On each reconcile cycle the manager ensures that exactly one active event
/// source job exists for every enabled `EventSource` trigger. When a trigger's
/// config changes the old job is cancelled and a fresh one is created; when a
/// trigger is removed its job is cancelled.
pub fn start(state: AppState, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _guard = AliveGuard::new(state.background_tasks.event_source_alive.clone());
        run_loop(state, cancel).await;
    })
}

async fn run_loop(state: AppState, cancel: CancellationToken) {
    tracing::info!("EventSourceManager started");

    loop {
        reconcile(&state).await;

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(RECONCILE_INTERVAL_SECS)) => {}
            _ = cancel.cancelled() => {
                tracing::info!("EventSourceManager shutting down");
                return;
            }
        }
    }
}

/// A desired event source derived from workspace config.
struct DesiredEventSource {
    workspace: String,
    trigger_name: String,
    task: String,
    fingerprint: String,
    action_spec: serde_json::Value,
}

/// Reconcile desired vs. active event source jobs.
///
/// 1. Collect all enabled EventSource triggers from all workspaces.
/// 2. Load all active event source jobs from the database.
/// 3. For each desired trigger:
///    - If no active job: create one.
///    - If active job exists but fingerprint changed: cancel old, create new.
/// 4. For any active job whose trigger no longer exists: cancel it.
#[tracing::instrument(skip(state))]
async fn reconcile(state: &AppState) {
    // --- Step 1: collect desired event sources ---
    let desired = collect_desired(state).await;

    // --- Step 2: load all active event source jobs ---
    let active = match load_active_event_source_jobs(state).await {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::error!("EventSourceManager: failed to load active jobs: {:#}", e);
            return;
        }
    };

    // Build a lookup: source_id -> (job_id, stored_fingerprint)
    // source_id format: "{workspace}/{trigger_name}"
    let mut active_by_source: HashMap<String, (Uuid, String)> = HashMap::new();
    for (job_id, source_id, fingerprint) in active {
        active_by_source.insert(source_id, (job_id, fingerprint));
    }

    // Build set of desired source_ids for the stale-detection pass
    let desired_source_ids: std::collections::HashSet<String> = desired
        .iter()
        .map(|d| format!("{}/{}", d.workspace, d.trigger_name))
        .collect();

    // --- Step 3: ensure each desired trigger has a current job ---
    for des in &desired {
        let source_id = format!("{}/{}", des.workspace, des.trigger_name);

        match active_by_source.get(&source_id) {
            None => {
                // No active job — create one.
                tracing::info!(
                    "EventSourceManager: creating job for event source '{}'",
                    source_id
                );
                if let Err(e) = create_event_source_job(state, des).await {
                    tracing::error!(
                        "EventSourceManager: failed to create job for '{}': {:#}",
                        source_id,
                        e
                    );
                }
            }
            Some((job_id, stored_fp)) if stored_fp != &des.fingerprint => {
                // Config changed — cancel old job, create a new one.
                tracing::info!(
                    "EventSourceManager: config changed for '{}', cancelling job {} and recreating",
                    source_id,
                    job_id
                );
                if let Err(e) = crate::cancellation::cancel_job(state, *job_id).await {
                    tracing::warn!(
                        "EventSourceManager: failed to cancel old job {} for '{}': {:#}",
                        job_id,
                        source_id,
                        e
                    );
                    // Continue to create the new job even if cancellation failed.
                }
                if let Err(e) = create_event_source_job(state, des).await {
                    tracing::error!(
                        "EventSourceManager: failed to create replacement job for '{}': {:#}",
                        source_id,
                        e
                    );
                }
            }
            Some(_) => {
                // Job exists with matching fingerprint — nothing to do.
            }
        }
    }

    // --- Step 4: cancel jobs for removed/disabled triggers ---
    for (source_id, (job_id, _)) in &active_by_source {
        if !desired_source_ids.contains(source_id) {
            tracing::info!(
                "EventSourceManager: trigger '{}' removed/disabled, cancelling job {}",
                source_id,
                job_id
            );
            if let Err(e) = crate::cancellation::cancel_job(state, *job_id).await {
                tracing::warn!(
                    "EventSourceManager: failed to cancel stale job {} for '{}': {:#}",
                    job_id,
                    source_id,
                    e
                );
            }
        }
    }
}

/// Collect all enabled EventSource triggers from all workspaces, building
/// `DesiredEventSource` entries with resolved env and computed fingerprints.
async fn collect_desired(state: &AppState) -> Vec<DesiredEventSource> {
    let mut desired = Vec::new();

    for ws_name in state.workspaces.names() {
        let config = match state.workspaces.get_config(ws_name).await {
            Some(c) => c,
            None => continue,
        };

        let secrets_ctx = serde_json::json!({ "secret": config.secrets });

        for (trigger_name, trigger_def) in &config.triggers {
            let (
                task,
                input,
                env,
                script,
                image,
                runner,
                language,
                dependencies,
                interpreter,
                manifest,
                restart_policy,
                backoff_secs,
                max_in_flight,
            ) = match trigger_def {
                TriggerDef::EventSource {
                    task,
                    enabled,
                    input,
                    env,
                    script,
                    image,
                    runner,
                    language,
                    dependencies,
                    interpreter,
                    manifest,
                    restart_policy,
                    backoff_secs,
                    max_in_flight,
                } if *enabled => (
                    task.clone(),
                    input.clone(),
                    env.clone(),
                    script.clone(),
                    image.clone(),
                    runner.clone(),
                    language.clone(),
                    dependencies.clone(),
                    interpreter.clone(),
                    manifest.clone(),
                    *restart_policy,
                    *backoff_secs,
                    *max_in_flight,
                ),
                _ => continue,
            };

            // Resolve Tera templates in env values (workspace secrets available).
            let resolved_env = match render_env_map(&env, &secrets_ctx) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        "EventSourceManager: failed to render env for '{}/{}': {:#}",
                        ws_name,
                        trigger_name,
                        e
                    );
                    continue;
                }
            };

            let fingerprint = compute_fingerprint(
                script.as_deref(),
                image.as_deref(),
                runner.as_deref(),
                language.as_deref(),
                &dependencies,
                interpreter.as_deref(),
                &resolved_env,
                manifest.as_ref(),
            );

            let runner_str = runner.clone().unwrap_or_else(|| "local".to_string());

            let action_spec = serde_json::json!({
                "workspace": ws_name,
                "target_task": task,
                "script": script,
                "image": image,
                "runner": runner_str,
                "language": language,
                "dependencies": dependencies,
                "interpreter": interpreter,
                "env": resolved_env,
                "input_defaults": input,
                "restart_policy": restart_policy_str(restart_policy),
                "backoff_secs": backoff_secs,
                "max_in_flight": max_in_flight,
                "manifest": manifest,
            });

            desired.push(DesiredEventSource {
                workspace: ws_name.to_string(),
                trigger_name: trigger_name.clone(),
                task,
                fingerprint,
                action_spec,
            });
        }
    }

    desired
}

/// Compute a SHA-256 fingerprint of the significant config fields of an event
/// source trigger. The fingerprint is used to detect config changes so that
/// stale jobs can be replaced.
#[allow(clippy::too_many_arguments)]
fn compute_fingerprint(
    script: Option<&str>,
    image: Option<&str>,
    runner: Option<&str>,
    language: Option<&str>,
    dependencies: &[String],
    interpreter: Option<&str>,
    env: &HashMap<String, String>,
    manifest: Option<&serde_json::Value>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(script.unwrap_or(""));
    hasher.update("|");
    hasher.update(image.unwrap_or(""));
    hasher.update("|");
    hasher.update(runner.unwrap_or(""));
    hasher.update("|");
    hasher.update(language.unwrap_or(""));
    hasher.update("|");
    // Sort dependencies for stability.
    let mut sorted_deps = dependencies.to_vec();
    sorted_deps.sort_unstable();
    hasher.update(sorted_deps.join(","));
    hasher.update("|");
    hasher.update(interpreter.unwrap_or(""));
    hasher.update("|");
    // Sort env keys for stability.
    let mut env_pairs: Vec<(&String, &String)> = env.iter().collect();
    env_pairs.sort_by_key(|(k, _)| *k);
    for (k, v) in env_pairs {
        hasher.update(k.as_bytes());
        hasher.update("=");
        hasher.update(v.as_bytes());
        hasher.update(";");
    }
    hasher.update("|");
    if let Some(m) = manifest {
        hasher.update(m.to_string());
    }
    format!("{:x}", hasher.finalize())
}

fn restart_policy_str(policy: RestartPolicy) -> &'static str {
    match policy {
        RestartPolicy::Always => "always",
        RestartPolicy::OnFailure => "on_failure",
        RestartPolicy::Never => "never",
    }
}

/// Minimal projection of a job row needed for event source reconciliation.
#[derive(Debug, sqlx::FromRow)]
struct EventSourceJobRow {
    pub job_id: Uuid,
    pub source_id: Option<String>,
    pub input: Option<serde_json::Value>,
}

/// Load all active (pending/running) event source jobs from the database.
///
/// Returns a list of `(job_id, source_id, fingerprint)` tuples. The
/// fingerprint is read from the job's `input` JSON field under the key
/// `"_fingerprint"`.
async fn load_active_event_source_jobs(state: &AppState) -> Result<Vec<(Uuid, String, String)>> {
    let rows = sqlx::query_as::<_, EventSourceJobRow>(
        "SELECT job_id, source_id, input FROM job \
         WHERE source_type = 'event_source' \
           AND status NOT IN ('completed', 'failed', 'cancelled', 'skipped')",
    )
    .fetch_all(&state.pool)
    .await
    .context("Failed to load active event source jobs")?;

    let result = rows
        .into_iter()
        .map(|row| {
            let source_id = row.source_id.unwrap_or_default();
            let fingerprint = row
                .input
                .as_ref()
                .and_then(|v| v.get("_fingerprint"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (row.job_id, source_id, fingerprint)
        })
        .collect();

    Ok(result)
}

/// Create an event source job and its single step in a transaction.
async fn create_event_source_job(state: &AppState, des: &DesiredEventSource) -> Result<()> {
    let source_id = format!("{}/{}", des.workspace, des.trigger_name);
    let task_name = format!("_event_source:{}", des.trigger_name);
    let job_id = Uuid::new_v4();

    let input = serde_json::json!({
        "_fingerprint": des.fingerprint,
    });

    let step = NewJobStep {
        job_id,
        step_name: "event_source".to_string(),
        action_name: des.trigger_name.clone(),
        action_type: "event_source".to_string(),
        action_image: des
            .action_spec
            .get("image")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        action_spec: Some(des.action_spec.clone()),
        input: None,
        status: "ready".to_string(),
        required_tags: vec!["event_source".to_string()],
        runner: des
            .action_spec
            .get("runner")
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string(),
        timeout_secs: None,
        when_condition: None,
        for_each_expr: None,
        loop_source: None,
        loop_index: None,
        loop_total: None,
        loop_item: None,
    };

    let mut tx = state
        .pool
        .begin()
        .await
        .context("Failed to begin transaction for event source job")?;

    JobRepo::create_with_parent_tx_id(
        &mut *tx,
        job_id,
        &des.workspace,
        &task_name,
        "distributed",
        Some(input),
        "event_source",
        Some(&source_id),
        None,
        None,
        None,
        None,
    )
    .await
    .context("Failed to create event source job")?;

    JobStepRepo::create_steps_tx(&mut *tx, &[step])
        .await
        .context("Failed to create event source step")?;

    tx.commit()
        .await
        .context("Failed to commit event source job creation")?;

    tracing::info!(
        "EventSourceManager: created job {} for event source '{}' -> task '{}'",
        job_id,
        source_id,
        des.task
    );

    Ok(())
}
