use crate::acl::{load_user_acl_context, make_task_path, TaskPermission};
use crate::state::AppState;
use crate::web::api::get_workspace_or_error;
use crate::web::api::middleware::AuthUser;
use crate::web::error::AppError;
use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    pub task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_task: Option<String>,
    pub enabled: bool,
    pub input: HashMap<String, serde_json::Value>,
    pub next_runs: Vec<String>,
}

impl TriggerInfo {
    /// Build a TriggerInfo from a name and TriggerDef, computing next_runs for cron triggers.
    pub fn from_def(name: &str, trigger: &TriggerDef, next_runs_count: usize) -> Self {
        let next_runs = compute_next_runs(trigger, next_runs_count);
        let (cron, timezone) = match trigger {
            TriggerDef::Scheduler { cron, timezone, .. } => (Some(cron.clone()), timezone.clone()),
            TriggerDef::Webhook { .. } | TriggerDef::EventSource { .. } => (None, None),
        };
        let target_task = match trigger {
            TriggerDef::EventSource { target_task, .. } => Some(target_task.clone()),
            TriggerDef::Scheduler { .. } | TriggerDef::Webhook { .. } => None,
        };
        Self {
            name: name.to_string(),
            trigger_type: trigger.trigger_type_str().to_string(),
            cron,
            timezone,
            task: trigger.task().to_string(),
            target_task,
            enabled: trigger.enabled(),
            input: trigger.input().clone(),
            next_runs: next_runs.iter().map(|dt| dt.to_rfc3339()).collect(),
        }
    }
}

/// Compute the next N fire times for a trigger.
///
/// Returns UTC ISO 8601 timestamps for scheduler-type triggers with a valid cron expression.
/// When a timezone is configured, computes occurrences in local time then converts to UTC.
/// Returns an empty vec for non-scheduler types or invalid/missing cron expressions.
pub fn compute_next_runs(trigger: &TriggerDef, count: usize) -> Vec<DateTime<Utc>> {
    let (cron_expr, tz_str) = match trigger {
        TriggerDef::Scheduler { cron, timezone, .. } => (cron, timezone.as_deref()),
        TriggerDef::Webhook { .. } | TriggerDef::EventSource { .. } => return vec![],
    };

    let cron = match CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(cron_expr)
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let tz: Option<Tz> = tz_str.and_then(|s| s.parse().ok());

    let now = Utc::now();
    match tz {
        Some(tz) => {
            let local_now = now.with_timezone(&tz);
            cron.iter_after(local_now)
                .take(count)
                .map(|dt| dt.with_timezone(&Utc))
                .collect()
        }
        None => cron.iter_after(now).take(count).collect(),
    }
}

