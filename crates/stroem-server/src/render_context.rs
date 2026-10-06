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
use stroem_common::models::job::StepStatus;
use stroem_common::secret::Secret;
use stroem_db::{JobStepRow, TaskStateRepo, WorkspaceStateRepo};
use uuid::Uuid;

/// The framework's own template variables. A flow step whose sanitized name
/// equals one of these collides with it (spec §3.3 rule 1, §6).
pub const FRAMEWORK_KEYS: [&str; 6] = ["input", "secret", "state", "global_state", "job", "each"];

/// Tera 2 keywords: a step with one of these (sanitized) names cannot be
/// referenced in a template (spec 2026-10-06 § 3.8).
pub const TERA_KEYWORDS: [&str; 15] = [
    "none", "null", "self", "loop", "break", "continue", "true", "false", "and", "or", "not", "is",
    "in", "if", "else",
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

/// The task + global snapshots for one `(workspace, task)`, resolved once
/// per entry (advance / init / claim) and threaded down by reference.
#[derive(Debug, Clone, Default)]
pub struct Snapshots {
    pub task: Option<Snapshot>,
    pub global: Option<Snapshot>,
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
    Snapshots { task, global }
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
    /// `input` = the prepared step input, OWNER secrets.
    ActionBody { prepared_input: Option<&'a Value> },
    /// Agent `prompt`/`system_prompt` (S4). Job input, caller secrets.
    AgentPrompt,
    /// `when:` / `for_each:` (S5). Job input, caller secrets, never `each`.
    Condition,
    /// `type: task` step `input:` (S6). Job input, caller secrets.
    ChildTaskInput,
    /// Approval `message:` (S6). `rendered_input`, when a nonempty object,
    /// replaces `input` AFTER step entries; otherwise job input.
    ApprovalMessage { rendered_input: Option<&'a Value> },
}

/// A flow step whose sanitized name equals a framework key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub step: String,
    pub key: &'static str,
}

/// Opaque. The only constructor is [`build`]. Holds rendered secrets, so the
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

    // The two real axes (spec §3.2).
    let (input, secrets): (Option<&Value>, &HashMap<String, Value>) = match scope {
        Scope::ActionBody { prepared_input } => (prepared_input, job.owner_secrets),
        Scope::StepInput
        | Scope::AgentPrompt
        | Scope::Condition
        | Scope::ChildTaskInput
        | Scope::ApprovalMessage { .. } => (job.job_input, job.caller_secrets),
    };

    if let Some(v) = input {
        upsert(&mut ctx, "input", v.clone());
    }
    upsert(
        &mut ctx,
        "secret",
        serde_json::to_value(secrets).unwrap_or_else(|_| json!({})),
    );
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
        }
    }
    /// Every scope, in one place, so a new variant cannot be forgotten.
    fn all_scopes<'a>(prepared: &'a serde_json::Value) -> Vec<Scope<'a>> {
        vec![
            Scope::StepInput,
            Scope::ActionBody {
                prepared_input: Some(prepared),
            },
            Scope::AgentPrompt,
            Scope::Condition,
            Scope::ChildTaskInput,
            Scope::ApprovalMessage {
                rendered_input: Some(prepared),
            },
        ]
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
        let rows = vec![row("none", "completed", Some(json!("x")))];
        let v = build(&job, &views(&rows), None, Scope::StepInput);
        let lines = v.log_lines();
        assert!(
            lines.iter().any(|l| l
                == "[render] step 'none' is a Tera keyword and cannot be referenced in templates"),
            "{lines:?}"
        );
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
}
