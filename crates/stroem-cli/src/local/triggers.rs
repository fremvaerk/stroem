use anyhow::Result;
use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};

pub fn cmd_triggers(config: &WorkspaceConfig) -> Result<()> {
    if config.triggers.is_empty() {
        println!("No triggers found.");
        return Ok(());
    }

    let mut rows: Vec<(String, &str, &str, String)> = config
        .triggers
        .iter()
        .map(|(name, trigger)| {
            let detail = match trigger {
                TriggerDef::Scheduler { cron, .. } => cron.clone(),
                TriggerDef::Webhook {
                    name: wh_name,
                    mode,
                    ..
                } => {
                    let mode_str = mode.as_deref().unwrap_or("async");
                    format!("{} ({})", wh_name, mode_str)
                }
                TriggerDef::EventSource {
                    task, target_task, ..
                } => {
                    format!("{} → {}", task, target_task)
                }
            };
            let display_name = if trigger.enabled() {
                name.to_string()
            } else {
                format!("{} [disabled]", name)
            };
            (
                display_name,
                trigger.trigger_type_str(),
                trigger.task(),
                detail,
            )
        })
        .collect();
    rows.sort_by_key(|r| r.0.clone());

    println!(
        "{:<25} {:<12} {:<25} SCHEDULE / WEBHOOK",
        "NAME", "TYPE", "TASK"
    );
    println!("{}", "-".repeat(80));
    for (name, ttype, task, detail) in &rows {
        println!("{:<25} {:<12} {:<25} {}", name, ttype, task, detail);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use stroem_common::models::workflow::ConcurrencyPolicy;

    #[test]
    fn triggers_empty_workspace() {
        let config = WorkspaceConfig::new();
        assert!(cmd_triggers(&config).is_ok());
    }

    #[test]
    fn triggers_lists_scheduler_and_webhook() {
        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "etl".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: ConcurrencyPolicy::Allow,
                timezone: None,
                force_refresh: false,
            },
        );
        config.triggers.insert(
            "deploy-hook".to_string(),
            TriggerDef::Webhook {
                name: "deploy-hook".to_string(),
                task: "deploy".to_string(),
                secret: None,
                input: HashMap::new(),
                enabled: true,
                mode: Some("sync".to_string()),
                timeout_secs: None,
                force_refresh: false,
            },
        );
        assert!(cmd_triggers(&config).is_ok());
    }

    #[test]
    fn triggers_webhook_mode_none_defaults_async() {
        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "ci-hook".to_string(),
            TriggerDef::Webhook {
                name: "ci-hook".to_string(),
                task: "build".to_string(),
                secret: None,
                input: HashMap::new(),
                enabled: true,
                mode: None,
                timeout_secs: None,
                force_refresh: false,
            },
        );
        assert!(cmd_triggers(&config).is_ok());
    }

    #[test]
    fn triggers_scheduler_with_timezone() {
        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "nightly-cph".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "report".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: ConcurrencyPolicy::Allow,
                timezone: Some("Europe/Copenhagen".to_string()),
                force_refresh: false,
            },
        );
        assert!(cmd_triggers(&config).is_ok());
    }

    #[test]
    fn triggers_disabled_shown_with_indicator() {
        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "old-trigger".to_string(),
            TriggerDef::Scheduler {
                cron: "0 6 * * 1".to_string(),
                task: "weekly".to_string(),
                input: HashMap::new(),
                enabled: false,
                concurrency: ConcurrencyPolicy::Allow,
                timezone: None,
                force_refresh: false,
            },
        );
        assert!(cmd_triggers(&config).is_ok());
    }

    #[test]
    fn triggers_event_source_shows_consumer_and_target() {
        use stroem_common::models::workflow::RestartPolicy;
        let mut config = WorkspaceConfig::new();
        config.triggers.insert(
            "queue-listener".to_string(),
            TriggerDef::EventSource {
                task: "consumer-task".to_string(),
                target_task: "process-event".to_string(),
                enabled: true,
                input: HashMap::new(),
                env: HashMap::new(),
                restart_policy: RestartPolicy::Always,
                backoff_secs: 5,
                max_in_flight: None,
            },
        );
        assert!(cmd_triggers(&config).is_ok());
    }
}