/// GET /api/workspaces/:ws/triggers — List all triggers in a workspace
#[tracing::instrument(skip(state))]
pub async fn list_triggers(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(ws): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let workspace = get_workspace_or_error(&state, &ws).await?;

    let mut triggers: Vec<TriggerInfo> = workspace
        .triggers
        .iter()
        .map(|(name, trigger)| TriggerInfo::from_def(name, trigger, 5))
        .collect();

    // ACL filter: only show triggers for tasks the user can access
    if let Some(ref auth) = auth_user {
        if state.acl.is_configured() {
            if let Ok(user_id) = auth.user_id() {
                if let Ok((is_admin, groups)) =
                    load_user_acl_context(&state.pool, user_id, auth.is_admin()).await
                {
                    if !is_admin {
                        triggers.retain(|t| {
                            let task_def = workspace.tasks.get(&t.task);
                            let folder = task_def.and_then(|td| td.folder.as_deref());
                            let task_path = make_task_path(folder, &t.task);
                            let perm = state.acl.evaluate(
                                &ws,
                                &task_path,
                                &auth.claims.email,
                                &groups,
                                false,
                            );
                            !matches!(perm, TaskPermission::Deny)
                        });
                    }
                }
            }
        }
    }

    Ok(Json(triggers))
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
                concurrency: Default::default(),
                timezone: None,
            },
            "webhook" => TriggerDef::Webhook {
                name: "test-hook".to_string(),
                task: "test-task".to_string(),
                secret: None,
                input: HashMap::new(),
                enabled,
                mode: None,
                timeout_secs: None,
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
    fn test_compute_next_runs_event_source_type() {
        // EventSource triggers always return no next_runs (no cron schedule).
        let trigger = TriggerDef::EventSource {
            task: "my-consumer".to_string(),
            target_task: "process-events".to_string(),
            enabled: true,
            input: HashMap::new(),
            env: HashMap::new(),
            restart_policy: Default::default(),
            backoff_secs: 5,
            max_in_flight: None,
        };
        let runs = compute_next_runs(&trigger, 5);
        assert!(runs.is_empty());
    }

    #[test]
    fn test_trigger_info_event_source_has_no_cron_or_timezone() {
        let trigger = TriggerDef::EventSource {
            task: "my-consumer".to_string(),
            target_task: "process-events".to_string(),
            enabled: true,
            input: HashMap::new(),
            env: HashMap::new(),
            restart_policy: Default::default(),
            backoff_secs: 5,
            max_in_flight: None,
        };
        let info = TriggerInfo::from_def("my-source", &trigger, 5);
        let json = serde_json::to_value(&info).unwrap();

        assert_eq!(json["type"], "event_source");
        // task() returns the consumer task name for EventSource
        assert_eq!(json["task"], "my-consumer");
        assert_eq!(json["target_task"], "process-events");
        assert_eq!(json["enabled"], true);
        assert!(json.get("cron").is_none(), "cron should be absent");
        assert!(json.get("timezone").is_none(), "timezone should be absent");
        assert_eq!(json["next_runs"], serde_json::json!([]));
    }

    #[test]
    fn test_trigger_info_scheduler_has_no_target_task() {
        let trigger = make_trigger("scheduler", Some("0 0 * * *"), true);
        let info = TriggerInfo::from_def("my-sched", &trigger, 0);
        let json = serde_json::to_value(&info).unwrap();
        assert!(
            json.get("target_task").is_none(),
            "target_task should be absent for scheduler triggers"
        );
    }

    #[test]
    fn test_trigger_info_webhook_has_no_target_task() {
        let trigger = make_trigger("webhook", None, true);
        let info = TriggerInfo::from_def("my-hook", &trigger, 0);
        let json = serde_json::to_value(&info).unwrap();
        assert!(
            json.get("target_task").is_none(),
            "target_task should be absent for webhook triggers"
        );
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

    #[test]
    fn test_compute_next_runs_with_timezone() {
        // "0 2 * * *" with Europe/Copenhagen should produce different UTC times than without.
        //
        // Note: `compute_next_runs` calls `Utc::now()` internally and there is no way to inject
        // a pinned time without refactoring the function. In the astronomically unlikely event
        // that this test runs exactly at 02:00:00 UTC while Copenhagen is also at 02:00:00 local
        // time (i.e. during the brief DST window where they coincide), the `assert_ne!` could
        // theoretically fail. This is acceptable given the negligible probability.
        let trigger_utc = TriggerDef::Scheduler {
            cron: "0 2 * * *".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: None,
        };
        let trigger_cph = TriggerDef::Scheduler {
            cron: "0 2 * * *".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: Some("Europe/Copenhagen".to_string()),
        };

        let runs_utc = compute_next_runs(&trigger_utc, 1);
        let runs_cph = compute_next_runs(&trigger_cph, 1);

        assert_eq!(runs_utc.len(), 1);
        assert_eq!(runs_cph.len(), 1);

        // Copenhagen is UTC+1 or UTC+2, so 2 AM Copenhagen != 2 AM UTC
        assert_ne!(
            runs_utc[0], runs_cph[0],
            "Timezone-aware next_runs should differ from UTC"
        );
    }

    #[test]
    fn test_compute_next_runs_with_timezone_multiple_occurrences() {
        let trigger = TriggerDef::Scheduler {
            cron: "0 0 * * *".to_string(),
            task: "test".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: Some("America/New_York".to_string()),
        };

        let runs = compute_next_runs(&trigger, 5);

        assert_eq!(runs.len(), 5);

        // All results must be in the future
        let now = Utc::now();
        for run in &runs {
            assert!(*run > now, "run {run} should be in the future");
        }

        // Results must be in strict ascending order
        for window in runs.windows(2) {
            assert!(
                window[0] < window[1],
                "runs must be in ascending order: {} >= {}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn test_trigger_info_serialization_includes_timezone() {
        let trigger = TriggerDef::Scheduler {
            cron: "0 0 * * *".to_string(),
            task: "test-task".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: Some("America/New_York".to_string()),
        };

        let info = TriggerInfo::from_def("test", &trigger, 1);
        let json = serde_json::to_value(&info).unwrap();

        assert_eq!(json["timezone"], "America/New_York");
        assert_eq!(json["name"], "test");
    }

    #[test]
    fn test_trigger_info_serialization_omits_timezone_when_none() {
        let trigger = TriggerDef::Scheduler {
            cron: "0 0 * * *".to_string(),
            task: "test-task".to_string(),
            input: HashMap::new(),
            enabled: true,
            concurrency: Default::default(),
            timezone: None,
        };

        let info = TriggerInfo::from_def("test", &trigger, 1);
        let json = serde_json::to_value(&info).unwrap();

        assert!(
            json.get("timezone").is_none(),
            "timezone field should be absent when None, got: {json}"
        );
    }
}
