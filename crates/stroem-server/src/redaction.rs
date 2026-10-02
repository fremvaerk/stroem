//! Per-job redaction (spec 2026-10-02 git refs § 7.4, § 9).
//!
//! Every API outlet that returns a job's input/output, step output or
//! `error_message` masks with the live workspaces' values PLUS the secret
//! values of every pin the job references. A pinned commit's sops values (or
//! how its vals references render) can differ from the live config, so the
//! live set alone would let an older or ref-only secret through. A pin that
//! cannot be loaded makes the set incomplete; an incomplete set is never
//! used — callers fail closed on [`RedactionUnavailable`].

use std::collections::BTreeSet;

use serde_json::Value;
use stroem_db::{JobRow, JobStepRow};

use crate::state::AppState;
use crate::workspace::pins::PinRef;
use crate::workspace_set::{
    collect_redaction_values, redact_secrets_in_str, WorkspaceSet, REDACTED,
};

/// A pin the job references could not be loaded, so its secret values are
/// unknown. Callers must not answer with a partial redaction set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionUnavailable {
    pub workspace: String,
    pub commit: String,
}

impl std::fmt::Display for RedactionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "redaction set unavailable: pin {}@{} could not be loaded",
            self.workspace, self.commit
        )
    }
}

impl std::error::Error for RedactionUnavailable {}

/// Job-object keys that carry identifiers, timestamps or enums — never user
/// content. Left untouched so a short secret value cannot mangle a link.
pub const JOB_IDENTIFIER_KEYS: &[&str] = &[
    "job_id",
    "workspace",
    "task_name",
    "mode",
    "status",
    "source_type",
    "source_id",
    "source_job_id",
    "restart_from_step",
    "parent_job_id",
    "parent_step_name",
    "revision",
    "ref",
    "task_folder",
    "worker_id",
    "created_at",
    "started_at",
    "completed_at",
    "retry_of_job_id",
    "retry_job_id",
    "retry_attempt",
    "max_retries",
];

/// Step-entry keys that carry identifiers, timestamps or enums. `child_jobs`
/// holds only links to child jobs (id, workspace, task name, status,
/// timestamp), so it is skipped whole.
pub const STEP_IDENTIFIER_KEYS: &[&str] = &[
    "step_name",
    "action_name",
    "action_type",
    "runner",
    "status",
    "worker_id",
    "started_at",
    "completed_at",
    "suspended_at",
    "retry_at",
    "retry_attempt",
    "max_retries",
    "skip_reason",
    "loop_source",
    "loop_index",
    "loop_total",
    "carried_over",
    "depends_on",
    "action_workspace",
    "action_ref",
    "action_revision",
    "task_workspace",
    "task_ref",
    "task_revision",
    "job_id",
    "workspace",
    "task_name",
    "job_status",
    "child_jobs",
];

/// Every distinct `(workspace, pin)` the job references: its own pin and each
/// step's action pin and task pin. Pure.
pub fn referenced_pins(job: &JobRow, steps: &[JobStepRow]) -> Vec<(String, PinRef)> {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out: Vec<(String, PinRef)> = Vec::new();
    let mut push = |ws: String, pin: PinRef| {
        if seen.insert((ws.clone(), pin.commit.clone())) {
            out.push((ws, pin));
        }
    };
    if let Some(pin) = PinRef::of_job(job) {
        push(job.workspace.clone(), pin);
    }
    for step in steps {
        if let Some(pin) = PinRef::of_step_action(step) {
            let ws = step
                .action_workspace
                .clone()
                .unwrap_or_else(|| job.workspace.clone());
            push(ws, pin);
        }
        if let Some((ws, pin)) = PinRef::of_step_task(step) {
            push(ws, pin);
        }
    }
    out
}

/// The live redaction set plus the secret values of every pin the job
/// references (spec § 7.4). `Err` = some pin could not be loaded.
#[tracing::instrument(skip_all, fields(job_id = %job.job_id, workspace = %job.workspace))]
pub async fn job_redaction_values(
    state: &AppState,
    job: &JobRow,
    steps: &[JobStepRow],
) -> Result<Vec<String>, RedactionUnavailable> {
    let set = WorkspaceSet::load(&state.workspaces, &job.workspace, None).await;
    let mut values = collect_redaction_values(&set);
    for (ws, pin) in referenced_pins(job, steps) {
        match state.workspaces.pins().ensure(&ws, &pin.commit).await {
            // The pin's complete set (R5): its secrets AND the `secret: true`
            // properties of its connections, whichever workspace types them.
            Ok(pinned) => values.extend(state.workspaces.pin_redaction_values(&ws, &pinned).await),
            Err(e) => {
                tracing::warn!(
                    job_id = %job.job_id,
                    workspace = %ws,
                    commit = %pin.commit,
                    "redaction set unavailable: {e}"
                );
                return Err(RedactionUnavailable {
                    workspace: ws,
                    commit: pin.commit,
                });
            }
        }
    }
    Ok(values)
}

