//! One owner for the step render context.
//!
//! Every template field a step can carry (`input:`, action body, `image:`,
//! agent prompts, `when:`/`for_each:`, `type: task` input, approval message)
//! renders against a context built HERE and nowhere else. The context is
//! opaque: no caller can patch a variable in afterwards.
//!
//! Spec: docs/superpowers/specs/2026-09-11-render-context-owner-design.md §3.

use std::collections::HashMap;

use serde_json::{json, Map, Value};
use sqlx::PgPool;
use stroem_common::file_field::JobFiles;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::InputFieldDef;
use stroem_common::secret::Secret;
use stroem_db::{JobStepRow, TaskStateRepo, WorkspaceStateRepo};
use uuid::Uuid;

/// The framework's own template variables. A flow step whose sanitized name
/// equals one of these collides with it (spec §3.3 rule 1, §6).
pub const FRAMEWORK_KEYS: [&str; 6] = ["input", "secret", "state", "global_state", "job", "each"];

/// Tera 2 keywords: a step with one of these (sanitized) names cannot be
/// referenced in a template (spec 2026-10-06 § 3.8). tera-2.4.0
/// `parsing/parser.rs` `RESERVED_NAMES` (incl. the capitalised `True`,
/// `False`, `None` literals) plus `if` / `else` of the inline if-expression.
pub const TERA_KEYWORDS: [&str; 18] = [
    "none", "None", "null", "self", "loop", "break", "continue", "true", "True", "false", "False",
    "and", "or", "not", "is", "in", "if", "else",
];

/// One resolved snapshot row. `json` is the persisted sidecar copy
/// (migration 047); `None` when the row has none, could not be parsed, or
/// predates 047. `storage_key`/`has_json` are what the worker needs to
/// download, independent of `json`.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: Uuid,
    pub storage_key: String,
    pub has_json: bool,
    pub json: Option<Value>,
}

/// What every render of one entry (advance / init / claim) samples once and
/// threads down by reference, for one job: the task + global snapshots of
/// its `(workspace, task)` and its attached input files.
#[derive(Debug, Clone, Default)]
pub struct Snapshots {
    pub task: Option<Snapshot>,
    pub global: Option<Snapshot>,
    /// The job's `job_input_file` rows under the entry's inputs root (spec
    /// 2026-10-29 § 5.2): canonical `/inputs` for advance and init, the
    /// claim root at claim. [`build`] maps a `file` value to its path from
    /// these and nowhere else. Default: no files.
    pub files: JobFiles,
}

/// Resolve the latest task and global snapshots. Pool only — no archive.
/// Best-effort: a lookup error is logged and yields `None` for that side.
///
/// `entry` labels the `stroem_snapshot_resolve_seconds` histogram (spec
/// §3.4) with which entry point resolved this snapshot — `"claim"`,
/// `"advance"` or `"init"` in production, `"test"` in tests.
#[tracing::instrument(skip(pool))]
pub async fn latest_snapshots(
    pool: &PgPool,
    workspace: &str,
    task_name: &str,
    git_ref: Option<&str>,
    entry: &'static str,
) -> Snapshots {
    let started = std::time::Instant::now();
    let task = match TaskStateRepo::get_latest_for_ref(pool, workspace, task_name, git_ref).await {
        Ok(row) => row.map(|r| Snapshot {
            id: r.id,
            storage_key: r.storage_key,
            has_json: r.has_json,
            json: r.state_json,
        }),
        Err(e) => {
            tracing::warn!(
                workspace,
                task_name,
                git_ref,
                "Failed to look up task state snapshot: {:#}",
                e
            );
            None
        }
    };
    let global = match WorkspaceStateRepo::get_latest_for_ref(pool, workspace, git_ref).await {
        Ok(row) => row.map(|r| Snapshot {
            id: r.id,
            storage_key: r.storage_key,
            has_json: r.has_json,
            json: r.state_json,
        }),
        Err(e) => {
            tracing::warn!(
                workspace,
                git_ref,
                "Failed to look up global state snapshot: {:#}",
                e
            );
            None
        }
    };
    metrics::histogram!(crate::metrics::STROEM_SNAPSHOT_RESOLVE_SECONDS, "entry" => entry)
        .record(started.elapsed().as_secs_f64());
    Snapshots {
        task,
        global,
        files: JobFiles::empty(),
    }
}

/// The fields the context needs from a step row.
#[derive(Debug, Clone, Copy)]
pub struct StepView<'a> {
    pub step_name: &'a str,
    pub status: &'a str,
    pub output: Option<&'a Value>,
    pub error_message: Option<&'a str>,
    pub loop_source: Option<&'a str>,
    /// A `for_each` placeholder (never a loop instance — those are already
    /// skipped above by `loop_source`). Scopes the failed-row output
    /// exposure below to a tolerated loop rollup only, never an ordinary
    /// failed step (spec §4/§12).
    pub is_placeholder: bool,
}

impl<'a> From<&'a JobStepRow> for StepView<'a> {
    fn from(r: &'a JobStepRow) -> Self {
        StepView {
            step_name: &r.step_name,
            status: &r.status,
            output: r.output.as_ref(),
            error_message: r.error_message.as_deref(),
            loop_source: r.loop_source.as_deref(),
            is_placeholder: r.for_each_expr.is_some(),
        }
    }
}

/// Project a slice of rows. Callers that mutate rows between renders (the
/// cascade) call this again each time.
pub fn views(rows: &[JobStepRow]) -> Vec<StepView<'_>> {
    rows.iter().map(StepView::from).collect()
}

/// The `each` variable of a loop instance.
#[derive(Debug, Clone, Copy)]
pub struct LoopSlot<'a> {
    pub item: &'a Value,
    pub index: i32,
    pub total: Option<i32>,
}

impl<'a> LoopSlot<'a> {
    /// `Some` only for a loop instance (both `loop_item` and `loop_index` set).
    pub fn of(row: &'a JobStepRow) -> Option<LoopSlot<'a>> {
        match (&row.loop_item, row.loop_index) {
            (Some(item), Some(index)) => Some(LoopSlot {
                item,
                index,
                total: row.loop_total,
            }),
            _ => None,
        }
    }
}

/// Per entry: what every render in that entry shares. All borrows.
#[derive(Debug, Clone, Copy)]
pub struct JobContext<'a> {
    /// For collision warnings only.
    pub job_id: Uuid,
    pub job_input: Option<&'a Value>,
    pub caller_secrets: &'a HashMap<String, Value>,
    pub owner_secrets: &'a HashMap<String, Value>,
    pub snapshots: &'a Snapshots,
    pub job_revision: Option<&'a str>,
    /// `job.git_ref` — the ref a pinned job runs at; `None` for unpinned jobs.
    pub job_ref: Option<&'a str>,
}

