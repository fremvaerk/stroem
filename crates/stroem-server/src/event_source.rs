use crate::state::{AliveGuard, AppState};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use stroem_common::models::workflow::{RestartPolicy, TriggerDef};
use stroem_common::template::render_env_map;
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
    /// Consumer task name — the task whose steps run the long-lived queue process.
    task: String,
    /// Target task — where emitted JSON-line events create new jobs.
    target_task: String,
    /// Resolved (Tera-rendered) environment variables for the consumer job.
    env: HashMap<String, String>,
    /// Default input values merged into every emitted job.
    input_defaults: HashMap<String, serde_json::Value>,
    restart_policy: RestartPolicy,
    /// Initial restart delay in seconds. Stored here so `create_event_source_job`
    /// can embed it in the job metadata for the worker.
    backoff_secs: u64,
    max_in_flight: Option<u32>,
    fingerprint: String,
}

/// Reconcile desired vs. active event source jobs.
///
/// 1. Collect all enabled EventSource triggers from all workspaces.
/// 2. Load all active event source jobs from the database.
/// 3. For each desired trigger:
///    - If no active job: create one.
///    - If active job exists but fingerprint changed: cancel old, create new.
/// 4. For any active job whose trigger no longer exists: cancel it.
/// 5. Restart recently completed/failed event source jobs according to their
///    restart policy.
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

    // Build a lookup: source_id -> Vec<(job_id, stored_fingerprint)>
    // source_id format: "{workspace}/{trigger_name}"
    // Multiple active jobs for the same source_id can occur if a previous
    // reconcile cycle created a job before the old one was fully cancelled.
    let mut active_by_source: HashMap<String, Vec<(Uuid, String)>> = HashMap::new();
    for (job_id, source_id, fingerprint) in active {
        active_by_source
            .entry(source_id)
            .or_default()
            .push((job_id, fingerprint));
    }

    // Build set of desired source_ids for the stale-detection pass
    let desired_source_ids: std::collections::HashSet<String> = desired
        .iter()
        .map(|d| format!("{}/{}", d.workspace, d.trigger_name))
        .collect();

    // --- Pre-load terminal jobs (used in both step 3 and step 5) ---
    //
    // We load terminal rows up front so that step 3 can consult them before
    // deciding whether to create a new job. Without this check, step 3 would
    // unconditionally create a job whenever there is no active one, bypassing
    // the restart policy for sources whose jobs recently terminated.
    let terminal_rows = match load_terminal_event_source_jobs(state).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(
                "EventSourceManager: failed to load terminal event source jobs: {:#}",
                e
            );
            return;
        }
    };

    // Build a lookup: source_id → most-recent terminal job row. We keep only
    // the first (most-recent) row per source because the query is ORDER BY
    // created_at DESC.
    let mut terminal_by_source: HashMap<String, &EventSourceJobRow> = HashMap::new();
    for row in &terminal_rows {
        let sid = row.source_id.as_deref().unwrap_or("");
        terminal_by_source.entry(sid.to_string()).or_insert(row);
    }

    // --- Step 3: ensure each desired trigger has a current job ---
    for des in &desired {
        let source_id = format!("{}/{}", des.workspace, des.trigger_name);

        match active_by_source.get(&source_id) {
            None => {
                // No active job. Only create a new one if there is no recent
                // terminal job for this source — that means this is a
                // brand-new trigger with no history. Triggers that have
                // recently terminated are handled exclusively by step 5 (the
                // restart loop), which applies backoff and policy checks.
                if terminal_by_source.contains_key(&source_id) {
                    // A recent terminal job exists — let step 5 decide whether
                    // and when to restart according to the restart policy.
                    continue;
                }
                // No terminal job in the recent window: fresh trigger. Create.
                tracing::info!(
                    "EventSourceManager: creating job for event source '{}'",
                    source_id
                );
                if let Err(e) = create_event_source_job(state, des, 0).await {
                    tracing::error!(
                        "EventSourceManager: failed to create job for '{}': {:#}",
                        source_id,
                        e
                    );
                }
            }
            Some(jobs) => {
                // Cancel any duplicate jobs beyond the first matching one.
                // If there is a job with the current fingerprint, keep it and
                // cancel all others; otherwise keep the first and cancel the rest.
                let matching_pos = jobs.iter().position(|(_, fp)| fp == &des.fingerprint);

                let keep_idx = matching_pos.unwrap_or(0);
                let (keep_job_id, keep_fp) = &jobs[keep_idx];

                // Cancel all duplicate jobs (all except the one we're keeping).
                for (i, (dup_id, _)) in jobs.iter().enumerate() {
                    if i == keep_idx {
                        continue;
                    }
                    tracing::warn!(
                        "EventSourceManager: cancelling duplicate job {} for '{}'",
                        dup_id,
                        source_id
                    );
                    if let Err(e) = crate::cancellation::cancel_job(state, *dup_id).await {
                        tracing::warn!(
                            "EventSourceManager: failed to cancel duplicate job {} for '{}': {:#}",
                            dup_id,
                            source_id,
                            e
                        );
                    }
                }

                // Now handle the kept job.
                if keep_fp != &des.fingerprint {
                    // Config changed — cancel the kept job and create a new one.
                    tracing::info!(
                        "EventSourceManager: config changed for '{}', cancelling job {} and recreating",
                        source_id,
                        keep_job_id
                    );
                    if let Err(e) = crate::cancellation::cancel_job(state, *keep_job_id).await {
                        tracing::warn!(
                            "EventSourceManager: failed to cancel old job {} for '{}': {:#}",
                            keep_job_id,
                            source_id,
                            e
                        );
                        // Continue to create the new job even if cancellation failed.
                    }
                    if let Err(e) = create_event_source_job(state, des, 0).await {
                        tracing::error!(
                            "EventSourceManager: failed to create replacement job for '{}': {:#}",
                            source_id,
                            e
                        );
                    }
                }
                // else: job exists with matching fingerprint — nothing to do.
            }
        }
    }

    // --- Step 4: cancel ALL jobs for removed/disabled triggers ---
    for (source_id, jobs) in &active_by_source {
        if !desired_source_ids.contains(source_id) {
            for (job_id, _) in jobs {
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

    // --- Step 5: restart recently completed/failed event source jobs ---
    // `terminal_rows` was already loaded before step 3.
    for row in terminal_rows {
        let source_id = row.source_id.unwrap_or_default();
        if !desired_source_ids.contains(&source_id) {
            // Trigger removed or disabled — do not restart.
            continue;
        }

        // Skip if there is already an active job for this source.
        if active_by_source.contains_key(&source_id) {
            continue;
        }

        let des = desired
            .iter()
            .find(|d| format!("{}/{}", d.workspace, d.trigger_name) == source_id);
        let Some(des) = des else { continue };

        let job_failed = row
            .status
            .as_deref()
            .map(|s| s == "failed")
            .unwrap_or(false);

        let should_restart = match des.restart_policy {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => job_failed,
            RestartPolicy::Never => false,
        };

        if !should_restart {
            continue;
        }

        let consecutive_failures = row
            .input
            .as_ref()
            .and_then(|v| v.get("_event_source"))
            .and_then(|v| v.get("consecutive_failures"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Reset the failure counter on a successful exit; increment on failure.
        let next_failures = if job_failed {
            consecutive_failures + 1
        } else {
            0
        };

        // Compute the required backoff delay before restarting.
        // Successful exits use a minimal 1-second delay; failures use
        // exponential backoff capped at 300 seconds (5 minutes).
        let delay_secs: u64 = if next_failures == 0 {
            1
        } else {
            let exp = (next_failures - 1).min(10);
            des.backoff_secs
                .saturating_mul(2u64.saturating_pow(exp as u32))
                .min(300)
        };

        // Skip this cycle if not enough time has elapsed since the job ended.
        if let Some(completed_at) = row.completed_at {
            let elapsed = chrono::Utc::now() - completed_at;
            if elapsed.num_seconds() < delay_secs as i64 {
                tracing::debug!(
                    "EventSourceManager: backoff for '{}' — waiting {}s (elapsed {}s)",
                    source_id,
                    delay_secs,
                    elapsed.num_seconds(),
                );
                continue;
            }
        }

        tracing::info!(
            "EventSourceManager: restarting event source '{}' (consecutive_failures={})",
            source_id,
            next_failures
        );

        if let Err(e) = create_event_source_job(state, des, next_failures).await {
            tracing::error!(
                "EventSourceManager: failed to restart job for '{}': {:#}",
                source_id,
                e
            );
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
            let (task, target_task, input, env, restart_policy, backoff_secs, max_in_flight) =
                match trigger_def {
                    TriggerDef::EventSource {
                        task,
                        target_task,
                        enabled,
                        input,
                        env,
                        restart_policy,
                        backoff_secs,
                        max_in_flight,
                    } if *enabled => (
                        task.clone(),
                        target_task.clone(),
                        input.clone(),
                        env.clone(),
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
                &task,
                &target_task,
                &resolved_env,
                &input,
                restart_policy,
                backoff_secs,
                max_in_flight,
            );

            desired.push(DesiredEventSource {
                workspace: ws_name.to_string(),
                trigger_name: trigger_name.clone(),
                task,
                target_task,
                env: resolved_env,
                input_defaults: input,
                restart_policy,
                backoff_secs,
                max_in_flight,
                fingerprint,
            });
        }
    }

    desired
}

/// Compute a SHA-256 fingerprint of the significant config fields of an event
/// source trigger. The fingerprint is used to detect config changes so that
/// stale jobs can be replaced.
///
/// Every field that affects job behaviour is included so that changing any
/// single field (including `task`, `target_task`, `input`, `restart_policy`,
/// `backoff_secs`, and `max_in_flight`) causes the old job to be replaced.
fn compute_fingerprint(
    task: &str,
    target_task: &str,
    env: &HashMap<String, String>,
    input: &HashMap<String, serde_json::Value>,
    restart_policy: RestartPolicy,
    backoff_secs: u64,
    max_in_flight: Option<u32>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(task);
    hasher.update("|");
    hasher.update(target_task);
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
    // Sort input keys for stability.
    let mut input_pairs: Vec<(&String, &serde_json::Value)> = input.iter().collect();
    input_pairs.sort_by_key(|(k, _)| *k);
    for (k, v) in input_pairs {
        hasher.update(k.as_bytes());
        hasher.update("=");
        hasher.update(v.to_string());
        hasher.update(";");
    }
    hasher.update("|");
    hasher.update(restart_policy_str(restart_policy));
    hasher.update("|");
    hasher.update(backoff_secs.to_string());
    hasher.update("|");
    hasher.update(
        max_in_flight
            .map(|v| v.to_string())
            .unwrap_or_default()
            .as_str(),
    );
    hex::encode(hasher.finalize())
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
    pub status: Option<String>,
    /// Best-effort timestamp of when the job reached a terminal state.
    /// Used to enforce backoff delay before restarting.
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Load all active (pending/running) event source jobs from the database.
///
/// Returns a list of `(job_id, source_id, fingerprint)` tuples. The
/// fingerprint is read from the job's `input` JSON field under the key
/// `"_fingerprint"`.
async fn load_active_event_source_jobs(state: &AppState) -> Result<Vec<(Uuid, String, String)>> {
    let rows = sqlx::query_as::<_, EventSourceJobRow>(
        "SELECT job_id, source_id, input, status, NULL::timestamptz as completed_at \
         FROM job \
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

/// Load recently completed or failed event source jobs from the database.
///
/// Only jobs that terminated within the last 10 minutes are returned, which
/// bounds the result set and avoids rescanning old history on every cycle.
async fn load_terminal_event_source_jobs(state: &AppState) -> Result<Vec<EventSourceJobRow>> {
    sqlx::query_as::<_, EventSourceJobRow>(
        "SELECT job_id, source_id, input, status, \
                COALESCE(completed_at, created_at) as completed_at \
         FROM job \
         WHERE source_type = 'event_source' \
           AND status IN ('completed', 'failed') \
           AND COALESCE(completed_at, created_at) > NOW() - INTERVAL '10 minutes' \
         ORDER BY created_at DESC",
    )
    .fetch_all(&state.pool)
    .await
    .context("Failed to load terminal event source jobs")
}

/// Create an event source consumer job via `create_job_for_task()`.
///
/// The job input carries event source metadata under `_event_source` so that
/// the worker and emit endpoint can identify the source trigger and its config.
/// `consecutive_failures` is threaded through restarts so callers can apply
/// backoff logic based on the failure history.
async fn create_event_source_job(
    state: &AppState,
    des: &DesiredEventSource,
    consecutive_failures: u64,
) -> Result<()> {
    let source_id = format!("{}/{}", des.workspace, des.trigger_name);

    let workspace_config = state
        .workspaces
        .get_config(&des.workspace)
        .await
        .with_context(|| {
            format!(
                "Workspace config not found for event source '{}'",
                source_id
            )
        })?;

    let revision = state.workspaces.get_revision(&des.workspace);

    // TODO: The `_event_source` and `_fingerprint` metadata keys are stored in
    // the job input so that the claim handler and reconciler can read them back.
    // This means they are also visible in Tera template context for downstream
    // steps. A future improvement is to resolve env at claim time (rather than
    // embedding it here) and store metadata out-of-band so it never leaks into
    // template rendering. For now the env values reflect the Tera-rendered
    // trigger env (with secrets substituted on the server side), which is
    // equivalent to how regular step inputs can carry rendered secret values.
    let job_input = serde_json::json!({
        "_fingerprint": des.fingerprint,
        "_event_source": {
            "target_task": des.target_task,
            "source_id": source_id,
            "input_defaults": des.input_defaults,
            "max_in_flight": des.max_in_flight,
            "env": des.env,
            "backoff_secs": des.backoff_secs,
            "consecutive_failures": consecutive_failures,
        }
    });

    let job_id = crate::job_creator::create_job_for_task(
        &state.pool,
        &workspace_config,
        &des.workspace,
        &des.task,
        job_input,
        "event_source",
        Some(&source_id),
        revision.as_deref(),
        None,
        state.config.agents.as_ref(),
    )
    .await
    .with_context(|| {
        format!(
            "Failed to create consumer job for event source '{}'",
            source_id
        )
    })?;

    tracing::info!(
        "EventSourceManager: created job {} for event source '{}' (consumer task='{}', target_task='{}')",
        job_id,
        source_id,
        des.task,
        des.target_task,
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stroem_common::models::workflow::RestartPolicy;

    /// Helper that calls `compute_fingerprint` with a complete, sensible set of
    /// defaults so individual tests only need to override the field they care about.
    fn fp(
        task: &str,
        target_task: &str,
        env: &HashMap<String, String>,
        input: &HashMap<String, serde_json::Value>,
        restart_policy: RestartPolicy,
        backoff_secs: u64,
        max_in_flight: Option<u32>,
    ) -> String {
        compute_fingerprint(
            task,
            target_task,
            env,
            input,
            restart_policy,
            backoff_secs,
            max_in_flight,
        )
    }

    fn default_fp() -> String {
        fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            None,
        )
    }

    #[test]
    fn test_fingerprint_determinism() {
        let a = default_fp();
        let b = default_fp();
        assert_eq!(a, b, "same inputs must produce the same fingerprint");
    }

    #[test]
    fn test_fingerprint_order_independence_env() {
        let mut env1 = HashMap::new();
        env1.insert("FOO".to_string(), "1".to_string());
        env1.insert("BAR".to_string(), "2".to_string());

        let mut env2 = HashMap::new();
        env2.insert("BAR".to_string(), "2".to_string());
        env2.insert("FOO".to_string(), "1".to_string());

        let h1 = fp(
            "my-consumer",
            "my-target",
            &env1,
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            None,
        );
        let h2 = fp(
            "my-consumer",
            "my-target",
            &env2,
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            None,
        );
        assert_eq!(h1, h2, "env insertion order must not affect fingerprint");
    }

    #[test]
    fn test_fingerprint_order_independence_input() {
        let mut input1 = HashMap::new();
        input1.insert("key_a".to_string(), serde_json::json!("alpha"));
        input1.insert("key_b".to_string(), serde_json::json!("beta"));

        let mut input2 = HashMap::new();
        input2.insert("key_b".to_string(), serde_json::json!("beta"));
        input2.insert("key_a".to_string(), serde_json::json!("alpha"));

        let h1 = fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &input1,
            RestartPolicy::Always,
            5,
            None,
        );
        let h2 = fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &input2,
            RestartPolicy::Always,
            5,
            None,
        );
        assert_eq!(h1, h2, "input insertion order must not affect fingerprint");
    }

    #[test]
    fn test_fingerprint_sensitivity_to_each_field() {
        let base = default_fp();

        // Changing task
        let changed_task = fp(
            "other-consumer",
            "my-target",
            &HashMap::new(),
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            None,
        );
        assert_ne!(base, changed_task, "task change must change fingerprint");

        // Changing target_task
        let changed_target = fp(
            "my-consumer",
            "other-target",
            &HashMap::new(),
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            None,
        );
        assert_ne!(
            base, changed_target,
            "target_task change must change fingerprint"
        );

        // Changing env
        let mut env = HashMap::new();
        env.insert("NEW_VAR".to_string(), "val".to_string());
        let changed_env = fp(
            "my-consumer",
            "my-target",
            &env,
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            None,
        );
        assert_ne!(base, changed_env, "env change must change fingerprint");

        // Changing input
        let mut input_map = HashMap::new();
        input_map.insert("key".to_string(), serde_json::json!("value"));
        let changed_input = fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &input_map,
            RestartPolicy::Always,
            5,
            None,
        );
        assert_ne!(base, changed_input, "input change must change fingerprint");

        // Changing restart_policy
        let changed_policy = fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &HashMap::new(),
            RestartPolicy::Never,
            5,
            None,
        );
        assert_ne!(
            base, changed_policy,
            "restart_policy change must change fingerprint"
        );

        // Changing backoff_secs
        let changed_backoff = fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &HashMap::new(),
            RestartPolicy::Always,
            10,
            None,
        );
        assert_ne!(
            base, changed_backoff,
            "backoff_secs change must change fingerprint"
        );

        // Changing max_in_flight
        let changed_max = fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            Some(3),
        );
        assert_ne!(
            base, changed_max,
            "max_in_flight change must change fingerprint"
        );
    }

    #[test]
    fn test_fingerprint_none_vs_some_zero_max_in_flight() {
        // max_in_flight: None vs Some(0) must differ.
        let mif_none = fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            None,
        );
        let mif_zero = fp(
            "my-consumer",
            "my-target",
            &HashMap::new(),
            &HashMap::new(),
            RestartPolicy::Always,
            5,
            Some(0),
        );
        assert_ne!(
            mif_none, mif_zero,
            "max_in_flight None and Some(0) must produce different fingerprints"
        );
    }

    #[test]
    fn test_restart_policy_str() {
        assert_eq!(restart_policy_str(RestartPolicy::Always), "always");
        assert_eq!(restart_policy_str(RestartPolicy::OnFailure), "on_failure");
        assert_eq!(restart_policy_str(RestartPolicy::Never), "never");
    }
}