/// Redact a job's `output` (webhook responses). Loads the job's steps for
/// their pins. A [`RedactionUnavailable`] is returned inside the `anyhow`
/// error; callers `downcast_ref` it to answer 503.
#[tracing::instrument(skip_all, fields(job_id = %job.job_id))]
pub async fn redact_job_output(
    state: &AppState,
    job: &JobRow,
    output: Option<Value>,
) -> anyhow::Result<Option<Value>> {
    let Some(mut output) = output else {
        return Ok(None);
    };
    let steps = stroem_db::JobStepRepo::get_steps_for_job(&state.pool, job.job_id).await?;
    let secrets = job_redaction_values(state, job, &steps).await?;
    redact_value_tree(&mut output, &secrets);
    Ok(Some(output))
}

/// Mask secret values in one string; a vals `ref+` reference is masked whole.
pub fn redact_str(s: &str, secrets: &[String]) -> String {
    if s.starts_with("ref+") {
        return REDACTED.to_string();
    }
    redact_secrets_in_str(s, secrets)
}

/// Mask secret values (and vals `ref+` references) in every string of a JSON tree.
pub fn redact_value_tree(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(s) => *s = redact_str(s, secrets),
        Value::Object(map) => {
            for v in map.values_mut() {
                redact_value_tree(v, secrets);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_value_tree(v, secrets);
            }
        }
        _ => {}
    }
}

/// Redact a serialised job (job detail, MCP status): every top-level value
/// except [`JOB_IDENTIFIER_KEYS`], and every entry of `steps` except its
/// [`STEP_IDENTIFIER_KEYS`]. Fields copied out of step output — e.g.
/// `approval_message` — are covered without being named here.
pub fn redact_job_response(job: &mut Value, secrets: &[String]) {
    let Value::Object(map) = job else {
        redact_value_tree(job, secrets);
        return;
    };
    for (key, v) in map.iter_mut() {
        if key == "steps" {
            if let Value::Array(steps) = v {
                for step in steps.iter_mut() {
                    redact_object_except(step, secrets, STEP_IDENTIFIER_KEYS);
                }
            }
        } else if !JOB_IDENTIFIER_KEYS.contains(&key.as_str()) {
            redact_value_tree(v, secrets);
        }
    }
}

