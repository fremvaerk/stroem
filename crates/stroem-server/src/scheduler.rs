use crate::job_creator::create_job_for_task;
use crate::workspace::WorkspaceManager;
use chrono::{DateTime, Utc};
use croner::parser::{CronParser, Seconds};
use croner::Cron;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
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
    last_run: Option<DateTime<Utc>>,
    next_run: Option<DateTime<Utc>>,
}

/// Spawn the scheduler background task.
///
/// Iterates triggers across all workspaces, computes smart sleep intervals,
/// and fires jobs at the correct time. Supports config hot-reload (preserving
/// last_run state for unchanged triggers) and clean shutdown via CancellationToken.
pub fn start(
    pool: PgPool,
    workspaces: Arc<WorkspaceManager>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_loop(pool, workspaces, cancel).await;
    })
}

async fn run_loop(pool: PgPool, workspaces: Arc<WorkspaceManager>, cancel: CancellationToken) {
    tracing::info!("Scheduler started");

    let mut triggers = load_triggers(&workspaces, None).await;
    tracing::info!("Scheduler loaded {} trigger(s)", triggers.len());

    loop {
        let now = Utc::now();

        // Compute next_run for triggers that need it, and collect keys to fire
        let mut to_fire = Vec::new();
        for (key, state) in triggers.iter_mut() {
            if state.next_run.is_none() {
                state.next_run = compute_next_run(&state.cron, state.last_run, now);
            }

            if let Some(next) = state.next_run {
                if now >= next {
                    to_fire.push(key.clone());
                }
            }
        }

        // Fire due triggers
        for key in &to_fire {
            let state = triggers.get(key).unwrap();
            fire_trigger(&pool, &workspaces, state).await;

            let state = triggers.get_mut(key).unwrap();
            let now = Utc::now();
            state.last_run = Some(now);
            state.next_run = compute_next_run(&state.cron, Some(now), now);
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
        triggers = load_triggers(&workspaces, Some(&triggers)).await;
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
        let config = match workspaces.get_config_async(ws_name).await {
            Some(c) => c,
            None => continue,
        };

        for (trigger_name, trigger_def) in &config.triggers {
            // Only process enabled scheduler triggers
            let (cron_expr, task, input) = match trigger_def {
                stroem_common::models::workflow::TriggerDef::Scheduler {
                    cron,
                    task,
                    input,
                    enabled,
                } if *enabled => (cron.clone(), task.clone(), input.clone()),
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

            let key = format!("{}/{}", ws_name, trigger_name);

            // Preserve state from previous cycle if cron expression unchanged
            let (last_run, next_run) = match previous {
                Some(prev) => match prev.get(&key) {
                    Some(old) if old.cron_expr == cron_expr => (old.last_run, old.next_run),
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
                    last_run,
                    next_run,
                },
            );
        }
    }

    triggers
}

/// Compute the next run time for a trigger.
fn compute_next_run(
    cron: &Cron,
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    // Start searching from the later of last_run or now
    let start = match last_run {
        Some(lr) if lr > now => lr,
        _ => now,
    };

    match cron.find_next_occurrence(&start, false) {
        Ok(next) => Some(next),
        Err(e) => {
            tracing::warn!("Failed to compute next occurrence: {:#}", e);
            None
        }
    }
}

/// Fire a trigger by creating a job via the shared job_creator.
async fn fire_trigger(pool: &PgPool, workspaces: &WorkspaceManager, state: &TriggerState) {
    let source_id = format!("{}/{}", state.workspace, state.trigger_name);
    let input = serde_json::to_value(&state.input).unwrap_or_default();

    tracing::info!(
        "Scheduler firing trigger '{}' -> task '{}'",
        source_id,
        state.task
    );

    let config = match workspaces.get_config_async(&state.workspace).await {
        Some(c) => c,
        None => {
            tracing::error!(
                "Workspace '{}' not found when firing trigger '{}'",
                state.workspace,
                source_id
            );
            return;
        }
    };

    match create_job_for_task(
        pool,
        &config,
        &state.workspace,
        &state.task,
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
        let next = compute_next_run(&cron, None, now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn test_compute_next_run_after_last_run() {
        let cron: Cron = "* * * * *".parse().unwrap();
        let now = Utc::now();
        let last_run = Some(now);
        let next = compute_next_run(&cron, last_run, now);
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
                action_type: "shell".to_string(),
                task: None,
                cmd: Some("echo hello".to_string()),
                script: None,
                runner: None,
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
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                inline_action: None,
            },
        );
        config.tasks.insert(
            "hello".to_string(),
            TaskDef {
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow,
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
        let next = compute_next_run(&cron, Some(future_last_run), now);
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
            },
        );
        config.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 0 2 * * *".to_string(),
                task: "task-b".to_string(),
                input: HashMap::new(),
                enabled: true,
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
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        // Create a workspace with no triggers so the scheduler sleeps for 60s
        let config = WorkspaceConfig::new();
        let mgr = Arc::new(WorkspaceManager::from_config("default", config));

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // We need a pool but won't actually use it — create a dummy connection string.
        // Since there are no triggers, fire_trigger is never called.
        // Use a pool that will fail on connect — it's never used without triggers.
        let pool = sqlx::PgPool::connect_lazy("postgres://invalid:5432/db").unwrap();

        let handle = start(pool, mgr, cancel.clone());

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
}