/// Which field is being rendered. The ONLY thing `build` matches on.
/// A variant that needs a value the caller must have produced first carries
/// it, so the requirement is in the type.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// Flow step `input:` (S1). `input` = job input, caller secrets.
    StepInput,
    /// Action `env`/`cmd`/`script`/`source`/`manifest`/`args`/`image` (S2+S3).
    /// `input` = the prepared step input, OWNER secrets. `action_schema` is
    /// the step's persisted `action_spec.input`: its `file` fields render as
    /// paths; `None` leaves the prepared input as it is.
    ActionBody {
        prepared_input: Option<&'a Value>,
        action_schema: Option<&'a HashMap<String, InputFieldDef>>,
    },
    /// Agent `prompt`/`system_prompt` (S4). Job input, caller secrets.
    AgentPrompt,
    /// `when:` / `for_each:` (S5). Job input, caller secrets, never `each`.
    Condition,
    /// `type: task` step `input:` (S6). Job input, caller secrets.
    ChildTaskInput,
    /// Approval `message:` (S6). `rendered_input`, when a nonempty object,
    /// replaces `input` AFTER step entries; otherwise job input.
    ApprovalMessage { rendered_input: Option<&'a Value> },
    /// A flow step's templated `ref:` (spec 2026-10-15 § 4.2 rule 2). Job
    /// input and NO `secret`: a ref is an identifier and is never masked.
    /// `each` for a loop instance.
    StepRef,
    /// A `type: task` action's templated `ref:` (§ 5.6): `input` is the
    /// schema-free ref projection of the caller's step input; `job`; nothing
    /// else (§ 4.1) — no `secret`, no state, no step entries, no `each`.
    TaskActionRef { ref_input: &'a Value },
}

/// A flow step whose sanitized name equals a framework key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub step: String,
    pub key: &'static str,
}

/// Opaque. The only constructors are [`build`] and [`webhook_ref`]. Holds rendered secrets, so the
/// value is `Secret`-wrapped and `Debug` prints nothing of it.
#[derive(Debug)]
pub struct RenderContext {
    value: Secret<Value>,
    collisions: Vec<Collision>,
}

impl RenderContext {
    pub fn as_value(&self) -> &Value {
        self.value.expose_secret()
    }
    pub fn collisions(&self) -> &[Collision] {
        &self.collisions
    }
    /// Job-log lines for the collisions, for callers with async log access.
    pub fn log_lines(&self) -> Vec<String> {
        self.collisions
            .iter()
            .map(|c| {
                if TERA_KEYWORDS.contains(&c.key) {
                    format!("[render] step '{}' is a Tera keyword and cannot be referenced in templates", c.step)
                } else {
                    format!(
                        "[render] step '{}' shadows template variable '{}'",
                        c.step, c.key
                    )
                }
            })
            .collect()
    }
}

/// Build the `job` template variable: `{{ job.revision }}`, `{{ job.ref }}`.
pub fn job_context(revision: Option<&str>, git_ref: Option<&str>) -> Value {
    json!({ "revision": revision, "ref": git_ref })
}

/// Insert-or-replace-in-place into an ordered `(key, value)` accumulator.
/// Mirrors `serde_json::Map::insert` under the `preserve_order` feature: a
/// repeated key keeps its original position instead of moving to the end.
fn upsert(entries: &mut Vec<(String, Value)>, key: &str, value: Value) {
    if let Some(existing) = entries.iter_mut().find(|(k, _)| k == key) {
        existing.1 = value;
    } else {
        entries.push((key.to_string(), value));
    }
}

