use crate::state::AppState;
use crate::web::api::get_workspace_or_error;
use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use croner::parser::{CronParser, Seconds};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::TriggerDef;

#[derive(Debug, Serialize)]
pub struct TriggerInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub trigger_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    pub task: String,
    pub enabled: bool,
    pub input: HashMap<String, serde_json::Value>,
    pub next_runs: Vec<String>,
}

impl TriggerInfo {
    /// Build a TriggerInfo from a name and TriggerDef, computing next_runs for cron triggers.
    pub fn from_def(name: &str, trigger: &TriggerDef, next_runs_count: usize) -> Self {
        let next_runs = compute_next_runs(trigger, next_runs_count);
        let cron = match trigger {
            TriggerDef::Scheduler { cron, .. } => Some(cron.clone()),
            _ => None,
        };
        Self {
            name: name.to_string(),
            trigger_type: trigger.trigger_type_str().to_string(),
            cron,
            task: trigger.task().to_string(),
            enabled: trigger.enabled(),
            input: trigger.input().clone(),
            next_runs: next_runs.iter().map(|dt| dt.to_rfc3339()).collect(),
        }
    }
}

/// Compute the next N fire times for a trigger.
///
/// Returns ISO 8601 timestamps for scheduler-type triggers with a valid cron expression.
/// Returns an empty vec for non-scheduler types or invalid/missing cron expressions.
pub fn compute_next_runs(trigger: &TriggerDef, count: usize) -> Vec<DateTime<Utc>> {
    let cron_expr = match trigger {
        TriggerDef::Scheduler { cron, .. } => cron,
        _ => return vec![],
    };

    let cron = match CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(cron_expr)
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let now = Utc::now();
    cron.iter_after(now).take(count).collect()
}

/// GET /api/workspaces/:ws/triggers — List all triggers in a workspace
#[tracing::instrument(skip(state))]
pub async fn list_triggers(
    State(state): State<Arc<AppState>>,
    Path(ws): Path<String>,
) -> impl IntoResponse {
    let workspace = match get_workspace_or_error(&state, &ws).await {
        Ok(w) => w,
        Err(resp) => return resp,
    };

    let triggers: Vec<TriggerInfo> = workspace
        .triggers
        .iter()
        .map(|(name, trigger)| TriggerInfo::from_def(name, trigger, 5))
        .collect();

    Json(triggers).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use stroem_common::models::workflow::TriggerDef;

    fn make_trigger(trigger_type: &str, cron: Option<&str>, enabled: bool) -> TriggerDef {
        match trigger_type {
            "scheduler" => TriggerDef::Scheduler {
                cron: cron.unwrap_or("* * * * *").to_string(),
                task: "test-task".to_string(),
                input: HashMap::new(),
                enabled,
            },
            "webhook" => TriggerDef::Webhook {
                name: "test-hook".to_string(),
                task: "test-task".to_string(),
                secret: None,
                input: HashMap::new(),
                enabled,
            },
            _ => panic!("Unknown trigger type: {trigger_type}"),
        }
    }

    #[test]
    fn test_compute_next_runs_valid_cron() {
        let trigger = make_trigger("scheduler", Some("0 0 * * *"), true);
        let runs = compute_next_runs(&trigger, 5);
        assert_eq!(runs.len(), 5);

        // All should be in the future
        let now = Utc::now();
        for run in &runs {
            assert!(*run > now);
        }

        // Should be in ascending order
        for window in runs.windows(2) {
            assert!(window[0] < window[1]);
        }
    }

    #[test]
    fn test_compute_next_runs_with_seconds() {
        let trigger = make_trigger("scheduler", Some("*/10 * * * * *"), true);
        let runs = compute_next_runs(&trigger, 3);
        assert_eq!(runs.len(), 3);
    }

    #[test]
    fn test_compute_next_runs_invalid_cron() {
        let trigger = make_trigger("scheduler", Some("not a cron"), true);
        let runs = compute_next_runs(&trigger, 5);
        assert!(runs.is_empty());
    }

    #[test]
    fn test_compute_next_runs_webhook_type() {
        // Webhook triggers should return no next_runs
        let trigger = make_trigger("webhook", None, true);
        let runs = compute_next_runs(&trigger, 5);
        assert!(runs.is_empty());
    }

    #[test]
    fn test_compute_next_runs_non_scheduler_type() {
        let trigger = make_trigger("webhook", Some("0 0 * * *"), true);
        let runs = compute_next_runs(&trigger, 5);
        assert!(runs.is_empty());
    }

    #[test]
    fn test_compute_next_runs_disabled_trigger_still_computes() {
        // Disabled triggers still compute next_runs — enabled is a display concern
        let trigger = make_trigger("scheduler", Some("0 0 * * *"), false);
        let runs = compute_next_runs(&trigger, 5);
        assert_eq!(runs.len(), 5);
    }

    #[test]
    fn test_compute_next_runs_zero_count() {
        let trigger = make_trigger("scheduler", Some("0 0 * * *"), true);
        let runs = compute_next_runs(&trigger, 0);
        assert!(runs.is_empty());
    }
}
