use crate::job_creator::create_job_for_task;
use crate::state::{AliveGuard, AppState};
use crate::workspace::WorkspaceManager;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::parser::{CronParser, Seconds};
use croner::Cron;
use std::collections::HashMap;
use stroem_common::models::workflow::ConcurrencyPolicy;
use stroem_db::JobRepo;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Key that uniquely identifies a trigger: "{workspace}/{trigger_name}"
type TriggerKey = String;

/// State tracked for each active trigger
struct TriggerState {
    cron: Cron,
    cron_expr: String,
    workspace: String,
    task: String,
    input: HashMap<String, serde_json::Value>,
    trigger_name: String,
    concurrency: ConcurrencyPolicy,
    /// Parsed IANA timezone. None means UTC.
    timezone: Option<Tz>,
    /// The raw timezone string from config (for hot-reload comparison).
    timezone_str: Option<String>,
    last_run: Option<DateTime<Utc>>,
    next_run: Option<DateTime<Utc>>,
}

/// Spawn the scheduler background task.
///
/// Iterates triggers across all workspaces, computes smart sleep intervals,
/// and fires jobs at the correct time. Supports config hot-reload (preserving
/// last_run state for unchanged triggers) and clean shutdown via CancellationToken.
pub fn start(state: AppState, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _guard = AliveGuard::new(state.background_tasks.scheduler_alive.clone());
        run_loop(state, cancel).await;
    })
}

async fn run_loop(state: AppState, cancel: CancellationToken) {
    let workspaces = &state.workspaces;
    tracing::info!("Scheduler started");

    let mut triggers = load_triggers(workspaces, None).await;
    tracing::info!("Scheduler loaded {} trigger(s)", triggers.len());

    loop {
        let now = Utc::now();

        // Compute next_run for triggers that need it, and collect keys to fire
        let mut to_fire = Vec::new();
        for (key, tstate) in triggers.iter_mut() {
            if tstate.next_run.is_none() {
                tstate.next_run =
                    compute_next_run(&tstate.cron, tstate.last_run, now, tstate.timezone);
            }

            if let Some(next) = tstate.next_run {
                if now >= next {
                    to_fire.push(key.clone());
                }
            }
        }

        // Fire due triggers
        for key in &to_fire {
            let tstate = triggers
                .get(key)
                .expect("key came from iterating the triggers map");
            fire_trigger(&state, workspaces, tstate).await;

            let tstate = triggers
                .get_mut(key)
                .expect("key came from iterating the triggers map");
            let now = Utc::now();
            tstate.last_run = Some(now);
            tstate.next_run = compute_next_run(&tstate.cron, Some(now), now, tstate.timezone);
        }

        // Determine sleep duration: minimum time until next trigger fires
        let sleep_duration = triggers
            .values()
            .filter_map(|s| s.next_run)
            .map(|next| {
                let diff = next - Utc::now();
                diff.to_std().unwrap_or(std::time::Duration::from_secs(1))
            })
            .min()
            .unwrap_or(std::time::Duration::from_secs(60));

        tracing::debug!("Scheduler sleeping for {:?}", sleep_duration);

        // Sleep until next trigger or cancellation
        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {},
            _ = cancel.cancelled() => {
                tracing::info!("Scheduler shutting down");
                return;
            }
        }

        // Hot-reload: re-scan workspace configs, preserving state for unchanged triggers
        triggers = load_triggers(workspaces, Some(&triggers)).await;
    }
}

