//! Settlement: deciding a job's terminal status from its steps.

use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashSet;
use stroem_common::models::job::{JobStatus, StepStatus};
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{JobRepo, JobStepRepo, JobStepRow};
use uuid::Uuid;

/// What settlement decided for a job whose every step is terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    pub status: JobStatus,
    pub output: Option<serde_json::Value>,
}

/// Pure settlement decision. `None` while any step is non-terminal.
pub fn decide(task: &TaskDef, steps: &[JobStepRow]) -> Option<Settled> {
    let terminal = |s: &JobStepRow| {
        matches!(
            s.status.parse::<StepStatus>().ok(),
            Some(StepStatus::Completed)
                | Some(StepStatus::Failed)
                | Some(StepStatus::Skipped)
                | Some(StepStatus::Cancelled)
        )
    };
    if !steps.iter().all(terminal) {
        return None;
    }

    // Loop instance steps ("process[0]") are not in task.flow — look up by
    // their placeholder name. Instance failures are already folded into the
    // placeholder by the cascade's rollup rule (R6).
    let flow_name = |name: &str| -> String {
        match name.find('[') {
            Some(i) => name[..i].to_string(),
            None => name.to_string(),
        }
    };
    let tolerated = |name: &str| -> bool {
        task.flow
            .get(&flow_name(name))
            .map(|fs| fs.continue_on_failure)
            .unwrap_or(false)
    };

    let untolerated_failure = steps
        .iter()
        .any(|s| s.status == StepStatus::Failed.as_ref() && !tolerated(&s.step_name));
    if untolerated_failure {
        return Some(Settled {
            status: JobStatus::Failed,
            output: None,
        });
    }

    if steps
        .iter()
        .any(|s| s.status == StepStatus::Cancelled.as_ref())
    {
        return Some(Settled {
            status: JobStatus::Cancelled,
            output: None,
        });
    }

    // Output = outputs of the flow's terminal steps (nothing depends on them).
    let depended_on: HashSet<&str> = task
        .flow
        .values()
        .flat_map(|fs| fs.depends_on.iter().map(|s| s.as_str()))
        .collect();
    let terminal_steps: HashSet<&str> = task
        .flow
        .keys()
        .filter(|name| !depended_on.contains(name.as_str()))
        .map(|s| s.as_str())
        .collect();
    let mut job_output = serde_json::Map::new();
    for s in steps {
        if terminal_steps.contains(s.step_name.as_str()) {
            if let Some(ref output) = s.output {
                job_output.insert(s.step_name.clone(), output.clone());
            }
        }
    }
    let output = if job_output.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(job_output))
    };
    Some(Settled {
        status: JobStatus::Completed,
        output,
    })
}

/// If every step of the job is terminal, decide and persist the job's final
/// status and return it; otherwise return `None` and touch nothing.
///
/// Single source of truth for terminal settlement — called from
/// `cascade_and_settle` AND from job creation (`dispatch::init`), so a job that
/// is already terminal at creation gets exactly the same rules.
#[tracing::instrument(skip(pool, task))]
pub async fn settle_if_all_terminal(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
) -> Result<Option<JobStatus>> {
    let steps = JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .context("Failed to get steps for settlement")?;
    let Some(settled) = decide(task, &steps) else {
        return Ok(None);
    };

    // `JobRepo::settle` already applies its own "Failed to settle job" context.
    let wrote =
        JobRepo::settle(pool, job_id, settled.status.clone(), settled.output.clone()).await?;
    if !wrote {
        let current = JobRepo::get(pool, job_id).await?.map(|j| j.status);
        tracing::info!(job_id = %job_id, ?current, "job already terminal, settlement not written");
        return Ok(current.and_then(|s| s.parse::<JobStatus>().ok()));
    }

    match settled.status {
        JobStatus::Failed => {
            tracing::info!("Job {} failed (one or more steps failed)", job_id);
        }
        JobStatus::Cancelled => {
            tracing::info!(
                "Job {} cancelled (a step was cancelled, no untolerated failure)",
                job_id
            );
        }
        _ => {
            let failed_count = steps
                .iter()
                .filter(|s| s.status == StepStatus::Failed.as_ref())
                .count();
            if failed_count > 0 {
                tracing::info!(
                    "Job {} completed with {} tolerable failure(s)",
                    job_id,
                    failed_count
                );
            } else {
                tracing::info!("Job {} completed successfully", job_id);
            }
        }
    }
    Ok(Some(settled.status))
}