/// Shared by [`build`] and, in tests, `build_ordered`: accumulates the
/// context as an ordered `Vec<(String, Value)>` (spec §3.3 rule 1's order is
/// an accumulation order, not a `serde_json::Map` iteration order — this
/// workspace's `serde_json` is not built with `preserve_order`) plus the
/// collisions found along the way.
fn build_entries(
    job: &JobContext<'_>,
    steps: &[StepView<'_>],
    loop_slot: Option<LoopSlot<'_>>,
    scope: Scope<'_>,
) -> (Vec<(String, Value)>, Vec<Collision>) {
    let mut ctx: Vec<(String, Value)> = Vec::new();
    let mut collisions = Vec::new();
    // The ONLY place a `file` value becomes its path (spec 2026-10-29 § 5.1):
    // the stored value is the file id, the template value
    // `{root}/{file_id}/{safe_name}` under the entry's root.
    let files = &job.snapshots.files;

    // § 4.1: a task action's ref sees its projection and `job`, nothing else.
    if let Scope::TaskActionRef { ref_input } = scope {
        upsert(&mut ctx, "input", files.template_input(ref_input));
        upsert(&mut ctx, "job", job_context(job.job_revision, job.job_ref));
        return (ctx, collisions);
    }

    // The two real axes (spec §3.2). The ref scopes carry no `secret`
    // (spec 2026-10-15 D10). The job input maps the job's attached fields;
    // the prepared action input maps the `file` fields of the action schema.
    let job_input = || job.job_input.map(|v| files.template_input(v));
    let (input, secrets): (Option<Value>, Option<&HashMap<String, Value>>) = match scope {
        Scope::ActionBody {
            prepared_input,
            action_schema,
        } => (
            prepared_input.map(|v| match action_schema {
                Some(schema) => files.template_prepared(v, schema),
                None => v.clone(),
            }),
            Some(job.owner_secrets),
        ),
        // TaskActionRef returned above.
        Scope::StepRef | Scope::TaskActionRef { .. } => (job_input(), None),
        Scope::StepInput
        | Scope::AgentPrompt
        | Scope::Condition
        | Scope::ChildTaskInput
        | Scope::ApprovalMessage { .. } => (job_input(), Some(job.caller_secrets)),
    };

    if let Some(v) = input {
        upsert(&mut ctx, "input", v);
    }
    if let Some(secrets) = secrets {
        upsert(
            &mut ctx,
            "secret",
            serde_json::to_value(secrets).unwrap_or_else(|_| json!({})),
        );
    }
    // C2 (spec 2026-10-06 § 3.6): `null`, not absent, when there is no
    // snapshot — Tera 2 errors on `state.x | default(..)` when `state` is
    // undefined but not when it is null.
    let task_state = job.snapshots.task.as_ref().and_then(|s| s.json.clone());
    upsert(&mut ctx, "state", task_state.unwrap_or(Value::Null));
    let global_state = job.snapshots.global.as_ref().and_then(|s| s.json.clone());
    upsert(
        &mut ctx,
        "global_state",
        global_state.unwrap_or(Value::Null),
    );
    // Before step outputs so a step literally named `job` shadows it.
    upsert(&mut ctx, "job", job_context(job.job_revision, job.job_ref));

    let completed = StepStatus::Completed.as_ref();
    let skipped = StepStatus::Skipped.as_ref();
    let failed = StepStatus::Failed.as_ref();
    let suspended = StepStatus::Suspended.as_ref();
    for s in steps {
        // Only the placeholder's aggregate represents a loop; instances are
        // unaddressable (`name[i]`) and skipped.
        if s.loop_source.is_some() {
            continue;
        }
        let mut entry = Map::new();
        if s.status == completed {
            entry.insert("output".into(), s.output.cloned().unwrap_or(Value::Null));
        } else if s.status == skipped || s.status == suspended {
            entry.insert("output".into(), Value::Null);
        } else if s.status == failed {
            // Scoped to loop placeholders only (spec §4/§12) — an ordinary
            // failed step's output (e.g. a rejected approval's stored
            // message) must stay masked; only a tolerated loop rollup's
            // output is newly exposed here.
            let output = if s.is_placeholder {
                s.output.cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            entry.insert("output".into(), output);
            if let Some(err) = s.error_message {
                entry.insert("error".into(), Value::String(err.to_string()));
            }
        } else {
            continue;
        }
        let name = s.step_name.replace('-', "_");
        if let Some(key) = FRAMEWORK_KEYS.iter().find(|k| **k == name) {
            collisions.push(Collision {
                step: s.step_name.to_string(),
                key,
            });
        }
        if let Some(key) = TERA_KEYWORDS.iter().find(|k| **k == name) {
            collisions.push(Collision {
                step: s.step_name.to_string(),
                key,
            });
        }
        upsert(&mut ctx, &name, Value::Object(entry));
    }

    if !matches!(scope, Scope::Condition) {
        if let Some(slot) = loop_slot {
            upsert(
                &mut ctx,
                "each",
                json!({ "item": slot.item, "index": slot.index, "total": slot.total }),
            );
        }
    }

    if let Scope::ApprovalMessage {
        rendered_input: Some(v),
    } = scope
    {
        if v.as_object().is_some_and(|m| !m.is_empty()) {
            upsert(&mut ctx, "input", v.clone());
        }
    }

    (ctx, collisions)
}

/// Build the render context for one field of one step.
///
/// Insertion order (spec §3.3 rule 1): `input`, `secret`, `state`,
/// `global_state`, `job`, step entries, `each`, then (ApprovalMessage only)
/// the rendered input. A step named like one of the first five keys shadows
/// it; `each` and the approval mapping shadow a step. Every collision is
/// recorded and `debug!`ed here — this runs on every cascade pass, not just
/// claim, so it must not warn on every pass. The claim path (the only caller
/// with job-log access) re-emits a `warn!` per deduplicated line when it
/// flushes `RenderContext::log_lines()` to the job log.
pub fn build(
    job: &JobContext<'_>,
    steps: &[StepView<'_>],
    loop_slot: Option<LoopSlot<'_>>,
    scope: Scope<'_>,
) -> RenderContext {
    let (entries, collisions) = build_entries(job, steps, loop_slot, scope);
    let ctx: Map<String, Value> = entries.into_iter().collect();

    for c in &collisions {
        tracing::debug!(
            job_id = %job.job_id,
            step = %c.step,
            key = c.key,
            "flow step name shadows a template variable"
        );
    }

    RenderContext {
        value: Secret::new(Value::Object(ctx)),
        collisions,
    }
}

/// The context of a webhook trigger's templated `ref:` (spec 2026-10-15 § 6):
/// the webhook job input (`body`, `headers`, `method`, `query` and the
/// trigger's `input` defaults) and nothing else — no job exists yet, so
/// [`build`], which takes a [`JobContext`], does not fit.
pub fn webhook_ref(input: &Value) -> RenderContext {
    let mut ctx = Map::new();
    ctx.insert("input".to_string(), input.clone());
    RenderContext {
        value: Secret::new(Value::Object(ctx)),
        collisions: Vec::new(),
    }
}

/// Test-only: same accumulation as [`build`], but exposes the insertion
/// order directly — this workspace's `serde_json::Map` does not preserve
/// insertion order (no `preserve_order` feature), so tests that assert on
/// key order cannot read it back off `RenderContext::as_value()`.
#[cfg(test)]
pub(crate) fn build_ordered(
    job: &JobContext<'_>,
    steps: &[StepView<'_>],
    loop_slot: Option<LoopSlot<'_>>,
    scope: Scope<'_>,
) -> Vec<(String, Value)> {
    build_entries(job, steps, loop_slot, scope).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use stroem_db::JobStepRow;

    fn row(name: &str, status: &str, output: Option<serde_json::Value>) -> JobStepRow {
        let mut r = JobStepRow::test_default(uuid::Uuid::nil(), name);
        r.status = status.to_string();
        r.output = output;
        r
    }
    fn failed(name: &str, err: &str) -> JobStepRow {
        let mut r = row(name, "failed", None);
        r.error_message = Some(err.to_string());
        r
    }
    fn instance(name: &str, source: &str, idx: i32, item: serde_json::Value) -> JobStepRow {
        let mut r = row(name, "completed", Some(json!("inst")));
        r.loop_source = Some(source.to_string());
        r.loop_index = Some(idx);
        r.loop_total = Some(3);
        r.loop_item = Some(item);
        r
    }
    fn secrets(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect()
    }
    fn snaps(task: Option<serde_json::Value>, global: Option<serde_json::Value>) -> Snapshots {
        let mk = |j: Option<serde_json::Value>| {
            Some(Snapshot {
                id: uuid::Uuid::nil(),
                storage_key: "k".into(),
                has_json: true,
                json: j,
            })
        };
        Snapshots {
            task: mk(task),
            global: mk(global),
            ..Default::default()
        }
    }
    /// Every scope, in one place, so a new variant cannot be forgotten.
    fn all_scopes<'a>(prepared: &'a serde_json::Value) -> Vec<Scope<'a>> {
        vec![
            Scope::StepInput,
            Scope::ActionBody {
                prepared_input: Some(prepared),
                action_schema: None,
            },
            Scope::AgentPrompt,
            Scope::Condition,
            Scope::ChildTaskInput,
            Scope::ApprovalMessage {
                rendered_input: Some(prepared),
            },
        ]
    }

    /// The ref scopes (spec 2026-10-15 § 4.2 rule 2): none carries `secret`.
    fn ref_scopes<'a>(ref_input: &'a serde_json::Value) -> Vec<Scope<'a>> {
        vec![Scope::StepRef, Scope::TaskActionRef { ref_input }]
    }

    /// Forces every new `Scope` variant into exactly one of the two lists
    /// above: the match is exhaustive, so a new variant does not compile here.
    #[allow(dead_code)]
    fn every_scope_is_listed(scope: Scope<'_>) -> &'static str {
        match scope {
            Scope::StepInput
            | Scope::ActionBody { .. }
            | Scope::AgentPrompt
            | Scope::Condition
            | Scope::ChildTaskInput
            | Scope::ApprovalMessage { .. } => "all_scopes",
            Scope::StepRef | Scope::TaskActionRef { .. } => "ref_scopes",
        }
    }

    #[test]
    fn ref_scopes_never_carry_secret() {
        let job_input = json!({"branch": "release/2.3"});
        let caller = secrets(&[("token", "s3cr3t-value")]);
        let sn = snaps(None, None);
        let ref_input = json!({"branch": "main"});
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        for scope in ref_scopes(&ref_input) {
            let label = format!("{scope:?}");
            let v = build(&job, &[], None, scope);
            assert!(
                v.as_value().get("secret").is_none(),
                "{label} carries secret"
            );
            assert!(
                !v.as_value().to_string().contains("s3cr3t-value"),
                "{label}"
            );
        }
        let v = webhook_ref(&json!({"body": {"ref": "main"}}));
        assert!(v.as_value().get("secret").is_none());
    }

    #[test]
    fn step_ref_carries_job_input_state_steps_and_each_in_order() {
        let job_input = json!({"branch": "release/2.3"});
        let caller = secrets(&[("c", "1")]);
        let sn = snaps(Some(json!({"s": 1})), Some(json!({"g": 1})));
        let rows = vec![row("pick", "completed", Some(json!({"branch": "main"})))];
        let views = views(&rows);
        let inst = instance("run[0]", "run", 0, json!("release/2.3"));
        let slot = LoopSlot::of(&inst);
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: Some("rev"),
            job_ref: None,
        };
        let ordered = build_ordered(&job, &views, slot, Scope::StepRef);
        let keys: Vec<&str> = ordered.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec!["input", "state", "global_state", "job", "pick", "each"]
        );
        let v = build(&job, &views, slot, Scope::StepRef);
        assert_eq!(v.as_value()["input"]["branch"], "release/2.3");
        assert_eq!(v.as_value()["pick"]["output"]["branch"], "main");
        assert_eq!(v.as_value()["each"]["item"], "release/2.3");
        // No loop slot → no `each`.
        let v = build(&job, &views, None, Scope::StepRef);
        assert!(v.as_value().get("each").is_none());
    }

    #[test]
    fn task_action_ref_carries_only_the_projection_and_job() {
        let job_input = json!({"branch": "release/2.3"});
        let caller = secrets(&[("c", "1")]);
        let sn = snaps(Some(json!({"s": 1})), None);
        let rows = vec![row("pick", "completed", Some(json!({"x": 1})))];
        let views = views(&rows);
        let inst = instance("call[0]", "call", 0, json!("i"));
        let ref_input = json!({"branch": "main"});
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: Some("rev"),
            job_ref: Some("release/2.3"),
        };
        let ordered = build_ordered(
            &job,
            &views,
            LoopSlot::of(&inst),
            Scope::TaskActionRef {
                ref_input: &ref_input,
            },
        );
        let keys: Vec<&str> = ordered.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["input", "job"]);
        assert_eq!(ordered[0].1, json!({"branch": "main"}));
        assert_eq!(ordered[1].1["ref"], "release/2.3");
    }

    #[test]
    fn webhook_ref_carries_only_the_webhook_input() {
        let input = json!({"body": {"ref": "main"}, "method": "POST"});
        let v = webhook_ref(&input);
        assert_eq!(v.as_value(), &json!({"input": input}));
        assert!(v.collisions().is_empty());
    }

    #[test]
    fn step_ref_records_collisions_like_every_scope() {
        let caller = secrets(&[]);
        let sn = snaps(None, None);
        let rows = vec![row("input", "completed", Some(json!(1)))];
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &views(&rows), None, Scope::StepRef);
        assert!(v
            .collisions()
            .iter()
            .any(|c| c.step == "input" && c.key == "input"));
    }

    // ── The anti-regression test (spec §5): every rule-1 key, every scope ──
    //
    // `serde_json` in this workspace is NOT built with the `preserve_order`
    // feature (`grep -c preserve_order Cargo.lock` == 0), so `Map::keys()`
    // does not reflect insertion order — it's a `BTreeMap`, alphabetical.
    // Order is instead asserted against `build_ordered`'s `Vec<(String,
    // Value)>`, which `build` itself accumulates before converting to a
    // `Map` (see the module body).
    #[test]
    fn every_scope_carries_every_framework_key_in_order() {
        let job_input = json!({"a": 1});
        let caller = secrets(&[("c", "1")]);
        let owner = secrets(&[("o", "2")]);
        let sn = snaps(Some(json!({"s": 1})), Some(json!({"g": 1})));
        let prepared = json!({"p": 1});
        let rows = vec![row("prev", "completed", Some(json!({"x": 1})))];
        let views = views(&rows);
        let inst = instance("prev[0]", "prev", 0, json!("i"));
        let slot = LoopSlot::of(&inst);
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &owner,
            snapshots: &sn,
            job_revision: Some("rev"),
            job_ref: None,
        };
        for scope in all_scopes(&prepared) {
            let is_condition = matches!(scope, Scope::Condition);
            let label = format!("{scope:?}");
            let ctx = build(&job, &views, slot, scope);
            let v = ctx.as_value().as_object().unwrap();
            for key in ["input", "secret", "state", "global_state", "job"] {
                assert!(v.contains_key(key), "{label}: missing {key}");
            }
            assert!(v.contains_key("prev"), "{label}: step entry missing");
            if is_condition {
                assert!(
                    !v.contains_key("each"),
                    "{label}: Condition must never carry each"
                );
            } else {
                assert_eq!(v["each"]["item"], json!("i"), "{label}: each missing");
            }
            // Order: framework keys precede step entries; each follows them.
            let ordered = build_ordered(&job, &views, slot, scope);
            let pos = |k: &str| ordered.iter().position(|(key, _)| key == k).unwrap();
            for key in ["input", "secret", "state", "global_state", "job"] {
                assert!(
                    pos(key) < pos("prev"),
                    "{label}: {key} must precede step entries"
                );
            }
            if !is_condition {
                assert!(
                    pos("prev") < pos("each"),
                    "{label}: each must follow step entries"
                );
            }
        }
    }

    // ── Scope axis (spec §3.2) ──
    #[test]
    fn action_body_uses_prepared_input_and_owner_secrets_others_use_job_and_caller() {
        let job_input = json!({"from": "job"});
        let prepared = json!({"from": "prepared"});
        let caller = secrets(&[("who", "caller")]);
        let owner = secrets(&[("who", "owner")]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &owner,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let body = build(
            &job,
            &[],
            None,
            Scope::ActionBody {
                prepared_input: Some(&prepared),
                action_schema: None,
            },
        );
        assert_eq!(body.as_value()["input"]["from"], "prepared");
        assert_eq!(body.as_value()["secret"]["who"], "owner");
        for scope in [
            Scope::StepInput,
            Scope::AgentPrompt,
            Scope::Condition,
            Scope::ChildTaskInput,
        ] {
            let c = build(&job, &[], None, scope);
            assert_eq!(c.as_value()["input"]["from"], "job");
            assert_eq!(c.as_value()["secret"]["who"], "caller");
        }
    }

    #[test]
    fn approval_message_uses_rendered_input_only_when_nonempty_object() {
        let job_input = json!({"from": "job"});
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let none = build(
            &job,
            &[],
            None,
            Scope::ApprovalMessage {
                rendered_input: None,
            },
        );
        assert_eq!(none.as_value()["input"]["from"], "job");
        let empty = json!({});
        let e = build(
            &job,
            &[],
            None,
            Scope::ApprovalMessage {
                rendered_input: Some(&empty),
            },
        );
        assert_eq!(e.as_value()["input"]["from"], "job");
        let mapped = json!({"foo": "approved"});
        let m = build(
            &job,
            &[],
            None,
            Scope::ApprovalMessage {
                rendered_input: Some(&mapped),
            },
        );
        assert_eq!(m.as_value()["input"]["foo"], "approved");
        // …and the mapping beats a completed step named `input` (inserted after steps).
        let rows = vec![row("input", "completed", Some(json!({"stepwins": true})))];
        let m2 = build(
            &job,
            &views(&rows),
            None,
            Scope::ApprovalMessage {
                rendered_input: Some(&mapped),
            },
        );
        assert_eq!(m2.as_value()["input"]["foo"], "approved");
        assert!(m2
            .collisions()
            .iter()
            .any(|c| c.step == "input" && c.key == "input"));
    }

    // ── Rules (spec §3.3) ──
    #[test]
    fn step_entries_cover_all_four_statuses_with_masked_output_and_error() {
        let rows = vec![
            row("done", "completed", Some(json!({"k": "v"}))),
            row("done_null", "completed", None),
            row("skip", "skipped", None),
            failed("boom", "kaput"),
            row(
                "wait",
                "suspended",
                Some(json!({"approval_message": "SECRET-ish"})),
            ),
            row("pend", "pending", Some(json!(1))),
            row("run", "running", None),
        ];
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &views(&rows), None, Scope::Condition);
        let v = v.as_value();
        assert_eq!(v["done"]["output"]["k"], "v");
        assert_eq!(v["done_null"]["output"], serde_json::Value::Null);
        assert_eq!(v["skip"]["output"], serde_json::Value::Null);
        assert_eq!(v["boom"]["output"], serde_json::Value::Null);
        assert_eq!(v["boom"]["error"], "kaput");
        // suspended rows keep their stored output MASKED (rule 3)
        assert_eq!(v["wait"]["output"], serde_json::Value::Null);
        assert!(v.get("pend").is_none() && v.get("run").is_none());
    }

    #[test]
    fn step_view_carries_is_placeholder_from_for_each_expr() {
        let mut r = row("p", "failed", None);
        r.for_each_expr = Some("{{ items }}".to_string());
        assert!(StepView::from(&r).is_placeholder);

        let ordinary = row("a", "failed", None);
        assert!(!StepView::from(&ordinary).is_placeholder);
    }

    /// spec §4/§12: a failed loop placeholder's output is no longer masked —
    /// cascade.rs's R6 now builds the output array even on a failed rollup,
    /// and it must reach templates, not render as `null`.
    #[test]
    fn a_failed_loop_placeholder_exposes_its_output() {
        let mut r = row("p", "failed", Some(json!([{"n": 1}, null])));
        r.for_each_expr = Some("{{ items }}".to_string());
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &views(&[r]), None, Scope::Condition);
        assert_eq!(v.as_value()["p"]["output"], json!([{"n": 1}, null]));
    }

    /// A rejected approval step already stores `{"approval_message": ...}`
    /// before failing — this must stay masked. Pins the scope boundary from
    /// spec §4/§12: this is NOT a blanket completed/failed output merge.
    #[test]
    fn an_ordinary_failed_step_still_renders_null_even_with_retained_output() {
        let r = row(
            "approval",
            "failed",
            Some(json!({"approval_message": "please confirm"})),
        );
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &views(&[r]), None, Scope::Condition);
        assert_eq!(v.as_value()["approval"]["output"], serde_json::Value::Null);
    }

    /// Migrated from the deleted `job_creator` context builder's tests: a failed
    /// step with no `error_message` carries `output: null` and NO `error` key,
    /// so `{{ x.error }}` in a downstream `when` stays undefined rather than
    /// rendering an empty string.
    #[test]
    fn failed_step_without_error_message_has_null_output_and_no_error_key() {
        let rows = vec![row("boom", "failed", None)];
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &views(&rows), None, Scope::Condition);
        assert_eq!(v.as_value()["boom"]["output"], serde_json::Value::Null);
        assert!(v.as_value()["boom"].get("error").is_none());
    }

    #[test]
    fn loop_instance_rows_are_skipped_and_hyphens_sanitized() {
        let rows = vec![
            row("my-step", "completed", Some(json!(["agg"]))),
            instance("my-step[0]", "my-step", 0, json!("a")),
        ];
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &views(&rows), None, Scope::StepInput);
        assert_eq!(v.as_value()["my_step"]["output"], json!(["agg"]));
        assert!(v.as_value().get("my-step[0]").is_none());
        assert!(v.as_value().get("my_step[0]").is_none());
    }

    #[test]
    fn secret_always_present_state_null_without_json() {
        let caller = secrets(&[]);
        let sn = snaps(None, Some(json!({"g": 1})));
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &[], None, Scope::StepInput);
        assert_eq!(v.as_value()["secret"], json!({}));
        assert_eq!(v.as_value().get("state"), Some(&Value::Null));
        assert_eq!(v.as_value()["global_state"]["g"], 1);
    }

    #[test]
    fn absent_snapshot_renders_state_as_null() {
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let ctx = build(&job, &[], None, Scope::StepInput);
        let v = ctx.as_value();
        let r = |t: &str| stroem_common::template::render_template(t, v).unwrap();
        assert_eq!(r("{{ state.cursor | default(value=0) }}"), "0");
        assert_eq!(
            r("{{ global_state.last | default(value='never') }}"),
            "never"
        );
        assert_eq!(r("{% if state.x %}a{% else %}b{% endif %}"), "b");
        assert_eq!(r("{{ not state }}"), "true");
    }

    #[test]
    fn step_named_after_a_tera_keyword_is_reported() {
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let rows = vec![
            row("none", "completed", Some(json!("x"))),
            row("True", "completed", Some(json!("x"))),
            row("False", "completed", Some(json!("x"))),
            row("None", "completed", Some(json!("x"))),
        ];
        let v = build(&job, &views(&rows), None, Scope::StepInput);
        let lines = v.log_lines();
        for step in ["none", "True", "False", "None"] {
            let want = format!(
                "[render] step '{step}' is a Tera keyword and cannot be referenced in templates"
            );
            assert!(lines.contains(&want), "{step}: {lines:?}");
        }
    }

    /// Every name Tera 2 reserves (tera-2.4.0 `parsing/parser.rs`
    /// `RESERVED_NAMES`) is a keyword here: a step named after one cannot
    /// be referenced, as Tera 2 itself shows.
    #[test]
    fn tera_keywords_cover_tera_2_reserved_names() {
        for name in [
            "true", "True", "false", "False", "loop", "self", "and", "or", "not", "is", "in",
            "continue", "break", "none", "None", "null",
        ] {
            assert!(TERA_KEYWORDS.contains(&name), "{name}");
        }
        let ctx = json!({"True": {"output": "x"}, "None": {"output": "x"}});
        for name in ["True", "None"] {
            let rendered =
                stroem_common::template::render_template(&format!("{{{{ {name}.output }}}}"), &ctx);
            assert!(
                rendered.as_deref().map_or(true, |r| r != "x"),
                "{name} must not be addressable: {rendered:?}"
            );
        }
    }

    #[test]
    fn job_metadata_always_present_and_shadowed_by_step_named_job() {
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: Some("abc"),
            job_ref: None,
        };
        let v = build(&job, &[], None, Scope::StepInput);
        assert_eq!(v.as_value()["job"]["revision"], "abc");
        let none = JobContext {
            job_revision: None,
            ..job
        };
        let v = build(&none, &[], None, Scope::StepInput);
        assert_eq!(v.as_value()["job"]["revision"], serde_json::Value::Null);
        let rows = vec![row("job", "completed", Some(json!({"mine": true})))];
        let v = build(&job, &views(&rows), None, Scope::StepInput);
        assert_eq!(v.as_value()["job"]["output"]["mine"], true);
        assert!(v
            .collisions()
            .iter()
            .any(|c| c.step == "job" && c.key == "job"));
    }

    #[test]
    fn job_metadata_carries_ref_of_a_pinned_job() {
        let caller = secrets(&[]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: Some("3f2a9c0e"),
            job_ref: Some("release/2.3"),
        };
        let v = build(&job, &[], None, Scope::StepInput);
        assert_eq!(v.as_value()["job"]["ref"], "release/2.3");
        assert_eq!(v.as_value()["job"]["revision"], "3f2a9c0e");
        assert_eq!(
            stroem_common::template::render_template(
                "{{ job.ref }}@{{ job.revision }}",
                v.as_value()
            )
            .unwrap(),
            "release/2.3@3f2a9c0e"
        );

        let unpinned = JobContext {
            job_ref: None,
            ..job
        };
        let v = build(&unpinned, &[], None, Scope::StepInput);
        assert_eq!(v.as_value()["job"]["ref"], serde_json::Value::Null);
        assert_eq!(
            stroem_common::template::render_template("[{{ job.ref }}]", v.as_value()).unwrap(),
            "[]",
            "an unpinned job's ref renders as an empty string"
        );
    }

    #[test]
    fn collisions_recorded_for_every_framework_key() {
        let caller = secrets(&[("k", "v")]);
        let sn = snaps(Some(json!({})), Some(json!({})));
        let job_input = json!({});
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(&job_input),
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let rows: Vec<JobStepRow> = ["input", "secret", "state", "global_state", "job", "each"]
            .iter()
            .map(|n| row(n, "completed", Some(json!("step"))))
            .collect();
        let inst = instance("x[0]", "x", 0, json!("i"));
        let v = build(&job, &views(&rows), LoopSlot::of(&inst), Scope::StepInput);
        let mut keys: Vec<&str> = v.collisions().iter().map(|c| c.key).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["each", "global_state", "input", "job", "secret", "state"]
        );
        // step wins for the five; loop variable wins for each
        assert_eq!(v.as_value()["secret"]["output"], "step");
        assert_eq!(v.as_value()["each"]["item"], "i");
        let lines = v.log_lines();
        assert!(lines
            .iter()
            .any(|l| l == "[render] step 'secret' shadows template variable 'secret'"));
    }

    #[test]
    fn render_context_debug_does_not_print_contents() {
        let caller = secrets(&[("PW", "SUPER-SECRET-7f3a")]);
        let sn = Snapshots::default();
        let job = JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: None,
            caller_secrets: &caller,
            owner_secrets: &caller,
            snapshots: &sn,
            job_revision: None,
            job_ref: None,
        };
        let v = build(&job, &[], None, Scope::StepInput);
        let dbg = format!("{v:?}");
        assert!(!dbg.contains("SUPER-SECRET-7f3a"), "leaked: {dbg}");
    }

    // ── File inputs (spec 2026-10-29 § 5.1, § 5.3): a `file` value renders
    // as `{root}/{file_id}/{safe_name}` in every scope; the root is the one
    // the entry sampled into `Snapshots.files`. ──

    use stroem_common::file_field::{FileRef, CANONICAL_INPUTS_ROOT};

    const FILE_A: &str = "file_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FILE_B: &str = "file_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CLAIM_PATH_A: &str = "/w/in/file_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/report.csv";
    const CLAIM_PATH_B: &str = "/w/in/file_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/report.csv";
    const CANONICAL_PATH_A: &str = "/inputs/file_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/report.csv";

    /// `attached` = `(task field, file id, safe name)`.
    fn file_snaps(root: &str, attached: &[(&str, &str, &str)]) -> Snapshots {
        Snapshots {
            files: JobFiles::new(
                root,
                attached.iter().map(|(field, id, name)| {
                    (
                        field.to_string(),
                        FileRef {
                            file_id: id.to_string(),
                            safe_name: name.to_string(),
                        },
                    )
                }),
            ),
            ..Default::default()
        }
    }
    fn file_job<'a>(
        input: &'a serde_json::Value,
        sn: &'a Snapshots,
        secrets: &'a HashMap<String, serde_json::Value>,
    ) -> JobContext<'a> {
        JobContext {
            job_id: uuid::Uuid::nil(),
            job_input: Some(input),
            caller_secrets: secrets,
            owner_secrets: secrets,
            snapshots: sn,
            job_revision: None,
            job_ref: None,
        }
    }
    fn render_in(ctx: &RenderContext, template: &str) -> String {
        stroem_common::template::render_template(template, ctx.as_value()).unwrap()
    }
    fn schema_of(fields: &[(&str, &str)]) -> HashMap<String, InputFieldDef> {
        fields
            .iter()
            .map(|(name, ty)| {
                (
                    name.to_string(),
                    InputFieldDef {
                        field_type: ty.to_string(),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }
    fn map_of(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect()
    }

    #[test]
    fn step_input_scope_renders_path() {
        let input = json!({"data_file": FILE_A, "name": "n"});
        let sn = file_snaps("/w/in", &[("data_file", FILE_A, "report.csv")]);
        let sec = secrets(&[]);
        let job = file_job(&input, &sn, &sec);
        let ctx = build(&job, &[], None, Scope::StepInput);
        assert_eq!(render_in(&ctx, "{{ input.data_file }}"), CLAIM_PATH_A);
        assert_eq!(render_in(&ctx, "{{ input.name }}"), "n");
        // The stored job input keeps the id.
        assert_eq!(input["data_file"], FILE_A);
    }

    #[test]
    fn action_body_scope_renders_path_for_file_fields_only() {
        let input = json!({"data_file": FILE_A});
        let sn = file_snaps("/w/in", &[("data_file", FILE_A, "report.csv")]);
        let sec = secrets(&[]);
        let job = file_job(&input, &sn, &sec);
        // `data` is a `file` field; `data_file` and `label` are strings that
        // happen to hold the id (not field-keyed: the ACTION schema decides);
        // `stray` is a `file` field naming a file the job does not have.
        let prepared = json!({
            "data": FILE_A,
            "data_file": FILE_A,
            "label": FILE_A,
            "stray": FILE_B,
            "other": "x",
        });
        let schema = schema_of(&[
            ("data", "file"),
            ("data_file", "string"),
            ("label", "string"),
            ("stray", "file"),
        ]);
        let ctx = build(
            &job,
            &[],
            None,
            Scope::ActionBody {
                prepared_input: Some(&prepared),
                action_schema: Some(&schema),
            },
        );
        let v = &ctx.as_value()["input"];
        assert_eq!(v["data"], CLAIM_PATH_A);
        assert_eq!(v["data_file"], FILE_A);
        assert_eq!(v["label"], FILE_A);
        assert_eq!(v["stray"], FILE_B);
        assert_eq!(v["other"], "x");

        // Without the action schema the prepared input is used as it is.
        let ctx = build(
            &job,
            &[],
            None,
            Scope::ActionBody {
                prepared_input: Some(&prepared),
                action_schema: None,
            },
        );
        assert_eq!(ctx.as_value()["input"], prepared);
    }

    #[test]
    fn agent_prompt_scope_renders_canonical_path() {
        let input = json!({"data_file": FILE_A});
        let sn = file_snaps(
            CANONICAL_INPUTS_ROOT,
            &[("data_file", FILE_A, "report.csv")],
        );
        let sec = secrets(&[]);
        let job = file_job(&input, &sn, &sec);
        let ctx = build(&job, &[], None, Scope::AgentPrompt);
        assert_eq!(
            render_in(&ctx, "Read {{ input.data_file }}"),
            format!("Read {CANONICAL_PATH_A}")
        );
    }

    #[test]
    fn condition_scope_absent_optional_field_stays_absent() {
        use stroem_common::template::evaluate_condition;
        let sec = secrets(&[]);

        // A file was given: the condition sees its canonical path.
        let given = json!({"data_file": FILE_A});
        let sn = file_snaps(
            CANONICAL_INPUTS_ROOT,
            &[("data_file", FILE_A, "report.csv")],
        );
        let ctx = build(&file_job(&given, &sn, &sec), &[], None, Scope::Condition);
        assert_eq!(render_in(&ctx, "{{ input.data_file }}"), CANONICAL_PATH_A);
        assert!(evaluate_condition("{{ input.data_file }}", ctx.as_value()).unwrap());

        // No file, no attachment row: absent stays absent — no key is added.
        let none = Snapshots::default();
        let absent = json!({"name": "n"});
        let ctx = build(&file_job(&absent, &none, &sec), &[], None, Scope::Condition);
        assert!(ctx.as_value()["input"].get("data_file").is_none());
        assert_eq!(render_in(&ctx, "{{ input.data_file is defined }}"), "false");

        // `null` and `""` ("no file" at creation) stay as they are: falsy.
        for empty in [json!({"data_file": null}), json!({"data_file": ""})] {
            let ctx = build(&file_job(&empty, &none, &sec), &[], None, Scope::Condition);
            assert_eq!(ctx.as_value()["input"]["data_file"], empty["data_file"]);
            assert!(!evaluate_condition("{{ input.data_file }}", ctx.as_value()).unwrap());
        }
    }

    #[test]
    fn child_task_input_scope_renders_path() {
        use stroem_common::template::render_input_typed_with_files;
        let input = json!({"data_file": FILE_A});
        let sn = file_snaps(
            CANONICAL_INPUTS_ROOT,
            &[("data_file", FILE_A, "report.csv")],
        );
        let sec = secrets(&[]);
        let job = file_job(&input, &sn, &sec);
        let ctx = build(&job, &[], None, Scope::ChildTaskInput);
        assert_eq!(render_in(&ctx, "{{ input.data_file }}"), CANONICAL_PATH_A);

        // The child task's `file` field looks the path up by id and stores
        // the id; a `string` field keeps the path.
        let step_input = map_of(&[
            ("data", "{{ input.data_file }}"),
            ("where", "{{ input.data_file }}"),
        ]);
        let child_schema = schema_of(&[("data", "file"), ("where", "string")]);
        let child = render_input_typed_with_files(
            &step_input,
            Some(&child_schema),
            ctx.as_value(),
            &sn.files,
        )
        .unwrap();
        assert_eq!(child["data"], FILE_A);
        assert_eq!(child["where"], CANONICAL_PATH_A);
    }

    #[test]
    fn approval_message_scope_renders_path_and_keeps_the_rendered_input() {
        let input = json!({"data_file": FILE_A});
        let sn = file_snaps(
            CANONICAL_INPUTS_ROOT,
            &[("data_file", FILE_A, "report.csv")],
        );
        let sec = secrets(&[]);
        let job = file_job(&input, &sn, &sec);
        let ctx = build(
            &job,
            &[],
            None,
            Scope::ApprovalMessage {
                rendered_input: None,
            },
        );
        assert_eq!(render_in(&ctx, "{{ input.data_file }}"), CANONICAL_PATH_A);
        // The rendered step input replaces `input` unchanged.
        let rendered = json!({"data_file": FILE_A});
        let ctx = build(
            &job,
            &[],
            None,
            Scope::ApprovalMessage {
                rendered_input: Some(&rendered),
            },
        );
        assert_eq!(ctx.as_value()["input"], rendered);
    }

    #[test]
    fn step_ref_and_task_action_ref_scope_render_path() {
        use stroem_common::template::render_ref_projection;
        let input = json!({"data_file": FILE_A});
        let sn = file_snaps(
            CANONICAL_INPUTS_ROOT,
            &[("data_file", FILE_A, "report.csv")],
        );
        let sec = secrets(&[]);
        let job = file_job(&input, &sn, &sec);

        let ctx = build(&job, &[], None, Scope::StepRef);
        assert_eq!(render_in(&ctx, "{{ input.data_file }}"), CANONICAL_PATH_A);

        // A task action's ref input is the projection of the step input,
        // rendered in the step-input context: it already holds the path.
        let child_ctx = build(&job, &[], None, Scope::ChildTaskInput);
        let ref_input = render_ref_projection(
            &map_of(&[("src", "{{ input.data_file }}")]),
            child_ctx.as_value(),
        )
        .unwrap();
        let ctx = build(
            &job,
            &[],
            None,
            Scope::TaskActionRef {
                ref_input: &ref_input,
            },
        );
        assert_eq!(render_in(&ctx, "{{ input.src }}"), CANONICAL_PATH_A);
        // A projection key named like the task field that still holds that
        // field's bare id renders the path too.
        let bare = json!({"data_file": FILE_A});
        let ctx = build(&job, &[], None, Scope::TaskActionRef { ref_input: &bare });
        assert_eq!(render_in(&ctx, "{{ input.data_file }}"), CANONICAL_PATH_A);
    }

    /// Spec revision 4: two uploads with the same name in task fields `a` and
    /// `b`; action field `a` (`file`) is given `{{ input.b }}`. One file has
    /// one path in every context of the step, and two files never share one.
    #[test]
    fn collision_case_two_files_same_name() {
        use stroem_common::template::render_input_typed_with_files;
        let input = json!({"a": FILE_A, "b": FILE_B});
        let sn = file_snaps(
            "/w/in",
            &[("a", FILE_A, "report.csv"), ("b", FILE_B, "report.csv")],
        );
        let sec = secrets(&[]);
        let job = file_job(&input, &sn, &sec);

        // S1: the job input.
        let s1 = build(&job, &[], None, Scope::StepInput);
        assert_eq!(render_in(&s1, "{{ input.b }}"), CLAIM_PATH_B);
        assert_eq!(render_in(&s1, "{{ input.a }}"), CLAIM_PATH_A);

        // A3: action field `a` resolves the path of B by id.
        let schema = schema_of(&[("a", "file")]);
        let prepared = render_input_typed_with_files(
            &map_of(&[("a", "{{ input.b }}")]),
            Some(&schema),
            s1.as_value(),
            &sn.files,
        )
        .unwrap();
        assert_eq!(prepared["a"], FILE_B);

        // S2/S3: the action's `a` is B's path, not A's.
        let s2 = build(
            &job,
            &[],
            None,
            Scope::ActionBody {
                prepared_input: Some(&prepared),
                action_schema: Some(&schema),
            },
        );
        assert_eq!(render_in(&s2, "{{ input.a }}"), CLAIM_PATH_B);
        assert_ne!(CLAIM_PATH_A, CLAIM_PATH_B);
    }
}