/// Load triggers from all workspaces.
///
/// If `previous` is provided, preserves `last_run` and `next_run` for triggers
/// whose cron expression hasn't changed (hot-reload).
async fn load_triggers(
    workspaces: &WorkspaceManager,
    previous: Option<&HashMap<TriggerKey, TriggerState>>,
) -> HashMap<TriggerKey, TriggerState> {
    let mut triggers = HashMap::new();

    for ws_name in workspaces.names() {
        let config = match workspaces.get_config(ws_name).await {
            Some(c) => c,
            None => continue,
        };

        for (trigger_name, trigger_def) in &config.triggers {
            // Only process enabled scheduler triggers
            let (cron_expr, task, input, concurrency, tz_str) = match trigger_def {
                stroem_common::models::workflow::TriggerDef::Scheduler {
                    cron,
                    task,
                    input,
                    enabled,
                    concurrency,
                    timezone,
                } if *enabled => (
                    cron.clone(),
                    task.clone(),
                    input.clone(),
                    *concurrency,
                    timezone.clone(),
                ),
                _ => continue,
            };

            let cron = match CronParser::builder()
                .seconds(Seconds::Optional)
                .build()
                .parse(&cron_expr)
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "Skipping trigger '{}/{}': invalid cron '{}': {}",
                        ws_name,
                        trigger_name,
                        cron_expr,
                        e
                    );
                    continue;
                }
            };

            // Parse timezone (validated at config load time, but handle gracefully)
            let tz: Option<Tz> = tz_str.as_deref().and_then(|s| match s.parse::<Tz>() {
                Ok(tz) => Some(tz),
                Err(_) => {
                    tracing::warn!(
                        "Trigger '{}/{}': invalid timezone '{}', using UTC",
                        ws_name,
                        trigger_name,
                        s
                    );
                    None
                }
            });

            let key = format!("{}/{}", ws_name, trigger_name);

            // Preserve state from previous cycle if cron expression and timezone unchanged
            let (last_run, next_run) = match previous {
                Some(prev) => match prev.get(&key) {
                    Some(old) if old.cron_expr == cron_expr && old.timezone_str == tz_str => {
                        (old.last_run, old.next_run)
                    }
                    _ => (None, None),
                },
                None => (None, None),
            };

            triggers.insert(
                key,
                TriggerState {
                    cron,
                    cron_expr,
                    workspace: ws_name.to_string(),
                    task,
                    input,
                    trigger_name: trigger_name.clone(),
                    concurrency,
                    timezone: tz,
                    timezone_str: tz_str,
                    last_run,
                    next_run,
                },
            );
        }
    }

    triggers
}

/// Compute the next run time for a trigger.
///
/// When a timezone is provided, converts the start time to the local timezone,
/// finds the next cron occurrence in local time, then converts back to UTC.
/// This ensures DST transitions are handled correctly by the `croner` crate.
fn compute_next_run(
    cron: &Cron,
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    timezone: Option<Tz>,
) -> Option<DateTime<Utc>> {
    // Start searching from the later of last_run or now
    let start = match last_run {
        Some(lr) if lr > now => lr,
        _ => now,
    };

    match timezone {
        Some(tz) => {
            // Convert to local timezone, find next occurrence there, convert back to UTC
            let local_start = start.with_timezone(&tz);
            match cron.find_next_occurrence(&local_start, false) {
                Ok(next_local) => Some(next_local.with_timezone(&Utc)),
                Err(e) => {
                    tracing::warn!("Failed to compute next occurrence (tz={}): {:#}", tz, e);
                    None
                }
            }
        }
        None => {
            // UTC (existing behavior)
            match cron.find_next_occurrence(&start, false) {
                Ok(next) => Some(next),
                Err(e) => {
                    tracing::warn!("Failed to compute next occurrence: {:#}", e);
                    None
                }
            }
        }
    }
}