/// Run the step cascade for `job_id`, then settle the job if every step is
/// terminal. Replaces `orchestrator::on_step_completed`; the workspace config
/// is required because production always has one.
#[tracing::instrument(skip(pool, task, workspace_config))]
pub async fn cascade_and_settle(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
    workspace_config: &WorkspaceConfig,
) -> Result<Option<JobStatus>> {
    crate::cascade::execute(pool, job_id, task, Some(workspace_config))
        .await
        .context("Failed to run step cascade")?;
    settle_if_all_terminal(pool, job_id, task).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use stroem_common::models::workflow::FlowStep;

    fn flow_step(deps: &[&str], continue_on_failure: bool) -> FlowStep {
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            input: HashMap::new(),
            continue_on_failure,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        }
    }

    fn task(flow: Vec<(&str, FlowStep)>) -> TaskDef {
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: flow.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        }
    }

    fn row(name: &str, status: &str, output: Option<serde_json::Value>) -> JobStepRow {
        let mut r = JobStepRow::test_default(Uuid::nil(), name);
        r.status = status.to_string();
        r.output = output;
        r
    }

    #[test]
    fn all_completed_completes_with_terminal_step_outputs() {
        let t = task(vec![
            ("a", flow_step(&[], false)),
            ("b", flow_step(&["a"], false)),
        ]);
        let steps = vec![
            row("a", "completed", Some(serde_json::json!({"x": 1}))),
            row("b", "completed", Some(serde_json::json!({"y": 2}))),
        ];
        let s = decide(&t, &steps).unwrap();
        assert_eq!(s.status, JobStatus::Completed);
        assert_eq!(s.output, Some(serde_json::json!({"b": {"y": 2}})));
    }

    #[test]
    fn untolerated_failure_fails() {
        let t = task(vec![("a", flow_step(&[], false))]);
        let s = decide(&t, &[row("a", "failed", None)]).unwrap();
        assert_eq!(s.status, JobStatus::Failed);
        assert_eq!(s.output, None);
    }

    #[test]
    fn tolerated_failure_completes_and_aggregates_the_rest() {
        let t = task(vec![
            ("a", flow_step(&[], true)),
            ("b", flow_step(&[], false)),
        ]);
        let steps = vec![
            row("a", "failed", None),
            row("b", "completed", Some(serde_json::json!(3))),
        ];
        let s = decide(&t, &steps).unwrap();
        assert_eq!(s.status, JobStatus::Completed);
        assert_eq!(s.output, Some(serde_json::json!({"b": 3})));
    }

    #[test]
    fn cancelled_without_failure_cancels() {
        let t = task(vec![
            ("a", flow_step(&[], false)),
            ("b", flow_step(&[], false)),
        ]);
        let steps = vec![row("a", "completed", None), row("b", "cancelled", None)];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Cancelled);
    }

    #[test]
    fn failed_beats_cancelled() {
        let t = task(vec![
            ("a", flow_step(&[], false)),
            ("b", flow_step(&[], false)),
        ]);
        let steps = vec![row("a", "failed", None), row("b", "cancelled", None)];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Failed);
    }

    #[test]
    fn live_step_is_none() {
        let t = task(vec![
            ("a", flow_step(&[], false)),
            ("b", flow_step(&[], false)),
        ]);
        for live in ["pending", "ready", "claimed", "running", "suspended"] {
            let steps = vec![row("a", "completed", None), row("b", live, None)];
            assert!(decide(&t, &steps).is_none(), "{live} must block settlement");
        }
    }

    #[test]
    fn instance_rows_map_to_their_placeholder_flow_step() {
        // placeholder "p" tolerates failure; its instance "p[0]" failed.
        let t = task(vec![("p", flow_step(&[], true))]);
        let steps = vec![row("p", "completed", None), row("p[0]", "failed", None)];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Completed);
    }

    #[test]
    fn empty_flow_completes_with_no_output() {
        let t = task(vec![]);
        let s = decide(&t, &[]).unwrap();
        assert_eq!(s.status, JobStatus::Completed);
        assert_eq!(s.output, None);
    }
}