fn redact_object_except(v: &mut Value, secrets: &[String], skip: &[&str]) {
    match v {
        Value::Object(map) => {
            for (k, x) in map.iter_mut() {
                if !skip.contains(&k.as_str()) {
                    redact_value_tree(x, secrets);
                }
            }
        }
        other => redact_value_tree(other, secrets),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use stroem_db::{JobRow, JobStepRow};
    use uuid::Uuid;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // ── redact_value_tree (moved from web/api/jobs.rs) ─────────────────

    #[test]
    fn redact_value_tree_exact_match() {
        let mut v = json!("s3cr3t-value");
        redact_value_tree(&mut v, &["s3cr3t-value".to_string()]);
        assert_eq!(v, json!(REDACTED));
    }

    #[test]
    fn redact_value_tree_substring_and_nested() {
        let secrets = vec!["s3cr3t".to_string()];
        let mut v = json!({
            "url": "https://hooks.slack.com/s3cr3t/path",
            "nested": {"key": "s3cr3t"},
            "list": ["safe", "s3cr3t", "also-safe"]
        });
        redact_value_tree(&mut v, &secrets);
        assert_eq!(
            v["url"],
            json!(format!("https://hooks.slack.com/{REDACTED}/path"))
        );
        assert_eq!(v["nested"]["key"], json!(REDACTED));
        assert_eq!(v["list"], json!(["safe", REDACTED, "also-safe"]));
    }

    #[test]
    fn redact_value_tree_multiple_secrets_in_one_string() {
        let secrets = vec!["user123".to_string(), "pass456".to_string()];
        let mut v = json!("postgres://user123:pass456@db.host/mydb");
        redact_value_tree(&mut v, &secrets);
        assert_eq!(
            v,
            json!(format!("postgres://{REDACTED}:{REDACTED}@db.host/mydb"))
        );
    }

    #[test]
    fn redact_value_tree_masks_vals_reference_without_secrets() {
        let mut v = json!({"db": "ref+vault://secret/db#password"});
        redact_value_tree(&mut v, &[]);
        assert_eq!(v["db"], json!(REDACTED));
    }

    #[test]
    fn redact_value_tree_no_match_untouched() {
        let mut v = json!({"a": "plain", "n": 3});
        redact_value_tree(&mut v, &["zzzz".to_string()]);
        assert_eq!(v, json!({"a": "plain", "n": 3}));
    }

    // ── redact_job_response ────────────────────────────────────────────

    #[test]
    fn redact_job_response_masks_copied_approval_message() {
        let secrets = vec!["tok-123456".to_string()];
        let mut v = json!({
            "job_id": "00000000-0000-0000-0000-000000000000",
            "workspace": "etl",
            "output": {"r": "tok-123456"},
            "steps": [{
                "step_name": "gate",
                "status": "suspended",
                "output": {"approval_message": "approve tok-123456?"},
                "approval_message": "approve tok-123456?",
                "error_message": null
            }]
        });
        redact_job_response(&mut v, &secrets);
        assert_eq!(
            v["steps"][0]["approval_message"],
            json!(format!("approve {REDACTED}?"))
        );
        assert_eq!(
            v["steps"][0]["output"]["approval_message"],
            json!(format!("approve {REDACTED}?"))
        );
        assert_eq!(v["output"]["r"], json!(REDACTED));
    }

    #[test]
    fn redact_job_response_leaves_identifier_keys_alone() {
        // A secret that happens to equal an identifier must not break links.
        let secrets = vec!["prod".to_string()];
        let mut v = json!({
            "workspace": "prod",
            "task_name": "prod",
            "input": {"env": "prod"},
            "steps": [{"step_name": "prod", "status": "completed", "output": {"x": "prod"}}]
        });
        redact_job_response(&mut v, &secrets);
        assert_eq!(v["workspace"], json!("prod"));
        assert_eq!(v["task_name"], json!("prod"));
        assert_eq!(v["steps"][0]["step_name"], json!("prod"));
        assert_eq!(v["input"]["env"], json!(REDACTED));
        assert_eq!(v["steps"][0]["output"]["x"], json!(REDACTED));
    }

    /// F30: a `type: task` step's `child_jobs` are links (id, workspace, task
    /// name, status, timestamp) — a short secret must not mangle them.
    #[test]
    fn redact_job_response_leaves_child_job_links_alone() {
        let secrets = vec!["prod".to_string()];
        let child = json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "workspace": "prod",
            "task_name": "prod",
            "status": "completed",
            "created_at": "2026-10-02T00:00:00Z"
        });
        let mut v = json!({"steps": [{
            "step_name": "call",
            "action_type": "task",
            "input": {"env": "prod"},
            "child_jobs": [child.clone()]
        }]});
        redact_job_response(&mut v, &secrets);
        assert_eq!(v["steps"][0]["child_jobs"], json!([child]));
        assert_eq!(v["steps"][0]["input"]["env"], json!(REDACTED));
    }

    #[test]
    fn redact_job_response_walks_retry_history_and_error_message() {
        let secrets = vec!["my-secret-token".to_string()];
        let mut v = json!({"steps": [{
            "step_name": "deploy",
            "error_message": "failed: my-secret-token rejected",
            "retry_history": [{"attempt": 1, "error": "boom my-secret-token"}]
        }]});
        redact_job_response(&mut v, &secrets);
        assert_eq!(
            v["steps"][0]["error_message"],
            json!(format!("failed: {REDACTED} rejected"))
        );
        assert_eq!(
            v["steps"][0]["retry_history"][0]["error"],
            json!(format!("boom {REDACTED}"))
        );
    }

    // ── referenced_pins ────────────────────────────────────────────────

    #[test]
    fn referenced_pins_empty_for_unpinned_job() {
        let job = JobRow::test_default();
        let steps = vec![JobStepRow::test_default(job.job_id, "a")];
        assert!(referenced_pins(&job, &steps).is_empty());
    }

    #[test]
    fn referenced_pins_collects_job_action_and_task_pins_once() {
        let mut job = JobRow::test_default();
        job.workspace = "etl".to_string();
        job.git_ref = Some("release/2.3".to_string());
        job.revision = Some(SHA_A.to_string());

        let mut action = JobStepRow::test_default(job.job_id, "export");
        action.action_workspace = Some("billing".to_string());
        action.action_ref = Some("v4.1.0".to_string());
        action.action_revision = Some(SHA_B.to_string());

        // Same (workspace, commit) as the job pin: must not be listed twice.
        let mut task = JobStepRow::test_default(job.job_id, "child");
        task.task_workspace = Some("etl".to_string());
        task.task_ref = Some("release/2.3".to_string());
        task.task_revision = Some(SHA_A.to_string());

        let pins = referenced_pins(&job, &[action, task]);
        let got: Vec<(String, String)> = pins.into_iter().map(|(ws, p)| (ws, p.commit)).collect();
        assert_eq!(
            got,
            vec![
                ("etl".to_string(), SHA_A.to_string()),
                ("billing".to_string(), SHA_B.to_string()),
            ]
        );
    }

    #[test]
    fn referenced_pins_action_pin_defaults_to_job_workspace() {
        let mut job = JobRow::test_default();
        job.workspace = "etl".to_string();
        let mut step = JobStepRow::test_default(Uuid::new_v4(), "s");
        step.action_ref = Some("release/2.3".to_string());
        step.action_revision = Some(SHA_A.to_string());
        let pins = referenced_pins(&job, &[step]);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].0, "etl");
    }
}