/// Fire a trigger by creating a job via the shared job_creator.
///
/// Applies concurrency policy before creating the job:
/// - `Allow`: no check (default, existing behavior)
/// - `Skip`: if there's an active job from this trigger, skip firing
/// - `CancelPrevious`: cancel any active jobs from this trigger, then fire
async fn fire_trigger(app_state: &AppState, workspaces: &WorkspaceManager, tstate: &TriggerState) {
    let source_id = format!("{}/{}", tstate.workspace, tstate.trigger_name);
    let input = serde_json::to_value(&tstate.input).unwrap_or_default();

    // Apply concurrency policy
    match tstate.concurrency {
        ConcurrencyPolicy::Skip => {
            match JobRepo::count_active_by_source(&app_state.pool, "trigger", &source_id).await {
                Ok(count) if count > 0 => {
                    tracing::info!(
                        "Trigger '{}': skipping (concurrency=skip, {} active job(s))",
                        source_id,
                        count
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        "Trigger '{}': failed to check active jobs: {:#}",
                        source_id,
                        e
                    );
                    // Proceed anyway — fail-open is safer than skipping silently
                }
                _ => {}
            }
        }
        ConcurrencyPolicy::CancelPrevious => {
            match JobRepo::get_active_job_ids_by_source(&app_state.pool, "trigger", &source_id)
                .await
            {
                Ok(active_job_ids) => {
                    for job_id in &active_job_ids {
                        tracing::info!(
                            "Trigger '{}': cancelling previous job {} (concurrency=cancel_previous)",
                            source_id,
                            job_id
                        );
                        if let Err(e) = crate::cancellation::cancel_job(app_state, *job_id).await {
                            tracing::warn!(
                                "Failed to cancel job {} for trigger '{}': {:#}",
                                job_id,
                                source_id,
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Trigger '{}': failed to get active jobs for cancellation: {:#}",
                        source_id,
                        e
                    );
                }
            }
        }
        ConcurrencyPolicy::Allow => {} // No check needed
    }

    tracing::info!(
        "Scheduler firing trigger '{}' -> task '{}'",
        source_id,
        tstate.task
    );

    let config = match workspaces.get_config(&tstate.workspace).await {
        Some(c) => c,
        None => {
            tracing::error!(
                "Workspace '{}' not found when firing trigger '{}'",
                tstate.workspace,
                source_id
            );
            return;
        }
    };

    match create_job_for_task(
        &app_state.pool,
        &config,
        &tstate.workspace,
        &tstate.task,
        input,
        "trigger",
        Some(&source_id),
    )
    .await
    {
        Ok(job_id) => {
            tracing::info!("Trigger '{}' created job {}", source_id, job_id);
        }
        Err(e) => {
            tracing::error!("Trigger '{}' failed to create job: {:#}", source_id, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use stroem_common::models::workflow::WorkspaceConfig;

    #[test]
    fn test_cron_parse_standard() {
        // 5-field: every minute
        let cron: Result<Cron, _> = "* * * * *".parse();
        assert!(cron.is_ok());
    }

    #[test]
    fn test_cron_parse_with_seconds() {
        // 6-field: every 10 seconds
        let cron = CronParser::builder()
            .seconds(Seconds::Optional)
            .build()
            .parse("*/10 * * * * *");
        assert!(cron.is_ok());
    }

    #[test]
    fn test_cron_invalid_expression() {
        let cron: Result<Cron, _> = "not a cron".parse();
        assert!(cron.is_err());
    }

    #[test]
    fn test_compute_next_run_finds_future_time() {
        let cron: Cron = "* * * * *".parse().unwrap();
        let now = Utc::now();
        let next = compute_next_run(&cron, None, now, None);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn test_compute_next_run_after_last_run() {
        let cron: Cron = "* * * * *".parse().unwrap();
        let now = Utc::now();
        let last_run = Some(now);
        let next = compute_next_run(&cron, last_run, now, None);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[tokio::test]
    async fn test_load_triggers_from_workspace() {
        use stroem_common::models::workflow::{
            ActionDef, FlowStep, TaskDef, TriggerDef, WorkspaceConfig,
        };

        let mut config = WorkspaceConfig::new();
        config.actions.insert(
            "greet".to_string(),
            ActionDef {
                action_type: "script".to_string(),
                name: None,
                description: None,
                task: None,
                cmd: None,
                script: Some("echo hello".to_string()),
                source: None,
                runner: None,
                language: None,
                dependencies: vec![],
                interpreter: None,
                tags: vec![],
                image: None,
                command: None,
                entrypoint: None,
                env: None,
                workdir: None,
                resources: None,
                input: HashMap::new(),
                output: None,
                manifest: None,
            },
        );

        let mut flow = HashMap::new();
        flow.insert(
            "step1".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        config.tasks.insert(
            "hello".to_string(),
            TaskDef {
                name: None,
                description: None,
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow,
                timeout: None,
                on_success: vec![],
                on_error: vec![],
            },
        );
        config.triggers.insert(
            "every-minute".to_string(),
            TriggerDef::Scheduler {
                cron: "* * * * *".to_string(),
                task: "hello".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr = WorkspaceManager::from_config("default", config);
        let triggers = load_triggers(&mgr, None).await;

        assert_eq!(triggers.len(), 1);
        assert!(triggers.contains_key("default/every-minute"));

        let t = &triggers["default/every-minute"];
        assert_eq!(t.workspace, "default");
        assert_eq!(t.task, "hello");
        assert_eq!(t.trigger_name, "every-minute");
        assert_eq!(t.cron_expr, "* * * * *");
    }

    #[tokio::test]
    async fn test_load_triggers_skips_disabled() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "disabled-trigger".to_string(),
            TriggerDef::Scheduler {
                cron: "* * * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: false,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr = WorkspaceManager::from_config("default", config);
        let triggers = load_triggers(&mgr, None).await;
        assert!(triggers.is_empty());
    }

    #[tokio::test]
    async fn test_load_triggers_skips_webhook() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "on-push".to_string(),
            TriggerDef::Webhook {
                name: "on-push".to_string(),
                task: "test".to_string(),
                secret: None,
                input: HashMap::new(),
                enabled: true,
                mode: None,
                timeout_secs: None,
            },
        );

        let mgr = WorkspaceManager::from_config("default", config);
        let triggers = load_triggers(&mgr, None).await;
        assert!(triggers.is_empty());
    }

    #[tokio::test]
    async fn test_load_triggers_preserves_last_run() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 0 2 * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr = WorkspaceManager::from_config("default", config);

        // First load
        let mut triggers = load_triggers(&mgr, None).await;
        assert_eq!(triggers.len(), 1);

        // Simulate a run
        let run_time = Utc::now();
        triggers.get_mut("default/nightly").unwrap().last_run = Some(run_time);

        // Reload (simulating hot-reload) — should preserve last_run
        let reloaded = load_triggers(&mgr, Some(&triggers)).await;
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded["default/nightly"].last_run, Some(run_time));
    }

    #[test]
    fn test_compute_next_run_uses_last_run_when_future() {
        // When last_run is in the future (e.g., clock skew), search from last_run
        let cron: Cron = "* * * * *".parse().unwrap();
        let now = Utc::now();
        let future_last_run = now + chrono::Duration::minutes(5);
        let next = compute_next_run(&cron, Some(future_last_run), now, None);
        assert!(next.is_some());
        // next should be after the future last_run, not after now
        assert!(next.unwrap() > future_last_run);
    }

    #[tokio::test]
    async fn test_load_triggers_skips_invalid_cron() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "bad-cron".to_string(),
            TriggerDef::Scheduler {
                cron: "not valid cron".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr = WorkspaceManager::from_config("default", config);
        let triggers = load_triggers(&mgr, None).await;
        assert!(triggers.is_empty());
    }

    #[tokio::test]
    async fn test_load_triggers_resets_state_on_cron_change() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        // First load with "* * * * *"
        let mut config1 = WorkspaceConfig::new();
        config1.triggers.insert(
            "my-trigger".to_string(),
            TriggerDef::Scheduler {
                cron: "* * * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr1 = WorkspaceManager::from_config("default", config1);
        let mut triggers = load_triggers(&mgr1, None).await;
        assert_eq!(triggers.len(), 1);

        // Simulate a run
        let run_time = Utc::now();
        triggers.get_mut("default/my-trigger").unwrap().last_run = Some(run_time);
        triggers.get_mut("default/my-trigger").unwrap().next_run =
            Some(run_time + chrono::Duration::minutes(1));

        // Reload with a DIFFERENT cron — state should be reset
        let mut config2 = WorkspaceConfig::new();
        config2.triggers.insert(
            "my-trigger".to_string(),
            TriggerDef::Scheduler {
                cron: "0 * * * *".to_string(), // changed from * to 0
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr2 = WorkspaceManager::from_config("default", config2);
        let reloaded = load_triggers(&mgr2, Some(&triggers)).await;
        assert_eq!(reloaded.len(), 1);
        // State should be reset because cron expression changed
        assert!(reloaded["default/my-trigger"].last_run.is_none());
        assert!(reloaded["default/my-trigger"].next_run.is_none());
        assert_eq!(reloaded["default/my-trigger"].cron_expr, "0 * * * *");
    }

    #[tokio::test]
    async fn test_load_triggers_multiple_triggers() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "every-minute".to_string(),
            TriggerDef::Scheduler {
                cron: "* * * * *".to_string(),
                task: "task-a".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );
        config.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 0 2 * * *".to_string(),
                task: "task-b".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );
        // This one should be skipped (disabled)
        config.triggers.insert(
            "disabled-one".to_string(),
            TriggerDef::Scheduler {
                cron: "0 0 * * *".to_string(),
                task: "task-c".to_string(),
                input: HashMap::new(),
                enabled: false,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr = WorkspaceManager::from_config("default", config);
        let triggers = load_triggers(&mgr, None).await;

        assert_eq!(triggers.len(), 2);
        assert!(triggers.contains_key("default/every-minute"));
        assert!(triggers.contains_key("default/nightly"));
        assert!(!triggers.contains_key("default/disabled-one"));

        assert_eq!(triggers["default/every-minute"].task, "task-a");
        assert_eq!(triggers["default/nightly"].task, "task-b");
    }

    #[tokio::test]
    async fn test_load_triggers_preserves_input() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        let mut input = HashMap::new();
        input.insert("env".to_string(), json!("production"));
        input.insert("retries".to_string(), json!(3));

        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "deploy".to_string(),
            TriggerDef::Scheduler {
                cron: "0 0 * * *".to_string(),
                task: "deploy-task".to_string(),
                input: input.clone(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr = WorkspaceManager::from_config("default", config);
        let triggers = load_triggers(&mgr, None).await;

        assert_eq!(triggers.len(), 1);
        let t = &triggers["default/deploy"];
        assert_eq!(t.input.len(), 2);
        assert_eq!(t.input["env"], json!("production"));
        assert_eq!(t.input["retries"], json!(3));
    }

    #[tokio::test]
    async fn test_scheduler_cancellation() {
        use crate::config::{DbConfig, LogStorageConfig, RecoveryConfig, ServerConfig};
        use crate::log_storage::LogStorage;
        use tokio_util::sync::CancellationToken;

        // Create a workspace with no triggers so the scheduler sleeps for 60s
        let ws_config = WorkspaceConfig::new();
        let mgr = WorkspaceManager::from_config("default", ws_config);

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

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
                sweep_interval_secs: 60,
                unmatched_step_timeout_secs: 30,
                ..Default::default()
            },
            acl: None,
            mcp: None,
        };
        let log_storage = LogStorage::new(&config.log_storage.local_dir);
        let pool = sqlx::PgPool::connect_lazy("postgres://invalid:5432/db").unwrap();
        let state = AppState::new(pool, mgr, config, log_storage, HashMap::new());

        let handle = start(state, cancel.clone());

        // Cancel after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        // The scheduler should exit within a reasonable time
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "Scheduler should have stopped after cancellation"
        );
    }

    #[test]
    fn test_update_wakeup_takes_minimum() {
        // Verify the min-duration logic: create two crons with different intervals
        let cron1: Cron = "* * * * *".parse().unwrap(); // every minute
        let cron2: Cron = "0 * * * *".parse().unwrap(); // every hour

        let now = Utc::now();
        let next1 = cron1.find_next_occurrence(&now, false).unwrap();
        let next2 = cron2.find_next_occurrence(&now, false).unwrap();

        // next1 should be sooner (every minute vs every hour)
        assert!(next1 <= next2);

        // The min of the two durations should be next1 - now
        let durations = vec![next1 - now, next2 - now];
        let min_dur = durations.into_iter().min().unwrap();
        assert_eq!(min_dur, next1 - now);
    }

    #[test]
    fn test_compute_next_run_with_timezone() {
        use chrono_tz::Tz;

        // Pin "now" to noon UTC on 2024-01-15 (January → CET = UTC+1, no DST).
        let now = DateTime::parse_from_rfc3339("2024-01-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let cron: Cron = "0 2 * * *".parse().unwrap();

        // Without timezone: next 02:00 UTC is 2024-01-16T02:00:00Z
        let next_utc = compute_next_run(&cron, None, now, None);
        assert!(next_utc.is_some());
        let expected_utc = DateTime::parse_from_rfc3339("2024-01-16T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            next_utc.unwrap(),
            expected_utc,
            "Without timezone, next 02:00 cron should fire at 2024-01-16T02:00:00Z"
        );

        // With Europe/Copenhagen (CET = UTC+1 in January):
        // 02:00 Copenhagen = 01:00 UTC, so next occurrence is 2024-01-16T01:00:00Z
        let tz_cph: Tz = "Europe/Copenhagen".parse().unwrap();
        let next_cph = compute_next_run(&cron, None, now, Some(tz_cph));
        assert!(next_cph.is_some());
        let expected_cph = DateTime::parse_from_rfc3339("2024-01-16T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            next_cph.unwrap(),
            expected_cph,
            "With Europe/Copenhagen (UTC+1 in Jan), next 02:00 local cron should fire at 2024-01-16T01:00:00Z"
        );
    }

    #[tokio::test]
    async fn test_load_triggers_resets_on_timezone_change() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        // Load with timezone = None
        let mut config1 = WorkspaceConfig::new();
        config1.triggers.insert(
            "tz-trigger".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let mgr1 = WorkspaceManager::from_config("default", config1);
        let mut triggers = load_triggers(&mgr1, None).await;
        assert_eq!(triggers.len(), 1);

        // Simulate a run
        let run_time = Utc::now();
        triggers.get_mut("default/tz-trigger").unwrap().last_run = Some(run_time);
        triggers.get_mut("default/tz-trigger").unwrap().next_run =
            Some(run_time + chrono::Duration::hours(1));

        // Reload with timezone changed — state should be reset
        let mut config2 = WorkspaceConfig::new();
        config2.triggers.insert(
            "tz-trigger".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(), // same cron
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: Some("Europe/Copenhagen".to_string()), // timezone changed
            },
        );

        let mgr2 = WorkspaceManager::from_config("default", config2);
        let reloaded = load_triggers(&mgr2, Some(&triggers)).await;
        assert_eq!(reloaded.len(), 1);
        // State should be reset because timezone changed
        assert!(reloaded["default/tz-trigger"].last_run.is_none());
        assert!(reloaded["default/tz-trigger"].next_run.is_none());
        assert!(reloaded["default/tz-trigger"].timezone.is_some());
    }

    #[tokio::test]
    async fn test_load_triggers_resets_on_timezone_removed() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        // First load: trigger has timezone = Some("Europe/Copenhagen")
        let mut config1 = WorkspaceConfig::new();
        config1.triggers.insert(
            "tz-trigger".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: Some("Europe/Copenhagen".to_string()),
            },
        );

        let mgr1 = WorkspaceManager::from_config("default", config1);
        let mut triggers = load_triggers(&mgr1, None).await;
        assert_eq!(triggers.len(), 1);

        // Simulate a completed run: set last_run and next_run
        let run_time = DateTime::parse_from_rfc3339("2024-01-15T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = triggers.get_mut("default/tz-trigger").unwrap();
        t.last_run = Some(run_time);
        t.next_run = Some(run_time + chrono::Duration::hours(24));

        // Reload with timezone removed (None) — same cron expression, different timezone_str
        let mut config2 = WorkspaceConfig::new();
        config2.triggers.insert(
            "tz-trigger".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(), // same cron
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None, // timezone removed
            },
        );

        let mgr2 = WorkspaceManager::from_config("default", config2);
        let reloaded = load_triggers(&mgr2, Some(&triggers)).await;

        assert_eq!(reloaded.len(), 1);
        let t = &reloaded["default/tz-trigger"];
        // State must be reset: timezone_str changed (Some → None)
        assert!(
            t.last_run.is_none(),
            "last_run should be reset when timezone removed"
        );
        assert!(
            t.next_run.is_none(),
            "next_run should be reset when timezone removed"
        );
        assert!(
            t.timezone.is_none(),
            "timezone should be None after removal"
        );
    }

    #[tokio::test]
    async fn test_load_triggers_invalid_timezone_falls_back_to_utc() {
        use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

        // Construct a trigger with an invalid timezone string, bypassing validation
        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "bad-tz".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: Some("Garbage/Zone".to_string()),
            },
        );

        let mgr = WorkspaceManager::from_config("default", config);
        let triggers = load_triggers(&mgr, None).await;

        // The trigger must still be present (graceful fallback, not skipped)
        assert_eq!(
            triggers.len(),
            1,
            "Trigger with invalid timezone should not be skipped"
        );
        assert!(triggers.contains_key("default/bad-tz"));

        let t = &triggers["default/bad-tz"];
        // timezone field should be None (fell back to UTC)
        assert!(
            t.timezone.is_none(),
            "Invalid timezone should fall back to UTC (timezone = None)"
        );
    }

    #[test]
    fn test_compute_next_run_dst_spring_forward() {
        use chrono_tz::Tz;

        // Europe/London springs forward on the last Sunday of March 2024 (2024-03-31).
        // At 01:00 GMT the clocks jump to 02:00 BST, so 01:30 local time does not exist.
        // "now" is 00:30 UTC — before the gap.
        let now = DateTime::parse_from_rfc3339("2024-03-31T00:30:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // "30 1 * * *" targets 01:30 local time, which falls inside the DST gap.
        let cron: Cron = "30 1 * * *".parse().unwrap();
        let tz_london: Tz = "Europe/London".parse().unwrap();

        let next = compute_next_run(&cron, None, now, Some(tz_london));

        // croner must return *some* future time (skip the gap, schedule the next valid day)
        assert!(
            next.is_some(),
            "Should find a next occurrence even when cron falls in DST gap"
        );
        assert!(
            next.unwrap() > now,
            "Next occurrence must be strictly in the future"
        );
    }

    #[test]
    fn test_compute_next_run_dst_fall_back() {
        use chrono_tz::Tz;

        // America/New_York falls back on the first Sunday of November 2024 (2024-11-03).
        // At 02:00 EDT (06:00 UTC) clocks go back to 01:00 EST, so 01:30 local time
        // occurs *twice* on this day.
        // "now" is 04:00 UTC (midnight EDT), before the fall-back.
        let now = DateTime::parse_from_rfc3339("2024-11-03T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // "30 1 * * *" targets 01:30 local, which is ambiguous on this day.
        let cron: Cron = "30 1 * * *".parse().unwrap();
        let tz_ny: Tz = "America/New_York".parse().unwrap();

        let next = compute_next_run(&cron, None, now, Some(tz_ny));

        // croner must return *some* future time (handle the ambiguous hour without panicking)
        assert!(
            next.is_some(),
            "Should find a next occurrence even on DST fall-back day"
        );
        assert!(
            next.unwrap() > now,
            "Next occurrence must be strictly in the future"
        );
    }
}
