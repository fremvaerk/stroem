//! Step cascade: the pure fixpoint that moves a job's non-running steps.
//! See docs/superpowers/specs/2026-09-08-step-cascade-design.md.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::gate::{gate, DepOutcome, Gate};
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_db::{JobRepo, JobRow, JobStepRepo, JobStepRow, NewJobStep};
use uuid::Uuid;

pub use stroem_common::models::job::SkipReason;

/// One state transition the cascade wants applied. Closed enum.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// pending → ready
    Promote { step: String },
    /// pending → skipped
    Skip { step: String, reason: SkipReason },
    /// pending → failed (when-evaluation or for_each-expression error)
    Fail { step: String, error: String },
    /// Placeholder pending → running; insert instance rows; job pending → running.
    Expand {
        placeholder: String,
        instances: Vec<NewJobStep>,
    },
    /// Placeholder pending → running, instances already exist (R0). No insert.
    Adopt { placeholder: String },
    /// running placeholder → completed(output) | failed(error)
    Rollup {
        placeholder: String,
        outcome: RollupOutcome,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RollupOutcome {
    Completed(Value),
    /// The loop's output array (spec §4/§12: one element per existing
    /// instance, null where it produced none) is built the same way for a
    /// failed rollup as for a completed one — never dropped to nothing just
    /// because the rollup didn't tolerate the failure.
    Failed(String, Value),
}

/// What one `run` decided, in production order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub changes: Vec<Change>,
}

/// A step is terminal in one of these four statuses.
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "skipped" | "cancelled")
}

const PENDING: &str = "pending";
const READY: &str = "ready";
const RUNNING: &str = "running";
const COMPLETED: &str = "completed";
const FAILED: &str = "failed";
const SKIPPED: &str = "skipped";
const CANCELLED: &str = "cancelled";

/// Maximum number of for_each instances (runtime limit)
const MAX_FOR_EACH_ITEMS: usize = 10000;

/// Parse the for_each expression and return the items array.
fn parse_for_each_items(
    expr: &str,
    render_ctx: &serde_json::Value,
) -> Result<Vec<serde_json::Value>> {
    // Try parsing as a JSON literal first (for literal arrays stored as JSON strings)
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(expr) {
        match value {
            serde_json::Value::Array(arr) => return Ok(arr),
            serde_json::Value::String(template) => {
                // It's a JSON-encoded string — this is a Tera template
                return render_for_each_template(&template, render_ctx);
            }
            // A YAML literal is template SOURCE and can hold a literal
            // secret (spec 2026-10-06 § 3.3): name the JSON type only.
            other => {
                bail!(
                    "for_each must render a JSON array, got a JSON {}",
                    stroem_common::template::json_type_name(&other)
                );
            }
        }
    }

    // If not valid JSON, treat as a raw Tera template
    render_for_each_template(expr, render_ctx)
}

/// Render a Tera template and parse the result as a JSON array.
fn render_for_each_template(
    template: &str,
    render_ctx: &serde_json::Value,
) -> Result<Vec<serde_json::Value>> {
    let rendered = stroem_common::template::render_template(template, render_ctx)
        .context("Failed to render for_each template")?;
    let value: serde_json::Value = serde_json::from_str(&rendered).map_err(|e| {
        anyhow::anyhow!(
            "for_each must render a JSON array; the rendered text ({} bytes) is not valid JSON \
             ({:?} error at line {}, column {}). Render arrays and objects with `| json_encode()`, \
             e.g. {{{{ step.output.items | json_encode() }}}}",
            rendered.len(),
            e.classify(),
            e.line(),
            e.column()
        )
    })?;
    match value {
        serde_json::Value::Array(arr) => Ok(arr),
        other => bail!(
            "for_each must render a JSON array, got a JSON {}",
            stroem_common::template::json_type_name(&other)
        ),
    }
}

/// In-memory copy of a job's step rows that the fixpoint mutates.
struct Snapshot {
    rows: Vec<JobStepRow>,
    index: HashMap<String, usize>,
}

impl Snapshot {
    fn new(rows: Vec<JobStepRow>) -> Self {
        let index = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.step_name.clone(), i))
            .collect();
        Self { rows, index }
    }

    fn status(&self, name: &str) -> Option<&str> {
        self.index.get(name).map(|&i| self.rows[i].status.as_str())
    }

    #[cfg(test)]
    fn skip_reason(&self, name: &str) -> Option<&str> {
        self.index
            .get(name)
            .and_then(|&i| self.rows[i].skip_reason.as_deref())
    }

    fn outcome(&self, name: &str) -> DepOutcome {
        match self.index.get(name) {
            Some(&i) => {
                DepOutcome::from_row(&self.rows[i].status, self.rows[i].skip_reason.as_deref())
            }
            None => DepOutcome::Pending,
        }
    }

    fn get_mut(&mut self, name: &str) -> Option<&mut JobStepRow> {
        let i = *self.index.get(name)?;
        Some(&mut self.rows[i])
    }

    /// Apply one change to the in-memory rows (§4.4).
    fn apply(&mut self, change: &Change) {
        match change {
            Change::Promote { step } => {
                if let Some(r) = self.get_mut(step) {
                    r.status = READY.to_string();
                }
            }
            Change::Skip { step, reason } => {
                if let Some(r) = self.get_mut(step) {
                    r.status = SKIPPED.to_string();
                    r.skip_reason = Some(reason.as_str().to_string());
                }
            }
            Change::Fail { step, error } => {
                if let Some(r) = self.get_mut(step) {
                    r.status = FAILED.to_string();
                    r.error_message = Some(error.clone());
                }
            }
            Change::Expand {
                placeholder,
                instances,
            } => {
                if let Some(r) = self.get_mut(placeholder) {
                    r.status = RUNNING.to_string();
                }
                for inst in instances {
                    let row = synthetic_row(inst);
                    self.index.insert(row.step_name.clone(), self.rows.len());
                    self.rows.push(row);
                }
            }
            Change::Adopt { placeholder } => {
                if let Some(r) = self.get_mut(placeholder) {
                    r.status = RUNNING.to_string();
                }
            }
            Change::Rollup {
                placeholder,
                outcome,
            } => {
                if let Some(r) = self.get_mut(placeholder) {
                    match outcome {
                        RollupOutcome::Completed(out) => {
                            r.status = COMPLETED.to_string();
                            r.output = Some(out.clone());
                        }
                        RollupOutcome::Failed(e, out) => {
                            r.status = FAILED.to_string();
                            r.error_message = Some(e.clone());
                            r.output = Some(out.clone());
                        }
                    }
                }
            }
        }
    }
}

/// An in-memory row for an instance that `apply` will insert from `inst`.
fn synthetic_row(inst: &NewJobStep) -> JobStepRow {
    JobStepRow {
        job_id: inst.job_id,
        step_name: inst.step_name.clone(),
        action_name: inst.action_name.clone(),
        action_type: inst.action_type.clone(),
        action_image: inst.action_image.clone(),
        action_spec: inst.action_spec.clone(),
        input: inst.input.clone(),
        status: inst.status.clone(),
        required_ability: inst.required_ability.clone(),
        required_tags: serde_json::to_value(&inst.required_tags).unwrap_or(Value::Array(vec![])),
        runner: inst.runner.clone(),
        timeout_secs: inst.timeout_secs,
        when_condition: None,
        for_each_expr: None,
        loop_source: inst.loop_source.clone(),
        loop_index: inst.loop_index,
        loop_total: inst.loop_total,
        loop_item: inst.loop_item.clone(),
        max_retries: inst.max_retries,
        retry_backoff_secs: inst.retry_backoff_secs,
        retry_strategy: inst.retry_strategy.clone(),
        retry_jitter: inst.retry_jitter,
        retry_history: Value::Array(vec![]),
        action_workspace: inst.action_workspace.clone(),
        action_revision: inst.action_revision.clone(),
        ..Default::default()
    }
}

// ── dependency gate (spec 2026-09-26 §2, §5) ─────────────────────────

fn gate_for(snap: &Snapshot, fs: &FlowStep) -> Gate {
    gate(&fs.depends_on, |d| snap.outcome(d))
}

fn is_placeholder(r: &JobStepRow) -> bool {
    r.for_each_expr.is_some()
}

// ── phases ───────────────────────────────────────────────────────────

/// P0: R5 sequential advance + R6 rollup over every running placeholder.
fn phase_rollup(snap: &Snapshot, task: &TaskDef) -> Vec<Change> {
    let mut out = Vec::new();
    for ph in snap
        .rows
        .iter()
        .filter(|r| is_placeholder(r) && r.status == RUNNING)
    {
        let mut instances: Vec<&JobStepRow> = snap
            .rows
            .iter()
            .filter(|r| r.loop_source.as_deref() == Some(ph.step_name.as_str()))
            .collect();
        if instances.is_empty() {
            continue;
        }
        instances.sort_by_key(|r| r.loop_index.unwrap_or(0));
        let flow_step = task.flow.get(&ph.step_name);
        let sequential = flow_step.map(|f| f.sequential).unwrap_or(false);
        let cof = flow_step.map(|f| f.continue_on_failure).unwrap_or(false);

        // R5
        if sequential {
            let any_bad = instances
                .iter()
                .any(|i| matches!(i.status.as_str(), FAILED | CANCELLED));
            if any_bad && !cof {
                let pending: Vec<&&JobStepRow> =
                    instances.iter().filter(|i| i.status == PENDING).collect();
                if !pending.is_empty() {
                    for i in pending {
                        out.push(Change::Skip {
                            step: i.step_name.clone(),
                            reason: SkipReason::Unreachable,
                        });
                    }
                    // Nothing else for this placeholder this phase: the rollup
                    // happens next pass, once those skips have been applied.
                    continue;
                }
                // Failure precedence still holds — never advance past a failed
                // instance — but with nothing left to skip, fall through to R6.
            }
            for i in instances.iter().filter(|i| is_terminal(&i.status)) {
                // A row with no `loop_index` has no successor to name.
                let Some(idx) = i.loop_index else { continue };
                let next_idx = idx + 1;
                if let Some(next) = instances
                    .iter()
                    .find(|n| n.loop_index == Some(next_idx) && n.status == PENDING)
                {
                    out.push(Change::Promote {
                        step: next.step_name.clone(),
                    });
                }
            }
        }

        // R6
        if instances.iter().all(|i| is_terminal(&i.status)) {
            // Whether the loop failed is decided by the statuses; the diagnostic
            // list names only the instances that actually carry a `loop_index`.
            let any_failed = instances.iter().any(|i| i.status == FAILED);
            let failed_indices: Vec<i32> = instances
                .iter()
                .filter(|i| i.status == FAILED)
                .filter_map(|i| i.loop_index)
                .collect();
            // Build the output array unconditionally now — one element per
            // existing instance, null where it produced none — regardless of
            // whether the rollup ends up Completed or Failed (spec §4).
            let output_array = Value::Array(
                instances
                    .iter()
                    .map(|i| i.output.clone().unwrap_or(Value::Null))
                    .collect(),
            );
            // The rollup's own status is never excused by its own
            // continue_on_failure — that flag protects only the JOB (spec
            // §6), never the loop's own terminal state. A loop with any
            // failed instance stays `failed`, cof'd or not.
            let outcome = if any_failed {
                RollupOutcome::Failed(
                    format!(
                        "for_each loop failed: instances {:?} failed",
                        failed_indices
                    ),
                    output_array,
                )
            } else {
                RollupOutcome::Completed(output_array)
            };
            out.push(Change::Rollup {
                placeholder: ph.step_name.clone(),
                outcome,
            });
        }
    }
    out
}

/// P1 (merged): cascade-skip + promote with `when`, relayed internally
/// until stable. Spec §5: replaces the old P1-then-P2 split, which only
/// ever gave one hop of same-pass relay and gave it asymmetrically
/// (skip-rooted chains got it, failed-rooted chains didn't). Returns every
/// change made across all its inner iterations, and leaves `snap` updated
/// to the final, stable state.
fn phase_promote(
    snap: &mut Snapshot,
    task: &TaskDef,
    mut build_ctx: impl FnMut(&Snapshot) -> Option<Value>,
) -> Vec<Change> {
    let mut all_changes = Vec::new();
    loop {
        let ctx = build_ctx(snap);
        let mut batch = Vec::new();
        for r in snap
            .rows
            .iter()
            .filter(|r| r.status == PENDING && !is_placeholder(r))
        {
            let Some(fs) = task.flow.get(&r.step_name) else {
                continue;
            };
            match gate_for(snap, fs) {
                Gate::Wait => continue,
                Gate::Omitted => {
                    batch.push(Change::Skip {
                        step: r.step_name.clone(),
                        reason: SkipReason::Unreachable,
                    });
                }
                Gate::Open => match (&r.when_condition, ctx.as_ref()) {
                    (None, _) => batch.push(Change::Promote {
                        step: r.step_name.clone(),
                    }),
                    (Some(_), None) => {} // no template context: stays pending (today's behaviour)
                    (Some(w), Some(c)) => match stroem_common::template::evaluate_condition(w, c) {
                        Ok(true) => batch.push(Change::Promote {
                            step: r.step_name.clone(),
                        }),
                        Ok(false) => batch.push(Change::Skip {
                            step: r.step_name.clone(),
                            reason: SkipReason::Condition,
                        }),
                        Err(e) => batch.push(Change::Fail {
                            step: r.step_name.clone(),
                            error: format!("when condition error: {:#}", e),
                        }),
                    },
                },
            }
        }
        if batch.is_empty() {
            return all_changes;
        }
        apply_all(snap, &batch);
        all_changes.extend(batch);
        // Loop: the next iteration's build_ctx(snap) call sees everything
        // just applied, and may now be able to decide rows this iteration
        // couldn't. Terminates because `batch.is_empty()` requires strictly
        // fewer pending rows each productive iteration, and the pending set
        // is finite.
    }
}

/// P3: R0 adopt (Task 6) + R4 retire/expand placeholders. Needs a context.
fn phase_placeholders(
    snap: &Snapshot,
    task: &TaskDef,
    ctx: Option<&Value>,
    job_id: uuid::Uuid,
) -> Vec<Change> {
    let mut out = Vec::new();
    let Some(ctx) = ctx else { return out };
    for r in snap
        .rows
        .iter()
        .filter(|r| r.status == PENDING && is_placeholder(r))
    {
        let Some(fs) = task.flow.get(&r.step_name) else {
            continue;
        };
        // R0: instances already exist (a past crash between insert and the
        // placeholder transition) → adopt the placeholder instead of leaving it
        // pending forever. Never re-expands.
        if snap.status(&format!("{}[0]", r.step_name)).is_some() {
            out.push(Change::Adopt {
                placeholder: r.step_name.clone(),
            });
            continue;
        }
        match gate_for(snap, fs) {
            Gate::Wait => continue,
            Gate::Omitted => {
                out.push(Change::Skip {
                    step: r.step_name.clone(),
                    reason: SkipReason::Unreachable,
                });
                continue;
            }
            Gate::Open => {}
        }
        if let Some(w) = &r.when_condition {
            match stroem_common::template::evaluate_condition(w, ctx) {
                Ok(true) => {}
                Ok(false) => {
                    out.push(Change::Skip {
                        step: r.step_name.clone(),
                        reason: SkipReason::Condition,
                    });
                    continue;
                }
                Err(e) => {
                    out.push(Change::Fail {
                        step: r.step_name.clone(),
                        error: format!("when condition error: {:#}", e),
                    });
                    continue;
                }
            }
        }
        let expr = r.for_each_expr.as_deref().unwrap_or("");
        let items = match parse_for_each_items(expr, ctx) {
            Ok(items) => items,
            Err(e) => {
                out.push(Change::Fail {
                    step: r.step_name.clone(),
                    error: format!("for_each expression error: {:#}", e),
                });
                continue;
            }
        };
        if items.is_empty() {
            out.push(Change::Skip {
                step: r.step_name.clone(),
                reason: SkipReason::Empty,
            });
            continue;
        }
        if items.len() > MAX_FOR_EACH_ITEMS {
            out.push(Change::Fail {
                step: r.step_name.clone(),
                error: format!(
                    "for_each produced {} items (max {})",
                    items.len(),
                    MAX_FOR_EACH_ITEMS
                ),
            });
            continue;
        }
        let total = items.len() as i32;
        let instances = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let instance_status = if fs.sequential && i > 0 {
                    StepStatus::Pending
                } else {
                    StepStatus::Ready
                };
                NewJobStep {
                    job_id,
                    step_name: format!("{}[{}]", r.step_name, i),
                    action_name: r.action_name.clone(),
                    action_type: r.action_type.clone(),
                    action_image: r.action_image.clone(),
                    action_spec: r.action_spec.clone(),
                    input: r.input.clone(),
                    status: instance_status.to_string(),
                    required_ability: r.required_ability.clone(),
                    required_tags: serde_json::from_value(r.required_tags.clone())
                        .unwrap_or_default(),
                    runner: r.runner.clone(),
                    timeout_secs: r.timeout_secs,
                    when_condition: None,
                    for_each_expr: None,
                    loop_source: Some(r.step_name.clone()),
                    loop_index: Some(i as i32),
                    loop_total: Some(total),
                    loop_item: Some(item.clone()),
                    max_retries: r.max_retries,
                    retry_backoff_secs: r.retry_backoff_secs,
                    retry_strategy: r.retry_strategy.clone(),
                    retry_jitter: r.retry_jitter,
                    action_workspace: r.action_workspace.clone(),
                    action_revision: r.action_revision.clone(),
                    // git refs: an instance runs the placeholder's pins (spec § 7.3).
                    action_ref: r.action_ref.clone(),
                    task_workspace: r.task_workspace.clone(),
                    task_ref: r.task_ref.clone(),
                    task_revision: r.task_revision.clone(),
                }
            })
            .collect();
        out.push(Change::Expand {
            placeholder: r.step_name.clone(),
            instances,
        });
    }
    out
}

fn apply_all(snap: &mut Snapshot, changes: &[Change]) {
    for c in changes {
        snap.apply(c);
    }
}

/// The `when:` / `for_each:` context for the rows as they are NOW (phase B
/// sees phase A's changes). `Scope::Condition` never carries `each` — the
/// placeholder's condition runs before instances exist (spec §4.2).
fn condition_context(
    job: &JobRow,
    rows: &[JobStepRow],
    ws: &WorkspaceConfig,
    snapshots: &crate::render_context::Snapshots,
) -> crate::render_context::RenderContext {
    use crate::render_context::{build, views, JobContext, Scope};
    let job_ctx = JobContext {
        job_id: job.job_id,
        job_input: job.input.as_ref(),
        caller_secrets: &ws.secrets,
        owner_secrets: &ws.secrets,
        snapshots,
        job_revision: job.revision.as_deref(),
        job_ref: job.git_ref.as_deref(),
    };
    build(&job_ctx, &views(rows), None, Scope::Condition)
}

/// The pure fixpoint (§4.4). Renders templates; never touches the database.
/// `workspace_config == None` reproduces today's "no template context" mode.
// used from Task 2 onward
pub fn run(
    task: &TaskDef,
    job: &JobRow,
    steps: &[JobStepRow],
    workspace_config: Option<&WorkspaceConfig>,
    snapshots: &crate::render_context::Snapshots,
) -> Result<Plan> {
    let pending_placeholders = steps
        .iter()
        .filter(|r| is_placeholder(r) && r.status == PENDING)
        .count();
    let universe = steps.len() + MAX_FOR_EACH_ITEMS * pending_placeholders;
    let bound = 4 * universe + 1;

    let mut snap = Snapshot::new(steps.to_vec());
    let mut changes = Vec::new();
    let mut passes = 0usize;
    loop {
        passes += 1;
        debug_assert!(passes <= bound, "cascade pass bound exceeded");
        if passes > bound {
            tracing::warn!(job_id = %job.job_id, "Cascade pass bound ({}) reached — breaking", bound);
            break;
        }
        let mut pass = Vec::new();

        let p0 = phase_rollup(&snap, task);
        apply_all(&mut snap, &p0);
        pass.extend(p0);

        let build_ctx = |s: &Snapshot| {
            workspace_config.map(|ws| {
                condition_context(job, &s.rows, ws, snapshots)
                    .as_value()
                    .clone()
            })
        };
        let p1 = phase_promote(&mut snap, task, build_ctx);
        pass.extend(p1);
        // (phase_promote already applied its own changes to `snap` internally —
        // no separate apply_all(&mut snap, &p1) call here, unlike the other phases.)

        let ctx_b = workspace_config.map(|ws| condition_context(job, &snap.rows, ws, snapshots));
        let p3 = phase_placeholders(
            &snap,
            task,
            ctx_b.as_ref().map(|c| c.as_value()),
            job.job_id,
        );
        apply_all(&mut snap, &p3);
        pass.extend(p3);

        if pass.is_empty() {
            break;
        }
        changes.extend(pass);
    }
    // Scrub secret values out of every failure message before the plan leaves
    // this function. `condition_context` puts the workspace secrets
    // into the `when` / `for_each` context, and Tera's raw text quotes the
    // offending value in filter errors. The template error itself carries none
    // of that text (spec 2026-10-06 § 3.2); this scrub is the second line, so
    // that a value reaching a message any other way is never persisted verbatim
    // to `job_step.error_message` and `retry_history`. Only the failure path
    // pays for this.
    if let Some(cfg) = workspace_config {
        if changes.iter().any(|c| matches!(c, Change::Fail { .. })) {
            let secret_values = crate::workspace_set::collect_config_secret_values(cfg);
            if !secret_values.is_empty() {
                for change in changes.iter_mut() {
                    if let Change::Fail { error, .. } = change {
                        *error = crate::workspace_set::redact_secrets_in_str(error, &secret_values);
                    }
                }
            }
        }
    }
    Ok(Plan { changes })
}

/// Counts per change kind, for the log line and for tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Applied {
    pub promoted: usize,
    pub skipped: usize,
    pub failed: usize,
    pub expanded: usize,
    pub adopted: usize,
    pub rolled_up: usize,
}

#[derive(Debug)]
pub enum ApplyError {
    /// A step-row statement affected fewer rows than the plan named: some row is
    /// no longer in the status the snapshot assumed. The caller rolls back.
    GuardMiss {
        step: String,
    },
    Db(anyhow::Error),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::GuardMiss { step } => write!(f, "cascade guard miss on step '{step}'"),
            ApplyError::Db(e) => write!(f, "{e:#}"),
        }
    }
}
impl std::error::Error for ApplyError {}
impl From<anyhow::Error> for ApplyError {
    fn from(e: anyhow::Error) -> Self {
        ApplyError::Db(e)
    }
}

fn expect_rows(n: u64, expected: usize, step: &str) -> Result<(), ApplyError> {
    if n as usize == expected {
        Ok(())
    } else {
        Err(ApplyError::GuardMiss {
            step: step.to_string(),
        })
    }
}

/// Apply `plan.changes` in order inside the caller's transaction (§4.6). Batches
/// runs of consecutive `Promote`s and `Skip`s into one statement each; every
/// step-row statement must affect exactly the rows it names.
pub async fn apply(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    job_id: Uuid,
    plan: &Plan,
) -> Result<Applied, ApplyError> {
    let mut a = Applied::default();
    let mut i = 0;
    // §4.7: step rows move in change order, then the job row once, last.
    let mut mark_job_running = false;
    let changes = &plan.changes;
    while i < changes.len() {
        match &changes[i] {
            Change::Promote { .. } => {
                let mut names = Vec::new();
                while let Some(Change::Promote { step }) = changes.get(i) {
                    names.push(step.clone());
                    i += 1;
                }
                let n = JobStepRepo::promote_steps_tx(&mut **tx, job_id, &names).await?;
                expect_rows(n, names.len(), &names.join(","))?;
                a.promoted += names.len();
            }
            Change::Skip { .. } => {
                // A run of consecutive skips is grouped into one bucket per reason
                // (first-seen order); rows within a run may therefore be written in
                // a different order than the plan lists them, which is safe because
                // each is an independent guarded single-row UPDATE inside the same
                // transaction (spec §4.3).
                let mut buckets: Vec<(SkipReason, Vec<String>)> = Vec::new();
                while let Some(Change::Skip { step, reason }) = changes.get(i) {
                    match buckets.iter_mut().find(|(r, _)| r == reason) {
                        Some((_, names)) => names.push(step.clone()),
                        None => buckets.push((*reason, vec![step.clone()])),
                    }
                    i += 1;
                }
                for (reason, names) in buckets {
                    let n = JobStepRepo::skip_steps_tx(&mut **tx, job_id, &names, reason.as_str())
                        .await?;
                    expect_rows(n, names.len(), &names.join(","))?;
                    a.skipped += names.len();
                }
            }
            Change::Fail { step, error } => {
                let n = JobStepRepo::fail_pending_step_tx(&mut **tx, job_id, step, error).await?;
                expect_rows(n, 1, step)?;
                a.failed += 1;
                i += 1;
            }
            Change::Expand {
                placeholder,
                instances,
            } => {
                // Placeholder transition FIRST, then the insert: instances exist only
                // if the transition is in the same committed transaction (fix 1).
                let n = JobStepRepo::start_placeholder_tx(&mut **tx, job_id, placeholder).await?;
                expect_rows(n, 1, placeholder)?;
                JobStepRepo::create_steps_tx(&mut **tx, instances).await?;
                mark_job_running = true; // zero rows allowed (R7); issued after the loop
                a.expanded += 1;
                i += 1;
            }
            Change::Adopt { placeholder } => {
                let n = JobStepRepo::start_placeholder_tx(&mut **tx, job_id, placeholder).await?;
                expect_rows(n, 1, placeholder)?;
                mark_job_running = true;
                a.adopted += 1;
                i += 1;
            }
            Change::Rollup {
                placeholder,
                outcome,
            } => {
                let n = match outcome {
                    RollupOutcome::Completed(out) => {
                        JobStepRepo::complete_placeholder_tx(&mut **tx, job_id, placeholder, out)
                            .await?
                    }
                    RollupOutcome::Failed(err, out) => {
                        JobStepRepo::fail_placeholder_tx(&mut **tx, job_id, placeholder, err, out)
                            .await?
                    }
                };
                expect_rows(n, 1, placeholder)?;
                a.rolled_up += 1;
                i += 1;
            }
        }
    }
    if mark_job_running {
        JobRepo::mark_running_if_pending_tx(&mut **tx, job_id).await?; // zero rows allowed (R7)
    }
    Ok(a)
}

const MAX_ATTEMPTS: usize = 3;

fn is_deadlock(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<sqlx::Error>()
            .and_then(|s| s.as_database_error())
            .and_then(|d| d.code())
            .map(|code| code == "40P01")
            .unwrap_or(false)
    })
}

/// The one entry point both callers use (§4.7): read the job and its steps, run
/// the pure fixpoint, apply the plan in one transaction. A guard miss (a row moved
/// between snapshot and apply) or a Postgres deadlock rolls back and re-runs from
/// a fresh snapshot, at most `MAX_ATTEMPTS` times.
#[tracing::instrument(skip(pool, task, workspace_config, snapshots))]
pub async fn execute(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
    workspace_config: Option<&WorkspaceConfig>,
    snapshots: &crate::render_context::Snapshots,
) -> Result<Plan> {
    // Why the last attempt was thrown away, for the exhaustion message.
    let mut last_cause = String::from("no re-run recorded");
    for attempt in 1..=MAX_ATTEMPTS {
        let job = JobRepo::get(pool, job_id).await?.context("Job not found")?;
        let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
        let plan = run(task, &job, &steps, workspace_config, snapshots)?;
        if plan.changes.is_empty() {
            return Ok(plan);
        }

        let mut tx = pool.begin().await.context("begin cascade apply")?;
        match apply(&mut tx, job_id, &plan).await {
            Ok(applied) => {
                tx.commit().await.context("commit cascade apply")?;
                tracing::info!(
                    job_id = %job_id, attempt,
                    promoted = applied.promoted, skipped = applied.skipped, failed = applied.failed,
                    expanded = applied.expanded, adopted = applied.adopted, rolled_up = applied.rolled_up,
                    "Cascade applied"
                );
                return Ok(plan);
            }
            Err(ApplyError::GuardMiss { step }) => {
                tx.rollback().await.ok();
                tracing::warn!(job_id = %job_id, attempt, step = %step, "Cascade guard miss — re-running");
                last_cause = format!("guard miss on step '{step}'");
            }
            Err(ApplyError::Db(e)) if is_deadlock(&e) => {
                tx.rollback().await.ok();
                tracing::warn!(job_id = %job_id, attempt, "Cascade deadlock (40P01) — re-running");
                last_cause = "deadlock (40P01)".to_string();
            }
            Err(ApplyError::Db(e)) => {
                tx.rollback().await.ok();
                return Err(e);
            }
        }
    }
    bail!(
        "cascade re-ran {} times without applying for job {}; last cause: {}",
        MAX_ATTEMPTS,
        job_id,
        last_cause
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use stroem_common::depends_on::Outcome;
    use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
    use stroem_db::{JobRow, JobStepRow};
    use uuid::Uuid;

    // ── builders ─────────────────────────────────────────────────────

    fn job(input: Option<Value>) -> JobRow {
        JobRow {
            git_ref: None,
            task_folder: None,
            job_id: Uuid::new_v4(),
            workspace: "default".to_string(),
            task_name: "t".to_string(),
            mode: "distributed".to_string(),
            input,
            output: None,
            status: "running".to_string(),
            source_type: "api".to_string(),
            source_id: None,
            worker_id: None,
            revision: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            log_path: None,
            parent_job_id: None,
            parent_step_name: None,
            timeout_secs: None,
            retry_of_job_id: None,
            retry_job_id: None,
            retry_attempt: 0,
            max_retries: None,
            raw_input: None,
            source_job_id: None,
            restart_from_step: None,
        }
    }

    fn row(name: &str, status: &str) -> JobStepRow {
        JobStepRow {
            job_id: Uuid::nil(),
            step_name: name.to_string(),
            action_name: "noop".to_string(),
            action_type: "script".to_string(),
            action_spec: Some(json!({"script": "true"})),
            status: status.to_string(),
            required_ability: "script".to_string(),
            required_tags: json!([]),
            runner: "local".to_string(),
            retry_history: json!([]),
            ..Default::default()
        }
    }
    fn row_when(name: &str, status: &str, when: &str) -> JobStepRow {
        JobStepRow {
            when_condition: Some(when.to_string()),
            ..row(name, status)
        }
    }
    fn row_skipped(name: &str, reason: &str) -> JobStepRow {
        JobStepRow {
            skip_reason: Some(reason.to_string()),
            ..row(name, "skipped")
        }
    }
    fn row_out(name: &str, output: Value) -> JobStepRow {
        JobStepRow {
            output: Some(output),
            ..row(name, "completed")
        }
    }
    fn placeholder(name: &str, status: &str, expr: &str) -> JobStepRow {
        JobStepRow {
            for_each_expr: Some(expr.to_string()),
            ..row(name, status)
        }
    }
    fn instance(source: &str, i: i32, status: &str, output: Option<Value>) -> JobStepRow {
        JobStepRow {
            loop_source: Some(source.to_string()),
            loop_index: Some(i),
            loop_total: Some(3),
            loop_item: Some(json!(i)),
            output,
            ..row(&format!("{source}[{i}]"), status)
        }
    }

    fn fs(deps: &[&str]) -> FlowStep {
        FlowStep {
            git_ref: None,
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: deps
                .iter()
                .map(|d| stroem_common::depends_on::DependsOnEntry::Name(d.to_string()))
                .collect(),
            input: HashMap::new(),
            continue_on_failure: false,
            legacy_continue_when_skipped: None,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        }
    }
    fn fs_cof(deps: &[&str]) -> FlowStep {
        FlowStep {
            continue_on_failure: true,
            ..fs(deps)
        }
    }
    /// Kept for the pre-0.18.0 continue_when_skipped/skip-reason test block
    /// below — this helper only needs to compile against the renamed,
    /// detection-only field; its many call sites' assertions still exercise
    /// retired `gate_for` semantics (`continue_when_skipped` is no longer
    /// read for behavior, spec 2026-10-01 §6) and are unrelated to the loop
    /// output fix.
    fn fs_cws(deps: &[&str]) -> FlowStep {
        FlowStep {
            legacy_continue_when_skipped: Some(true),
            ..fs(deps)
        }
    }
    fn fs_seq(deps: &[&str]) -> FlowStep {
        FlowStep {
            sequential: true,
            ..fs(deps)
        }
    }
    fn task(flow: Vec<(&str, FlowStep)>) -> TaskDef {
        task_owned(flow.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    /// Same as [`task`], for generated flows whose names are owned `String`s.
    fn task_owned(flow: Vec<(String, FlowStep)>) -> TaskDef {
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: flow.into_iter().collect(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        }
    }

    fn ws() -> WorkspaceConfig {
        WorkspaceConfig::new()
    }

    /// Final in-memory status of every row after `run`, by name.
    fn final_statuses(plan: &Plan, rows: &[JobStepRow]) -> HashMap<String, String> {
        let mut snap = Snapshot::new(rows.to_vec());
        for c in &plan.changes {
            snap.apply(c);
        }
        snap.rows
            .iter()
            .map(|r| (r.step_name.clone(), r.status.clone()))
            .collect()
    }

    /// Every `Skip` in plan order as `(step, reason)`.
    fn skips(plan: &Plan) -> Vec<(String, &'static str)> {
        plan.changes
            .iter()
            .filter_map(|c| match c {
                Change::Skip { step, reason } => Some((step.clone(), reason.as_str())),
                _ => None,
            })
            .collect()
    }
    fn s(step: &str, reason: &'static str) -> (String, &'static str) {
        (step.to_string(), reason)
    }

    fn names(plan: &Plan) -> Vec<String> {
        plan.changes
            .iter()
            .map(|c| match c {
                Change::Promote { step } => format!("promote:{step}"),
                Change::Skip { step, .. } => format!("skip:{step}"),
                Change::Fail { step, .. } => format!("fail:{step}"),
                Change::Expand {
                    placeholder,
                    instances,
                } => {
                    format!("expand:{placeholder}:{}", instances.len())
                }
                Change::Adopt { placeholder } => format!("adopt:{placeholder}"),
                Change::Rollup {
                    placeholder,
                    outcome: RollupOutcome::Completed(_),
                } => {
                    format!("rollup-ok:{placeholder}")
                }
                Change::Rollup {
                    placeholder,
                    outcome: RollupOutcome::Failed(_, _),
                } => {
                    format!("rollup-fail:{placeholder}")
                }
            })
            .collect()
    }

    // ── R1/R2/R3: promotion and skipping ─────────────────────────────

    #[test]
    fn linear_promotion() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"])), ("c", fs(&["b"]))]);
        let rows = vec![
            row("a", "completed"),
            row("b", "pending"),
            row("c", "pending"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
    }

    #[test]
    fn diamond_join_waits_for_both_branches() {
        let t = task(vec![
            ("root", fs(&[])),
            ("l", fs(&["root"])),
            ("r", fs(&["root"])),
            ("join", fs(&["l", "r"])),
        ]);
        let rows = vec![
            row("root", "completed"),
            row("l", "completed"),
            row("r", "running"),
            row("join", "pending"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(plan.changes.is_empty(), "join must wait for r");
    }

    #[test]
    fn failed_dep_skips_dependents_transitively_in_one_run() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"])), ("c", fs(&["b"]))]);
        let rows = vec![row("a", "failed"), row("b", "pending"), row("c", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["b"], "skipped");
        assert_eq!(s["c"], "skipped");
    }

    #[test]
    fn dependent_accepting_failed_or_cancelled_promotes() {
        // Retired-behavior replacement: a dependency's own `continue_on_failure`
        // no longer bypasses the gate for its dependents (spec 2026-10-01 §2.3)
        // — only the dependent's own `accept` set does.
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs_with_accept(&[("a", &[Outcome::Failed])])),
            ("x", fs(&[])),
            ("y", fs_with_accept(&[("x", &[Outcome::Cancelled])])),
        ]);
        let rows = vec![
            row("a", "failed"),
            row("b", "pending"),
            row("x", "cancelled"),
            row("y", "pending"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["b"], "ready");
        assert_eq!(s["y"], "ready");
    }

    #[test]
    fn mixed_skipped_and_failed_deps_without_cof_skips() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let rows = vec![row("a", "skipped"), row("b", "failed"), row("c", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(final_statuses(&plan, &rows)["c"], "skipped");
    }

    #[test]
    fn all_deps_skipped_cascade_skips_even_with_truthy_when() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row("a", "skipped"), row_when("b", "pending", "true")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["skip:b"]);
    }

    #[test]
    fn all_deps_skipped_applies_without_workspace_config() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row("a", "skipped"), row("b", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            None,
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["skip:b"]);
    }

    #[test]
    fn when_true_false_and_error() {
        let t = task(vec![
            ("a", fs(&[])),
            ("t", fs(&["a"])),
            ("f", fs(&["a"])),
            ("e", fs(&["a"])),
        ]);
        let rows = vec![
            row_out("a", json!({"go": true})),
            row_when("t", "pending", "{{ a.output.go }}"),
            row_when("f", "pending", "{{ not a.output.go }}"),
            row_when("e", "pending", "{{ a.output.missing.deep }}"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["t"], "ready");
        assert_eq!(s["f"], "skipped");
        assert_eq!(s["e"], "failed");
        let err = plan.changes.iter().find_map(|c| match c {
            Change::Fail { step, error } if step == "e" => Some(error.clone()),
            _ => None,
        });
        assert!(err.unwrap().starts_with("when condition error: "));
    }

    /// Security regression (2026-09-11): `condition_context` puts the
    /// workspace secrets into the `when` context, and Tera's raw text quotes
    /// the offending value in filter errors (`round(method=…)` here, asserted
    /// first). The resulting `Change::Fail` error is persisted to
    /// `job_step.error_message` and `retry_history`, so it must carry no value:
    /// the template error is value-free (spec 2026-10-06 § 3.2) and `run`
    /// scrubs as the second line.
    #[test]
    fn when_condition_error_does_not_leak_secret_values() {
        const TPL: &str = "{{ 1 | round(method=secret.db.host) }}";
        assert!(
            crate::workspace_set::tera_raw_detail_contains(
                TPL,
                &json!({"secret": {"db": {"host": "db.internal.prod"}}}),
                "db.internal.prod"
            ),
            "fixture must leak through Tera's raw text, else this test is vacuous"
        );
        let t = task(vec![("a", fs(&[])), ("e", fs(&["a"]))]);
        let mut w = ws();
        w.secrets
            .insert("db".to_string(), json!({"host": "db.internal.prod"}));
        let rows = vec![row("a", "completed"), row_when("e", "pending", TPL)];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&w),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let err = plan
            .changes
            .iter()
            .find_map(|c| match c {
                Change::Fail { step, error } if step == "e" => Some(error.clone()),
                _ => None,
            })
            .expect("step e must fail");
        assert!(
            !err.contains("db.internal.prod"),
            "when condition error leaked a secret value: {err}"
        );
        assert!(
            err.contains("round"),
            "the error must still name the failing filter: {err}"
        );
    }

    /// spec §1.2 bug 3: `{{ state.x }}` in a `when:` was always undefined
    /// because the cascade context never carried a snapshot.
    #[test]
    fn when_condition_sees_task_and_global_state() {
        use crate::render_context::{Snapshot as StateSnapshot, Snapshots};
        let t = task(vec![
            ("a", fs(&[])),
            ("renew", fs(&["a"])),
            ("g", fs(&["a"])),
        ]);
        let rows = vec![
            row("a", "completed"),
            row_when(
                "renew",
                "pending",
                "{{ not state or state.days_remaining < 30 }}",
            ),
            row_when("g", "pending", "{{ global_state.flag }}"),
        ];
        let mk = |j: serde_json::Value| {
            Some(StateSnapshot {
                id: uuid::Uuid::nil(),
                storage_key: "k".into(),
                has_json: true,
                json: Some(j),
            })
        };
        let fresh = Snapshots {
            task: mk(json!({"days_remaining": 60})),
            global: mk(json!({"flag": false})),
        };
        let plan = run(&t, &job(None), &rows, Some(&ws()), &fresh).unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["renew"], "skipped", "fresh snapshot must skip the guard");
        assert_eq!(s["g"], "skipped");

        let stale = Snapshots {
            task: mk(json!({"days_remaining": 10})),
            global: mk(json!({"flag": true})),
        };
        let plan = run(&t, &job(None), &rows, Some(&ws()), &stale).unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["renew"], "ready");
        assert_eq!(s["g"], "ready");

        // No snapshot at all: `not state` is true — first run.
        let plan = run(&t, &job(None), &rows, Some(&ws()), &Snapshots::default()).unwrap();
        assert_eq!(final_statuses(&plan, &rows)["renew"], "ready");
    }

    #[test]
    fn when_without_workspace_config_stays_pending() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row("a", "completed"), row_when("b", "pending", "true")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            None,
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(plan.changes.is_empty());
    }

    #[test]
    fn dependent_cof_no_longer_tolerates_a_failed_dep() {
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs(&[])),
            ("c", fs(&["a", "b"])),
            ("d", fs_cof(&["a"])),
        ]);
        // a failed, b still running: the uniform barrier (spec §2.3) makes c
        // wait for b too, rather than deciding immediately off a's failure
        // alone; d's own `fs_cof` no longer tolerates a's failure (only d's
        // own `accept` can).
        let rows = vec![
            row("a", "failed"),
            row("b", "running"),
            row("c", "pending"),
            row("d", "pending"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["c"], "pending");
        assert_eq!(s["d"], "skipped");

        // d's own `accept: [failed]` (replacing the retired cof-bypass) passes
        // it past a's failure; b still running keeps c waiting regardless of
        // a's own (now gate-irrelevant) continue_on_failure.
        let t2 = task(vec![
            ("a", fs_cof(&[])),
            ("b", fs(&[])),
            ("c", fs(&["a", "b"])),
            ("d", fs_with_accept(&[("a", &[Outcome::Failed])])),
        ]);
        let rows2 = vec![
            row("a", "failed"),
            row("b", "running"),
            row("c", "pending"),
            row("d", "pending"),
        ];
        let plan2 = run(
            &t2,
            &job(None),
            &rows2,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let s2 = final_statuses(&plan2, &rows2);
        assert_eq!(s2["d"], "ready");
        assert_eq!(s2["c"], "pending");
    }

    #[test]
    fn rows_absent_from_flow_are_ignored() {
        let t = task(vec![("a", fs(&[]))]);
        let rows = vec![row("a", "completed"), row("ghost", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(plan.changes.is_empty());
    }

    // ── phase order ──────────────────────────────────────────────────

    #[test]
    fn placeholder_sees_a_step_skipped_earlier_in_the_same_pass() {
        // Codex's counterexample: a root `a` with when:false, independent root
        // placeholder `p` whose when tests `a is defined`. Today expansion sees the
        // skip; the phase model must too (ctx_B is built after P1/P2).
        let t = task(vec![("a", fs(&[])), ("p", fs(&[]))]);
        let rows = vec![
            row_when("a", "pending", "false"),
            JobStepRow {
                when_condition: Some(
                    "{% if a is defined %}true{% else %}false{% endif %}".to_string(),
                ),
                ..placeholder("p", "pending", "[1,2]")
            },
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["skip:a", "expand:p:2"]);
    }

    #[test]
    fn when_in_p1_does_not_see_a_skip_from_the_same_phase_until_next_pass() {
        // `a` and `b` are both roots; b's `when` references a. P1 evaluates both
        // against the SAME phase-start snapshot, where `a` is still pending and so
        // absent from the render context. b therefore takes the `else` branch and
        // errors on the undefined `a`, which fails it in pass 1 — a is skipped by
        // the same phase, but b never sees that skip, because a failed step is
        // terminal and is never re-evaluated in a later pass.
        //
        // This is the phase model's defining trade-off: within one phase, decisions
        // are made on one consistent snapshot; only the NEXT pass observes them.
        let t = task(vec![("a", fs(&[])), ("b", fs(&[]))]);
        let rows = vec![
            row_when("a", "pending", "false"),
            row_when(
                "b",
                "pending",
                "{% if a is defined %}yes{% else %}{{ a.output }}{% endif %}",
            ),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        // pass 1: a skipped, b fails (a undefined inside the else branch)
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["a"], "skipped");
        assert_eq!(
            s["b"], "failed",
            "b rendered against the phase-start snapshot"
        );
    }

    // ── R4: placeholder retirement / expansion ───────────────────────

    #[test]
    fn placeholder_retirement_branches() {
        let t = task(vec![
            ("dep_failed", fs(&[])),
            ("p_failed_dep", fs(&["dep_failed"])),
            ("dep_skipped", fs(&[])),
            ("p_all_skipped", fs(&["dep_skipped"])),
            ("p_when_false", fs(&[])),
            ("p_when_err", fs(&[])),
            ("p_expr_err", fs(&[])),
            ("p_empty", fs(&[])),
            ("p_too_many", fs(&[])),
            ("dep_failed_cof", fs(&[])),
            // Retired-behavior replacement: the dependency's own cof no
            // longer bypasses the gate; the placeholder's own `accept` does.
            (
                "p_cof",
                fs_with_accept(&[("dep_failed_cof", &[Outcome::Failed])]),
            ),
            ("p_absent_from_flow_is_ignored_via_missing_entry", fs(&[])),
        ]);
        let big = format!("[{}]", vec!["1"; 10_001].join(","));
        let rows = vec![
            row("dep_failed", "failed"),
            placeholder("p_failed_dep", "pending", "[1]"),
            row("dep_skipped", "skipped"),
            placeholder("p_all_skipped", "pending", "[1]"),
            JobStepRow {
                when_condition: Some("false".into()),
                ..placeholder("p_when_false", "pending", "[1]")
            },
            JobStepRow {
                when_condition: Some("{{ nope.x }}".into()),
                ..placeholder("p_when_err", "pending", "[1]")
            },
            placeholder("p_expr_err", "pending", "{{ nope.x }}"),
            placeholder("p_empty", "pending", "[]"),
            placeholder("p_too_many", "pending", &big),
            row("dep_failed_cof", "failed"),
            placeholder("p_cof", "pending", "[7]"),
            placeholder("ghost", "pending", "[1]"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["p_failed_dep"], "skipped");
        assert_eq!(s["p_all_skipped"], "skipped");
        assert_eq!(s["p_when_false"], "skipped");
        assert_eq!(s["p_when_err"], "failed");
        assert_eq!(s["p_expr_err"], "failed");
        assert_eq!(s["p_empty"], "skipped");
        assert_eq!(s["p_too_many"], "failed");
        assert_eq!(s["p_cof"], "running");
        assert_eq!(s["ghost"], "pending");
        let too_many_err = plan.changes.iter().find_map(|c| match c {
            Change::Fail { step, error } if step == "p_too_many" => Some(error.clone()),
            _ => None,
        });
        assert_eq!(
            too_many_err.unwrap(),
            "for_each produced 10001 items (max 10000)"
        );
        let expr_err = plan.changes.iter().find_map(|c| match c {
            Change::Fail { step, error } if step == "p_expr_err" => Some(error.clone()),
            _ => None,
        });
        assert!(expr_err.unwrap().starts_with("for_each expression error: "));
    }

    #[test]
    fn placeholders_stay_pending_without_workspace_config() {
        let t = task(vec![("d", fs(&[])), ("p", fs(&["d"]))]);
        let rows = vec![row("d", "failed"), placeholder("p", "pending", "[1]")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            None,
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(plan.changes.is_empty(), "R4 needs a config even to retire");
    }

    #[test]
    fn expand_builds_instances_parallel_and_sequential() {
        let t = task(vec![("par", fs(&[])), ("seq", fs_seq(&[]))]);
        let rows = vec![
            placeholder("par", "pending", "[\"a\",\"b\"]"),
            placeholder("seq", "pending", "[\"a\",\"b\"]"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        for c in &plan.changes {
            if let Change::Expand {
                placeholder,
                instances,
            } = c
            {
                assert_eq!(instances.len(), 2);
                assert_eq!(instances[0].step_name, format!("{placeholder}[0]"));
                assert_eq!(
                    instances[0].loop_source.as_deref(),
                    Some(placeholder.as_str())
                );
                assert_eq!(instances[0].loop_index, Some(0));
                assert_eq!(instances[0].loop_total, Some(2));
                assert_eq!(instances[0].loop_item, Some(json!("a")));
                assert!(instances[0].when_condition.is_none());
                assert!(instances[0].for_each_expr.is_none());
                assert_eq!(instances[0].status, "ready");
                let second = if placeholder == "seq" {
                    "pending"
                } else {
                    "ready"
                };
                assert_eq!(instances[1].status, second, "{placeholder}[1]");
            }
        }
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["par"], "running");
        assert_eq!(s["seq"], "running");
        assert_eq!(s["par[1]"], "ready");
        assert_eq!(s["seq[1]"], "pending");
    }

    /// git refs (spec § 7.3): an instance runs the placeholder's action and
    /// task pins, not the live config.
    #[test]
    fn expand_instances_copy_the_placeholder_pins() {
        let t = task(vec![("p", fs(&[]))]);
        let mut p = placeholder("p", "pending", "[1, 2]");
        p.action_workspace = Some("etl".into());
        p.action_ref = Some("release/2.3".into());
        p.action_revision = Some("c1".into());
        p.task_workspace = Some("billing".into());
        p.task_ref = Some("v4".into());
        p.task_revision = Some("c2".into());
        let plan = run(
            &t,
            &job(None),
            &[p],
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let instances = plan
            .changes
            .iter()
            .find_map(|c| match c {
                Change::Expand { instances, .. } => Some(instances),
                _ => None,
            })
            .expect("p expands");
        assert_eq!(instances.len(), 2);
        for i in instances {
            assert_eq!(
                i.action_workspace.as_deref(),
                Some("etl"),
                "{}",
                i.step_name
            );
            assert_eq!(i.action_ref.as_deref(), Some("release/2.3"));
            assert_eq!(i.action_revision.as_deref(), Some("c1"));
            assert_eq!(i.task_workspace.as_deref(), Some("billing"));
            assert_eq!(i.task_ref.as_deref(), Some("v4"));
            assert_eq!(i.task_revision.as_deref(), Some("c2"));
        }
    }

    #[test]
    fn expand_renders_tera_string_and_literal_array() {
        let t = task(vec![("a", fs(&[])), ("p", fs(&["a"]))]);
        let rows = vec![
            row_out("a", json!({"items": [1, 2, 3]})),
            placeholder("p", "pending", "{{ a.output.items | json_encode() }}"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["expand:p:3"]);
    }

    #[test]
    fn pending_placeholder_with_existing_instances_is_adopted() {
        let t = task(vec![("p", fs(&[]))]);
        let rows = vec![
            placeholder("p", "pending", "[1,2]"),
            instance("p", 0, "completed", Some(json!(1))),
            instance("p", 1, "completed", Some(json!(2))),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(
            names(&plan),
            ["adopt:p", "rollup-ok:p"],
            "adopted in P3, rolled up in the next pass"
        );
    }

    #[test]
    fn adoption_needs_a_workspace_config_like_expansion() {
        let t = task(vec![("p", fs(&[]))]);
        let rows = vec![
            placeholder("p", "pending", "[1]"),
            instance("p", 0, "completed", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            None,
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(plan.changes.is_empty());
    }

    // ── R5/R6: sequential advance and rollup ─────────────────────────

    #[test]
    fn sequential_advance_promotes_next_after_completed_or_skipped() {
        let t = task(vec![("x", fs_seq(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "completed", None),
            instance("x", 1, "pending", None),
            instance("x", 2, "pending", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["promote:x[1]"]);
    }

    #[test]
    fn sequential_failure_skips_all_pending_including_successor_of_a_completed_later_instance() {
        // [failed, completed, pending]: today promotes [2] on [1]'s completion;
        // R5's failure precedence never does (spec §4.3 behavioural change).
        let t = task(vec![("x", fs_seq(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "failed", None),
            instance("x", 1, "completed", None),
            instance("x", 2, "pending", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["x[2]"], "skipped");
        assert_eq!(s["x"], "failed", "rolled up in the following pass");
        assert!(!names(&plan).contains(&"promote:x[2]".to_string()));
    }

    #[test]
    fn sequential_cof_promotes_past_failure_and_cancelled_middle_instance() {
        let t = task(vec![(
            "x",
            FlowStep {
                continue_on_failure: true,
                legacy_continue_when_skipped: None,
                ..fs_seq(&[])
            },
        )]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "failed", None),
            instance("x", 1, "cancelled", None),
            instance("x", 2, "pending", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["promote:x[2]"]);
    }

    #[test]
    fn sequential_no_op_while_current_instance_is_live() {
        let t = task(vec![("x", fs_seq(&[]))]);
        for live in ["ready", "claimed", "running"] {
            let rows = vec![
                placeholder("x", "running", "[1,2]"),
                instance("x", 0, live, None),
                instance("x", 1, "pending", None),
            ];
            let plan = run(
                &t,
                &job(None),
                &rows,
                Some(&ws()),
                &crate::render_context::Snapshots::default(),
            )
            .unwrap();
            assert!(plan.changes.is_empty(), "{live}");
        }
    }

    #[test]
    fn rollup_completed_orders_by_index_with_null_for_missing_output_and_cancelled_ok() {
        let t = task(vec![("x", fs(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 2, "completed", Some(json!("c"))),
            instance("x", 0, "completed", Some(json!("a"))),
            instance("x", 1, "cancelled", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        match &plan.changes[..] {
            [Change::Rollup {
                placeholder,
                outcome: RollupOutcome::Completed(out),
            }] => {
                assert_eq!(placeholder, "x");
                assert_eq!(out, &json!(["a", null, "c"]));
            }
            other => panic!(
                "unexpected plan {:?}",
                names(&Plan {
                    changes: other.to_vec()
                })
            ),
        }
    }

    #[test]
    fn rollup_completed_has_one_element_per_existing_instance() {
        // The rollup array is built from the instance ROWS, not from loop_total:
        // a missing index is not a null hole, it is simply absent. Instances [0]
        // and [2] exist (loop_total says 3) → a two-element array.
        let t = task(vec![("x", fs(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "completed", Some(json!("a"))),
            instance("x", 2, "completed", Some(json!("c"))),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        match &plan.changes[..] {
            [Change::Rollup {
                placeholder,
                outcome: RollupOutcome::Completed(out),
            }] => {
                assert_eq!(placeholder, "x");
                assert_eq!(out, &json!(["a", "c"]), "no null inserted for absent [1]");
            }
            other => panic!(
                "unexpected plan {:?}",
                names(&Plan {
                    changes: other.to_vec()
                })
            ),
        }
    }

    /// spec §4: a failed rollup must build the SAME output array a completed
    /// rollup would — one element per existing instance, null where it
    /// produced none — not drop it. The pre-fix code only built the array on
    /// the `Completed` branch. `fs_cof` on purpose: `continue_on_failure`
    /// protects only the job (spec §6), never the rollup's own status — a
    /// cof'd placeholder with a failed instance still rolls up `Failed`.
    #[test]
    fn rollup_builds_the_output_array_on_failure_too_not_just_completion() {
        let task = task(vec![("p", fs_cof(&[]))]);
        let rows = vec![
            placeholder("p", "running", "[2]"),
            instance("p", 0, "completed", Some(json!({"n": 1}))),
            instance("p", 1, "failed", None),
        ];
        let plan_changes = phase_rollup(&Snapshot::new(rows), &task);
        let rollup = plan_changes
            .iter()
            .find_map(|c| match c {
                Change::Rollup {
                    outcome: RollupOutcome::Failed(_, out),
                    ..
                } => Some(out.clone()),
                _ => None,
            })
            .expect("expected a Failed rollup");
        assert_eq!(rollup, json!([{"n": 1}, null]));
    }

    /// Same fix, one layer up: `Snapshot::apply`'s `Rollup` arm must copy the
    /// `Failed` outcome's output into the in-memory row too, not just the
    /// error — same-pass visibility (a downstream `when` reading the rollup
    /// output in the SAME `run` call) needs this, mirroring the `Completed`
    /// arm it already had.
    #[test]
    fn snapshot_apply_copies_output_on_a_failed_rollup_too() {
        let mut snap = Snapshot::new(vec![placeholder("p", "running", "[1]")]);
        snap.apply(&Change::Rollup {
            placeholder: "p".to_string(),
            outcome: RollupOutcome::Failed("boom".into(), json!([null])),
        });
        assert_eq!(snap.status("p"), Some("failed"));
        assert_eq!(snap.rows[0].output, Some(json!([null])));
    }

    #[test]
    fn rollup_failed_text_and_cof() {
        // `y` has its own `continue_on_failure`, but that flag protects only
        // the JOB (spec §6) — the rollup's own status is never excused by
        // it, so both `x` and `y` roll up `Failed` here.
        let t = task(vec![("x", fs(&[])), ("y", fs_cof(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "completed", None),
            instance("x", 1, "failed", None),
            instance("x", 2, "failed", None),
            placeholder("y", "running", "[1]"),
            instance("y", 0, "failed", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let mut fails = vec![];
        let mut oks = vec![];
        for c in &plan.changes {
            match c {
                Change::Rollup {
                    placeholder,
                    outcome: RollupOutcome::Failed(e, _),
                } => fails.push((placeholder.clone(), e.clone())),
                Change::Rollup {
                    placeholder,
                    outcome: RollupOutcome::Completed(_),
                } => oks.push(placeholder.clone()),
                _ => {}
            }
        }
        assert_eq!(
            fails,
            [
                (
                    "x".to_string(),
                    "for_each loop failed: instances [1, 2] failed".to_string()
                ),
                (
                    "y".to_string(),
                    "for_each loop failed: instances [0] failed".to_string()
                ),
            ]
        );
        assert!(
            oks.is_empty(),
            "cof no longer excuses the rollup's own status, only the job's (spec §6)"
        );
    }

    /// An instance row with no `loop_index` (legacy/hand-written data): R5 has no
    /// successor to name for it, and R6's diagnostic list simply omits it.
    #[test]
    fn instance_without_loop_index_is_skipped_by_r5_and_omitted_from_r6_text() {
        let t = task(vec![("x", fs_seq(&[]))]);
        let mut orphan = instance("x", 0, "failed", None);
        orphan.loop_index = None;
        orphan.step_name = "x[?]".to_string();
        let rows = vec![placeholder("x", "running", "[1]"), orphan];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(
            names(&plan),
            ["rollup-fail:x"],
            "no successor promotion for an index-less instance"
        );
        let Change::Rollup {
            outcome: RollupOutcome::Failed(e, _),
            ..
        } = &plan.changes[0]
        else {
            panic!("expected a failed rollup, got {:?}", plan.changes[0]);
        };
        assert_eq!(e, "for_each loop failed: instances [] failed");
    }

    #[test]
    fn no_rollup_while_non_terminal_or_non_running_or_no_instances() {
        let t = task(vec![("x", fs(&[])), ("c", fs(&[])), ("z", fs(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2]"),
            instance("x", 0, "completed", None),
            instance("x", 1, "running", None),
            placeholder("c", "cancelled", "[1]"),
            instance("c", 0, "completed", None),
            placeholder("z", "running", "[1]"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(plan.changes.is_empty());
    }

    #[test]
    fn rollup_for_placeholder_absent_from_flow_is_non_sequential_without_cof() {
        let t = task(vec![]);
        let rows = vec![
            placeholder("x", "running", "[1,2]"),
            instance("x", 0, "failed", None),
            instance("x", 1, "pending", None),
        ];
        // Not sequential → no R5 skip; not all terminal → no rollup.
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(plan.changes.is_empty());
    }

    #[test]
    fn rollup_works_without_workspace_config() {
        let t = task(vec![("x", fs(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1]"),
            instance("x", 0, "completed", Some(json!(1))),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            None,
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["rollup-ok:x"]);
    }

    #[test]
    fn downstream_when_sees_rollup_output_in_the_same_run() {
        let t = task(vec![("x", fs(&[])), ("after", fs(&["x"]))]);
        let rows = vec![
            placeholder("x", "running", "[1]"),
            instance("x", 0, "completed", Some(json!(5))),
            row_when("after", "pending", "{{ x.output[0] == 5 }}"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["rollup-ok:x", "promote:after"]);
    }

    /// P0 runs before the context that P1's `when` templates are rendered
    /// against, so even a root step that does not depend on the loop sees the
    /// rollup output in the same `run`.
    #[test]
    fn independent_root_when_sees_loop_rollup_in_same_run() {
        let t = task(vec![("x", fs(&[])), ("r", fs(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1]"),
            instance("x", 0, "completed", Some(json!(5))),
            row_when("r", "pending", "{{ x.output[0] == 5 }}"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["rollup-ok:x", "promote:r"]);
    }

    /// Instances inherit the placeholder's retry *configuration*. They never
    /// inherit its spent budget: `NewJobStep` has no `retry_attempt` field, so
    /// every inserted row starts at the column default of 0 (asserted against a
    /// real database in `cascade_apply_test`).
    #[test]
    fn instances_start_at_retry_attempt_zero() {
        let t = task(vec![("x", fs(&[]))]);
        let mut ph = placeholder("x", "pending", "[\"a\",\"b\"]");
        ph.retry_attempt = 3;
        ph.max_retries = Some(2);
        ph.retry_backoff_secs = Some(7);
        ph.retry_strategy = Some("exponential".to_string());
        ph.retry_jitter = true;
        let plan = run(
            &t,
            &job(None),
            &[ph],
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let Change::Expand { instances, .. } = &plan.changes[0] else {
            panic!("expected an Expand, got {:?}", plan.changes[0]);
        };
        assert_eq!(instances.len(), 2);
        for inst in instances {
            assert_eq!(inst.max_retries, Some(2));
            assert_eq!(inst.retry_backoff_secs, Some(7));
            assert_eq!(inst.retry_strategy.as_deref(), Some("exponential"));
            assert!(inst.retry_jitter);
        }
    }

    /// A cancelled instance stops a sequential loop the way a failed one does
    /// (R5 failure precedence), but it does not make the loop fail: R6 sees no
    /// `failed` instance and rolls up completed, with `null` for every instance
    /// that produced no output.
    #[test]
    fn sequential_cancelled_middle_without_cof_skips_rest_then_rolls_up_completed() {
        let t = task(vec![("x", fs_seq(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "completed", Some(json!("a"))),
            instance("x", 1, "cancelled", None),
            instance("x", 2, "pending", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["skip:x[2]", "rollup-ok:x"]);
        let Change::Rollup {
            outcome: RollupOutcome::Completed(out),
            ..
        } = &plan.changes[1]
        else {
            panic!("expected a completed rollup, got {:?}", plan.changes[1]);
        };
        assert_eq!(out, &json!(["a", null, null]));
    }

    /// A placeholder whose name is absent from the flow has no `sequential` and
    /// no `continue_on_failure` to read, so R6 defaults both to false and a
    /// failed instance fails the loop.
    #[test]
    fn missing_flow_placeholder_rolls_up_failed_without_cof() {
        let t = task(vec![]);
        let rows = vec![
            placeholder("x", "running", "[1,2]"),
            instance("x", 0, "completed", Some(json!("a"))),
            instance("x", 1, "failed", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["rollup-fail:x"]);
    }

    /// A 300-deep chain behind a failed root cascade-skips entirely in one
    /// `run`, well inside the pass bound.
    #[test]
    fn long_cascade_skip_chain_converges() {
        const N: usize = 300;
        let mut flow = vec![("root".to_string(), fs(&[]))];
        let mut rows = vec![row("root", "failed")];
        for i in 0..N {
            let prev = if i == 0 {
                "root".to_string()
            } else {
                format!("s{}", i - 1)
            };
            flow.push((format!("s{i}"), fs(&[prev.as_str()])));
            rows.push(row(&format!("s{i}"), "pending"));
        }
        let t = task_owned(flow);
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(plan.changes.len(), N, "one Skip per pending step");
        let s = final_statuses(&plan, &rows);
        for i in 0..N {
            assert_eq!(s[&format!("s{i}")], "skipped", "s{i}");
        }
    }

    // ── termination / idempotency / context ──────────────────────────

    #[test]
    fn fixpoint_is_idempotent_on_a_long_linear_chain() {
        let mut flow = vec![("root", fs(&[]))];
        let mut rows = vec![row("root", "completed")];
        for i in 0..300 {
            let name = format!("s{i}");
            let dep = if i == 0 {
                "root".to_string()
            } else {
                format!("s{}", i - 1)
            };
            flow.push((
                Box::leak(name.clone().into_boxed_str()),
                fs(&[Box::leak(dep.into_boxed_str())]),
            ));
            rows.push(row(&name, "pending"));
        }
        let t = task(flow);
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["promote:s0"], "only the first is promotable");
        let mut snap = rows.clone();
        snap[1].status = "ready".to_string();
        let again = run(
            &t,
            &job(None),
            &snap,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(
            again.changes.is_empty(),
            "snapshot at fixpoint yields an empty plan"
        );
    }

    #[test]
    fn fixpoint_promotes_a_wide_fan_out_in_one_pass() {
        // One completed root with 200 independent dependents: every one of them
        // is promotable against the same snapshot, so a single pass clears them.
        const WIDTH: usize = 200;
        let mut flow = vec![("root".to_string(), fs(&[]))];
        let mut rows = vec![row("root", "completed")];
        for i in 0..WIDTH {
            let name = format!("s{i}");
            flow.push((name.clone(), fs(&["root"])));
            rows.push(row(&name, "pending"));
        }
        let t = task_owned(flow);
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();

        assert_eq!(plan.changes.len(), WIDTH, "every dependent is promoted");
        let mut promoted: Vec<String> = names(&plan);
        promoted.sort();
        let mut expected: Vec<String> = (0..WIDTH).map(|i| format!("promote:s{i}")).collect();
        expected.sort();
        assert_eq!(promoted, expected);

        let s = final_statuses(&plan, &rows);
        assert!(
            (0..WIDTH).all(|i| s[&format!("s{i}")] == "ready"),
            "all dependents end ready"
        );

        // And the result is a fixpoint: re-running on the promoted snapshot is a no-op.
        let mut snap = rows.clone();
        for r in snap.iter_mut().skip(1) {
            r.status = "ready".to_string();
        }
        let again = run(
            &t,
            &job(None),
            &snap,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert!(again.changes.is_empty());
    }

    #[test]
    fn job_input_and_secret_reach_when_templates() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![
            row("a", "completed"),
            row_when("b", "pending", "{{ input.fast }}"),
        ];
        let plan = run(
            &t,
            &job(Some(json!({"fast": true}))),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
        let plan = run(
            &t,
            &job(Some(json!({"fast": false}))),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["skip:b"]);

        // A workspace secret reaches the `when` context under `secret`.
        let mut with_secret = ws();
        with_secret.secrets.insert("API_KEY".into(), json!("k"));
        let rows = vec![
            row("a", "completed"),
            row_when("b", "pending", "{{ secret.API_KEY == \"k\" }}"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&with_secret),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(
            names(&plan),
            ["promote:b"],
            "secret reached the when template"
        );
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        // Tera 2: § 3.9 item 3 — comparing against a missing field is false, not an error.
        assert_eq!(
            names(&plan),
            ["skip:b"],
            "without the secret in the config the key is undefined, so the comparison is false"
        );
    }

    /// Mirrors orchestrator_test::test_convergence_without_continue_on_failure.
    #[test]
    fn convergence_without_continue_on_failure_scenario() {
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs(&["a"])),
            ("c", fs(&["a"])),
            ("d", fs(&["b", "c"])),
        ]);
        let rows = vec![
            row("a", "completed"),
            row_when("b", "pending", "{% if input.use_fast %}true{% endif %}"),
            row_when("c", "pending", "{% if not input.use_fast %}true{% endif %}"),
            row("d", "pending"),
        ];
        let plan = run(
            &t,
            &job(Some(json!({"use_fast": true}))),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let statuses = final_statuses(&plan, &rows);
        assert_eq!(statuses["b"], "ready");
        assert_eq!(statuses["c"], "skipped");
        assert_eq!(statuses["d"], "pending", "d waits for b");

        // Once b settles completed, c's choice-skip no longer converges
        // automatically: d needs its own `accept` entry for c's skip to pass
        // (replacing the retired continue_when_skipped).
        let rows2 = vec![
            row("a", "completed"),
            row("b", "completed"),
            row_skipped("c", "condition"),
            row("d", "pending"),
        ];
        let plan2 = run(
            &t,
            &job(Some(json!({"use_fast": true}))),
            &rows2,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan2), [s("d", "unreachable")]);

        // With d's own depends_on accepting both b's Completed and c's
        // Skipped outcome, d promotes.
        let t2 = task(vec![
            ("a", fs(&[])),
            ("b", fs(&["a"])),
            ("c", fs(&["a"])),
            (
                "d",
                fs_with_accept(&[
                    ("b", &[Outcome::Completed]),
                    ("c", &[Outcome::Completed, Outcome::Skipped]),
                ]),
            ),
        ]);
        let plan3 = run(
            &t2,
            &job(Some(json!({"use_fast": true}))),
            &rows2,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan3), ["promote:d"]);
    }

    // --- for_each expression parser tests (moved with the helpers from job_creator) ---

    #[test]
    fn test_parse_for_each_items_literal_array() {
        let expr = r#"["us-east-1","eu-west-1","ap-south-1"]"#;
        let ctx = json!({});
        let items = parse_for_each_items(expr, &ctx).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].as_str().unwrap(), "us-east-1");
        assert_eq!(items[1].as_str().unwrap(), "eu-west-1");
        assert_eq!(items[2].as_str().unwrap(), "ap-south-1");
    }

    #[test]
    fn test_parse_for_each_items_json_encoded_template_string() {
        // When a for_each Tera template is serialised via serde_json::Value::to_string()
        // the string is JSON-encoded (quoted). parse_for_each_items parses the outer JSON
        // string value, discovers it is a Tera template, renders it, then parses the
        // rendered result as a JSON array.
        let expr = r#""{{ input.items }}""#; // JSON-encoded Tera template
        let ctx = json!({"input": {"items": "[1,2,3]"}});
        let items = parse_for_each_items(expr, &ctx).unwrap();
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn test_parse_for_each_items_non_array_fails() {
        let expr = r#"42"#;
        let ctx = json!({});
        let result = parse_for_each_items(expr, &ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert_eq!(msg, "for_each must render a JSON array, got a JSON number");
    }

    /// Codex review (R19): a literal JSON `for_each` is template SOURCE — a
    /// YAML literal can hold a literal secret — so the error names the JSON
    /// type only, in `parse_for_each_items` and in the persisted step error.
    #[test]
    fn literal_for_each_error_names_the_json_type_only() {
        let expr = r#"{"token":"literal-secret"}"#;
        let err = parse_for_each_items(expr, &json!({})).unwrap_err();
        let text = format!("{err:#} {err:?}");
        assert!(!text.contains("literal-secret"), "{text}");
        assert!(!text.contains("token"), "{text}");
        assert_eq!(
            format!("{err:#}"),
            "for_each must render a JSON array, got a JSON object"
        );

        let t = task(vec![("loop", fs(&[]))]);
        let rows = vec![placeholder("loop", "pending", expr)];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let persisted = plan
            .changes
            .iter()
            .find_map(|c| match c {
                Change::Fail { step, error } if step == "loop" => Some(error.clone()),
                _ => None,
            })
            .expect("the placeholder fails");
        assert!(!persisted.contains("literal-secret"), "{persisted}");
        assert!(persisted.contains("got a JSON object"), "{persisted}");
    }

    #[test]
    fn test_parse_for_each_items_empty_array() {
        let expr = r#"[]"#;
        let ctx = json!({});
        let items = parse_for_each_items(expr, &ctx).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn test_parse_for_each_items_numeric_elements() {
        let expr = r#"[1, 2, 3]"#;
        let ctx = json!({});
        let items = parse_for_each_items(expr, &ctx).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].as_i64().unwrap(), 1);
        assert_eq!(items[2].as_i64().unwrap(), 3);
    }

    // --- render_for_each_template: error message tests ---

    #[test]
    fn for_each_errors_never_contain_rendered_content() {
        let ctx = json!({"secret": {"X": "for-each-canary"}});
        let err = render_for_each_template("{{ secret.X | upper }}", &ctx).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            !text.contains("FOR-EACH-CANARY") && !text.contains("for-each-canary"),
            "{text}"
        );
        assert!(text.contains("is not valid JSON"), "{text}");
        let err = render_for_each_template("{{ secret | json_encode() }}", &ctx).unwrap_err();
        let text = format!("{err:#}");
        assert!(!text.contains("for-each-canary"), "{text}");
        assert!(text.contains("got a JSON object"), "{text}");
    }

    #[test]
    fn test_for_each_non_json_error_without_object_hint() {
        // Shape-only error: it never echoes the rendered text.
        let ctx = json!({"step1": {"output": "hello"}});
        let result = render_for_each_template("{{ step1.output }}", &ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("is not valid JSON"),
            "Error should mention invalid JSON: {}",
            msg
        );
        assert!(
            !msg.contains("hello"),
            "Error must not echo the rendered text: {}",
            msg
        );
    }

    #[test]
    fn test_for_each_valid_json_non_array_errors() {
        // A template that renders to valid JSON but not an array should
        // produce a clear "must render a JSON array" error.
        let ctx = json!({"count": 5});
        let result = render_for_each_template("{{ count }}", &ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("got a JSON number"),
            "Error should mention array requirement: {}",
            msg
        );
    }

    #[test]
    fn test_for_each_json_encode_filter_produces_correct_result() {
        // Validates that applying json_encode() — the fix suggested by the
        // error hint — actually works for arrays of objects.
        let ctx = json!({"step1": {"output": [{"x": 1}, {"x": 2}]}});
        let items = render_for_each_template("{{ step1.output | json_encode() }}", &ctx).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["x"], 1);
        assert_eq!(items[1]["x"], 2);
    }

    // ── accept-set tolerance + skip reasons (spec 2026-10-01 §2, replacing
    // the retired continue_when_skipped/continue_on_failure gate interplay
    // from spec 2026-09-09) ──

    #[test]
    fn dependent_accepting_skipped_promotes_past_a_condition_skip() {
        let t = task(vec![
            ("a", fs(&[])),
            (
                "b",
                fs_with_accept(&[("a", &[Outcome::Completed, Outcome::Skipped])]),
            ),
        ]);
        let rows = vec![row_skipped("a", "condition"), row("b", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
    }

    #[test]
    fn dependent_accepting_skipped_promotes_past_an_empty_loop_skip() {
        let t = task(vec![
            ("a", fs(&[])),
            (
                "b",
                fs_with_accept(&[("a", &[Outcome::Completed, Outcome::Skipped])]),
            ),
        ]);
        let rows = vec![row_skipped("a", "empty"), row("b", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
    }

    #[test]
    fn accepting_skipped_still_evaluates_its_own_falsy_when_as_condition() {
        // Even though b's `accept` passes a's choice-skip through the gate,
        // b's own `when` is still evaluated on its own terms.
        let t = task(vec![
            ("a", fs(&[])),
            (
                "b",
                fs_with_accept(&[("a", &[Outcome::Completed, Outcome::Skipped])]),
            ),
        ]);
        let rows = vec![
            row_skipped("a", "condition"),
            row_when("b", "pending", "false"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("b", "condition")]);
    }

    #[test]
    fn cof_alone_no_longer_bypasses_a_choice_skip() {
        // continue_on_failure on the skipped dependency does not pass a
        // choice skip — cof no longer affects the gate at all; only the
        // dependent's own `accept` set does (spec 2026-10-01 §2.3).
        let t = task(vec![("a", fs_cof(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row_skipped("a", "condition"), row("b", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable")]);
    }

    #[test]
    fn accept_on_dependent_only_is_what_bypasses_now() {
        // The old flag lived on the skipped dependency; `accept` lives on
        // the dependent that would benefit, by design (spec §2.2).
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row_skipped("a", "condition"), row("b", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable")]);
    }

    #[test]
    fn accept_requires_every_dependency_to_be_individually_satisfied() {
        // c depends on a (tolerated skip) and b (not tolerated); both skipped
        // by condition. Every edge must be satisfied on its own terms for c
        // to be open — tolerating one does not blanket-tolerate the group.
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs(&[])),
            (
                "c",
                fs_with_accept(&[
                    ("a", &[Outcome::Completed, Outcome::Skipped]),
                    ("b", &[Outcome::Completed]),
                ]),
            ),
        ]);
        let rows = vec![
            row_skipped("a", "condition"),
            row_skipped("b", "condition"),
            row("c", "pending"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("c", "unreachable")]);
    }

    #[test]
    fn cws_with_unreachable_dep_is_skipped_unreachable() {
        let t = task(vec![("a", fs_cws(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row_skipped("a", "unreachable"), row("b", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable")]);
    }

    #[test]
    fn cws_with_mixed_condition_and_unreachable_deps_is_skipped_unreachable() {
        // "any tainted dependency" (spec §2.2): one benign branch must not
        // launder a failure, even when every skipped dependency carries the
        // flag.
        let t = task(vec![
            ("a", fs_cws(&[])),
            ("b", fs_cws(&[])),
            ("c", fs(&["a", "b"])),
        ]);
        let rows = vec![
            row_skipped("a", "condition"),
            row_skipped("b", "unreachable"),
            row("c", "pending"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("c", "unreachable")]);
    }

    #[test]
    fn accepting_omitted_passes_its_dependent() {
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs_with_accept(&[("a", &[Outcome::Omitted])])),
        ]);
        let rows = vec![row_skipped("a", "unreachable"), row("b", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
    }

    #[test]
    fn accept_sets_combine_across_a_two_hop_chain() {
        // x failed → y accepts x's Failed outcome and reaches its own `when`
        // (false → condition skip); z accepts y's Skipped outcome.
        let mut y = fs_with_accept(&[("x", &[Outcome::Failed])]);
        y.when = Some("false".to_string());
        let t = task(vec![
            ("x", fs(&[])),
            ("y", y),
            (
                "z",
                fs_with_accept(&[("y", &[Outcome::Completed, Outcome::Skipped])]),
            ),
        ]);
        let rows = vec![
            row("x", "failed"),
            row_when("y", "pending", "false"),
            row("z", "pending"),
        ];
        let plan = run_default(&t, &rows);
        assert!(skips(&plan).contains(&s("y", "condition")), "{:?}", plan);
        assert!(
            names(&plan).contains(&"promote:z".to_string()),
            "{:?}",
            names(&plan)
        );
    }

    #[test]
    fn null_reason_counts_as_unreachable() {
        // Pre-migration / older-replica rows (spec §2.2).
        let t = task(vec![("a", fs_cws(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row("a", "skipped"), row("b", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable")]);
    }

    #[test]
    fn unreachable_propagates_through_a_chain_in_one_run() {
        // a failed → b (plain, default accept) → c: b unreachable (omitted),
        // c unreachable too — b's own gate has no accept for a's failure, and
        // c's own gate has no accept for b's resulting omission, so the
        // block passes straight through to c.
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"])), ("c", fs(&["b"]))]);
        let rows = vec![row("a", "failed"), row("b", "pending"), row("c", "pending")];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable"), s("c", "unreachable")]);

        // b's own continue_on_failure does not rescue b from a's failure
        // (only b's own `accept` could) — b is still skipped unreachable —
        // but c's own `accept` for b's Omitted outcome lets c promote.
        let t_accept = task(vec![
            ("a", fs(&[])),
            ("b", fs(&["a"])),
            ("c", fs_with_accept(&[("b", &[Outcome::Omitted])])),
        ]);
        let rows = vec![row("a", "failed"), row("b", "pending"), row("c", "pending")];
        let plan = run(
            &t_accept,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable")]);
        assert!(
            names(&plan).contains(&"promote:c".to_string()),
            "{:?}",
            names(&plan)
        );
    }

    #[test]
    fn condition_skip_propagates_and_is_accepted_two_hops_downstream_in_one_run() {
        // x completed → a (when false, own-choice skip) → b (plain, default
        // accept: blocked by a's skip, itself becomes omitted/unreachable) →
        // c (accepts b's Omitted outcome): a condition, b unreachable, c
        // promoted — all three decided within the merged relay's single
        // outer pass, replacing the old cws-on-the-skipped-step mechanism.
        let t = task(vec![
            ("x", fs(&[])),
            ("a", fs(&["x"])),
            ("b", fs(&["a"])),
            ("c", fs_with_accept(&[("b", &[Outcome::Omitted])])),
        ]);
        let rows = vec![
            row("x", "completed"),
            row_when("a", "pending", "false"),
            row("b", "pending"),
            row("c", "pending"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(skips(&plan), [s("a", "condition"), s("b", "unreachable")]);
        assert!(
            names(&plan).contains(&"promote:c".to_string()),
            "{:?}",
            names(&plan)
        );
    }

    #[test]
    fn three_hop_chain_reasons_match_statuses() {
        // Spec §5: the merged relay resolves this whole 3-hop chain within a
        // single outer pass's inner relay (not 3 outer passes, as the old
        // P1-then-P2 one-hop-per-pass split needed) — but the externally
        // observed `run()` output is the same set of decisions either way,
        // and the reason always travels with the status: every omitted link
        // is `unreachable` now (spec §2.1: `cascade` is retired).
        let t = task(vec![
            ("x", fs(&[])),
            ("a", fs(&["x"])),
            ("b", fs(&["a"])),
            ("c", fs(&["b"])),
        ]);
        let rows = vec![
            row("x", "completed"),
            row_when("a", "pending", "false"),
            row("b", "pending"),
            row("c", "pending"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        assert_eq!(
            skips(&plan),
            [
                s("a", "condition"),
                s("b", "unreachable"),
                s("c", "unreachable")
            ]
        );
        let mut snap = Snapshot::new(rows.clone());
        for c in &plan.changes {
            snap.apply(c);
        }
        assert_eq!(snap.skip_reason("a"), Some("condition"));
        assert_eq!(snap.skip_reason("b"), Some("unreachable"));
        assert_eq!(snap.skip_reason("c"), Some("unreachable"));
    }

    fn run_default(t: &TaskDef, rows: &[JobStepRow]) -> Plan {
        run(
            t,
            &job(None),
            rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap()
    }

    #[test]
    fn mixed_completed_and_unreachable_skipped_deps_skip_unreachable() {
        // A healthy sibling must not launder an upstream failure: `b` was skipped
        // BECAUSE something upstream failed, so `c` is unreachable too.
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let rows = vec![
            row("a", "completed"),
            row_skipped("b", "unreachable"),
            row("c", "pending"),
        ];
        let plan = run_default(&t, &rows);
        assert_eq!(names(&plan), ["skip:c"]);
        assert_eq!(skips(&plan), [s("c", "unreachable")]);
    }

    #[test]
    fn mixed_completed_and_reasonless_skipped_dep_skips_unreachable() {
        // A skip with no recorded reason (pre-046 rows, e.g. carried into a restart)
        // is read as unreachable, exactly as the all-deps-skipped rule reads it.
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let rows = vec![
            row("a", "completed"),
            row("b", "skipped"),
            row("c", "pending"),
        ];
        let plan = run_default(&t, &rows);
        assert_eq!(skips(&plan), [s("c", "unreachable")]);
    }

    #[test]
    fn mixed_completed_and_condition_skipped_dep_is_unreachable() {
        // A branch switched off by its own `when` is a choice, not a failure
        // — but without the dependent's own `accept` for it, it still blocks c.
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let rows = vec![
            row("a", "completed"),
            row_skipped("b", "condition"),
            row("c", "pending"),
        ];
        let plan = run_default(&t, &rows);
        assert_eq!(skips(&plan), [s("c", "unreachable")]);
    }

    #[test]
    fn mixed_completed_and_accepted_condition_skipped_dep_promotes() {
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs(&[])),
            (
                "c",
                fs_with_accept(&[
                    ("a", &[Outcome::Completed]),
                    ("b", &[Outcome::Completed, Outcome::Skipped]),
                ]),
            ),
        ]);
        let rows = vec![
            row("a", "completed"),
            row_skipped("b", "condition"),
            row("c", "pending"),
        ];
        let plan = run_default(&t, &rows);
        assert_eq!(names(&plan), ["promote:c"]);
    }

    #[test]
    fn dependent_accepting_omitted_promotes_among_completed_ones() {
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs(&[])),
            (
                "c",
                fs_with_accept(&[("a", &[Outcome::Completed]), ("b", &[Outcome::Omitted])]),
            ),
        ]);
        let rows = vec![
            row("a", "completed"),
            row_skipped("b", "unreachable"),
            row("c", "pending"),
        ];
        let plan = run_default(&t, &rows);
        assert_eq!(names(&plan), ["promote:c"]);
    }

    #[test]
    fn placeholder_with_completed_and_unreachable_skipped_deps_is_retired_not_expanded() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("m", fs(&["a", "b"]))]);
        let rows = vec![
            row("a", "completed"),
            row_skipped("b", "unreachable"),
            placeholder("m", "pending", "[1,2]"),
        ];
        let plan = run_default(&t, &rows);
        assert_eq!(names(&plan), ["skip:m"]);
        assert_eq!(skips(&plan), [s("m", "unreachable")]);
    }

    #[test]
    fn failure_on_one_branch_stops_the_merge_and_everything_after_it() {
        // Three parallel branches feed a for_each merge; one branch's first step
        // failed while the other two completed. Nothing downstream of the merge
        // may run.
        let t = task(vec![
            ("pred_master", fs(&[])),
            ("imp_master", fs(&["pred_master"])),
            ("imp_beta", fs(&[])),
            ("imp_stage", fs(&[])),
            ("merge", fs(&["imp_master", "imp_beta", "imp_stage"])),
            ("agg", fs(&["merge"])),
            ("upload", fs(&["agg"])),
        ]);
        let rows = vec![
            row("pred_master", "failed"),
            row("imp_master", "pending"),
            row("imp_beta", "completed"),
            row("imp_stage", "completed"),
            placeholder("merge", "pending", "[1]"),
            row("agg", "pending"),
            row("upload", "pending"),
        ];
        let plan = run_default(&t, &rows);

        let mut got = skips(&plan);
        got.sort();
        assert_eq!(
            got,
            [
                s("agg", "unreachable"),
                s("imp_master", "unreachable"),
                s("merge", "unreachable"),
                s("upload", "unreachable"),
            ]
        );
        let statuses = final_statuses(&plan, &rows);
        assert!(
            !statuses.keys().any(|k| k.starts_with("merge[")),
            "the merge placeholder must not expand: {statuses:?}"
        );
    }

    #[test]
    fn reason_on_r3_unreachable_and_r5_sequential_stop() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"])), ("x", fs_seq(&[]))]);
        let rows = vec![
            row("a", "failed"),
            row("b", "pending"),
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "failed", None),
            instance("x", 1, "pending", None),
            instance("x", 2, "pending", None),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let sk = skips(&plan);
        assert!(sk.contains(&s("b", "unreachable")), "{sk:?}");
        assert!(sk.contains(&s("x[1]", "unreachable")), "{sk:?}");
        assert!(sk.contains(&s("x[2]", "unreachable")), "{sk:?}");
    }

    #[test]
    fn reason_on_r4_placeholder_condition_empty_unreachable_and_accepted() {
        let t = task(vec![
            ("root", fs(&[])),
            ("dead", fs(&[])),
            ("gone", fs(&[])),
            ("gone_accepted", fs(&[])),
            ("p_when", fs(&["root"])),
            ("p_empty", fs(&["root"])),
            ("p_unreach", fs(&["dead"])),
            ("p_omitted", fs(&["gone"])),
            (
                "p_accepted",
                fs_with_accept(&[("gone_accepted", &[Outcome::Completed, Outcome::Skipped])]),
            ),
        ]);
        let rows = vec![
            row("root", "completed"),
            row("dead", "failed"),
            row_skipped("gone", "condition"),
            row_skipped("gone_accepted", "condition"),
            JobStepRow {
                when_condition: Some("false".to_string()),
                ..placeholder("p_when", "pending", "[1]")
            },
            placeholder("p_empty", "pending", "[]"),
            placeholder("p_unreach", "pending", "[1]"),
            placeholder("p_omitted", "pending", "[1]"),
            placeholder("p_accepted", "pending", "[1]"),
        ];
        let plan = run(
            &t,
            &job(None),
            &rows,
            Some(&ws()),
            &crate::render_context::Snapshots::default(),
        )
        .unwrap();
        let sk = skips(&plan);
        assert!(sk.contains(&s("p_when", "condition")), "{sk:?}");
        assert!(sk.contains(&s("p_empty", "empty")), "{sk:?}");
        assert!(sk.contains(&s("p_unreach", "unreachable")), "{sk:?}");
        assert!(sk.contains(&s("p_omitted", "unreachable")), "{sk:?}");
        assert!(
            names(&plan).contains(&"expand:p_accepted:1".to_string()),
            "a placeholder whose own accept tolerates the condition skip expands: {:?}",
            names(&plan)
        );
    }

    #[test]
    fn uniform_barrier_waits_for_a_running_sibling_even_with_a_failed_dep() {
        // Spec §2.3: today's fail-fast BlockFail dominance is retired — even
        // a tree already conclusively unsatisfiable by one child (a failed)
        // waits for every other referenced step (b, still running) before c
        // is decided at all.
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let rows = vec![row("a", "failed"), row("b", "running"), row("c", "pending")];
        let plan = run_default(&t, &rows);
        assert!(plan.changes.is_empty(), "{plan:?}");
    }

    #[test]
    fn cascade_skip_waits_for_a_running_sibling_then_becomes_unreachable() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let waiting = vec![
            row_skipped("a", "condition"),
            row("b", "running"),
            row("c", "pending"),
        ];
        assert!(run_default(&t, &waiting).changes.is_empty());
        let failed = vec![
            row_skipped("a", "condition"),
            row("b", "failed"),
            row("c", "pending"),
        ];
        assert_eq!(skips(&run_default(&t, &failed)), [s("c", "unreachable")]);
    }

    #[test]
    fn dependent_accepting_a_failed_placeholder_promotes() {
        let t = task(vec![
            ("p", fs_cof(&[])),
            ("d", fs_with_accept(&[("p", &[Outcome::Failed])])),
        ]);
        let rows = vec![placeholder("p", "failed", "[1]"), row("d", "pending")];
        assert_eq!(names(&run_default(&t, &rows)), ["promote:d"]);
    }

    #[test]
    fn placeholder_with_completed_and_condition_skipped_deps_is_unreachable() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("m", fs(&["a", "b"]))]);
        let rows = vec![
            row("a", "completed"),
            row_skipped("b", "condition"),
            placeholder("m", "pending", "[1,2]"),
        ];
        assert_eq!(skips(&run_default(&t, &rows)), [s("m", "unreachable")]);
    }

    // ── pass timing (spec §5, Codex rounds 1-3) ──────────────────────
    // An independent placeholder `p` whose `when` sees `b` only once `b` has
    // a row status other than pending (skipped rows enter the context).
    fn observer() -> JobStepRow {
        JobStepRow {
            when_condition: Some("{% if b is defined %}true{% else %}false{% endif %}".to_string()),
            ..placeholder("p", "pending", "[1]")
        }
    }
    fn timing_task(b: FlowStep) -> TaskDef {
        task(vec![
            ("x", fs(&[])),
            ("a", fs(&["x"])),
            ("b", b),
            ("p", fs(&[])),
        ])
    }

    #[test]
    fn timing_unreachable_chain_keeps_legacy_pass() {
        // x unreachable → a (P1, all deps skipped) → b (P2) → p sees b, expands.
        let t = timing_task(fs(&["a"]));
        let rows = vec![
            row_skipped("x", "unreachable"),
            row("a", "pending"),
            row("b", "pending"),
            observer(),
        ];
        let n = names(&run_default(&t, &rows));
        assert!(n.contains(&"expand:p:1".to_string()), "{n:?}");
    }

    #[test]
    fn timing_failed_root_now_resolves_same_pass_as_unreachable_root() {
        // x failed -> a -> b -> p (placeholder). Under the old P1/P2 split, a
        // failed (not skipped) root needed pass 2 to reach b, stranding p's
        // already-committed condition-skip from pass 1. The merged relay
        // decides a, then b, inside pass 1's single merged phase, before p (in
        // P3) ever runs — so p now expands, matching the unreachable-root case
        // exactly. Spec §5.
        let t = timing_task(fs(&["a"]));
        let rows = vec![
            row("x", "failed"),
            row("a", "pending"),
            row("b", "pending"),
            observer(),
        ];
        let n = names(&run_default(&t, &rows));
        assert!(n.contains(&"expand:p:1".to_string()), "{n:?}");
    }

    #[test]
    fn timing_accepted_change_dependent_cof_no_longer_delays() {
        // 0.16 left b (own cof) pending in P2; now b is skipped in P2 → p expands.
        let t = timing_task(fs_cof(&["a"]));
        let rows = vec![
            row_skipped("x", "unreachable"),
            row("a", "pending"),
            row("b", "pending"),
            observer(),
        ];
        let n = names(&run_default(&t, &rows));
        assert!(n.contains(&"expand:p:1".to_string()), "{n:?}");
    }

    #[test]
    fn timing_accepted_change_unknown_reason_is_failure_class() {
        let t = timing_task(fs(&["a"]));
        let rows = vec![
            row_skipped("x", "brand-new"),
            row("a", "pending"),
            row("b", "pending"),
            observer(),
        ];
        let plan = run_default(&t, &rows);
        assert!(skips(&plan).contains(&s("a", "unreachable")));
        assert!(names(&plan).contains(&"expand:p:1".to_string()));
    }

    #[test]
    fn timing_flagged_placeholder_now_waits_for_a_pending_sibling() {
        // l depends on [x (already unreachable), y (still running)]. Today's
        // fail-fast BlockFail dominance retires l immediately, ignoring y. The
        // uniform barrier (spec §2.3) waits for y before deciding anything.
        let t = task(vec![
            ("x", fs(&[])),
            ("y", fs(&[])),
            ("l", fs_cof(&["x", "y"])),
        ]);
        let rows = vec![
            row_skipped("x", "unreachable"),
            row("y", "running"),
            placeholder("l", "pending", "[1]"),
        ];
        let plan = run_default(&t, &rows);
        assert!(
            skips(&plan).is_empty(),
            "l must not retire yet — y is still running: {plan:?}"
        );
    }

    fn fs_with_accept(deps: &[(&str, &[stroem_common::depends_on::Outcome])]) -> FlowStep {
        FlowStep {
            depends_on: deps
                .iter()
                .map(|(name, accept)| {
                    stroem_common::depends_on::DependsOnEntry::Step(
                        stroem_common::depends_on::StepEntry {
                            step: name.to_string(),
                            accept: stroem_common::depends_on::AcceptSet::Outcomes(accept.to_vec()),
                        },
                    )
                })
                .collect(),
            ..fs(&[])
        }
    }

    #[test]
    fn inner_relay_rebuilds_context_for_each_batch_not_once_for_the_whole_pass() {
        // a(when: false); b accepts a's Skipped outcome AND tests in its own
        // `when` whether `a` is defined. If the relay reused one stale context
        // across inner iterations, b (decided in a later inner iteration than
        // a) would incorrectly see `a` as still absent. Spec §5's
        // context/batch contract.
        //
        // Note: `phase_promote` evaluates a step's own `when` from the ROW's
        // `when_condition` (set here via `row_when`), never from the
        // `FlowStep.when` in `task.flow` — this harness (unlike real job
        // creation) does not sync the two, so the rows carry the conditions,
        // not the `FlowStep`s built below.
        let t = task(vec![
            ("a", fs(&[])),
            (
                "b",
                fs_with_accept(&[("a", &[Outcome::Completed, Outcome::Skipped])]),
            ),
        ]);
        let rows = vec![
            row_when("a", "pending", "false"),
            row_when(
                "b",
                "pending",
                "{% if a is defined %}true{% else %}false{% endif %}",
            ),
        ];
        let plan = run_default(&t, &rows);
        assert!(
            names(&plan).contains(&"promote:b".to_string()),
            "b must see a's Skipped outcome and run, not condition-skip on a stale read: {plan:?}"
        );
    }

    // ── migration timing: in-flight loop rollup cutover (spec §11) ─────

    #[test]
    fn a_loop_already_rolled_up_before_upgrade_keeps_its_historical_status_hiding_a_failure() {
        // Simulates a loop that finished (rolled up to completed, hiding a
        // tolerated failure) before the upgrade took effect — P0 only
        // visits RUNNING placeholders, so an already-terminal one must
        // never be revisited or re-rolled-up.
        let t = task(vec![("p", fs_cof(&[]))]);
        let rows = vec![
            placeholder("p", "completed", "[1]"), // already rolled up, pre-upgrade style
            instance("p", 0, "failed", None),
        ];
        let plan = phase_rollup(&Snapshot::new(rows), &t);
        assert!(
            plan.is_empty(),
            "an already-completed placeholder must not be re-rolled-up: {plan:?}"
        );
    }

    #[test]
    fn a_loop_still_running_at_upgrade_time_rolls_up_under_the_new_rule() {
        let t = task(vec![("p", fs_cof(&[]))]);
        let rows = vec![
            placeholder("p", "running", "[1]"),
            instance("p", 0, "failed", None),
        ];
        let plan = phase_rollup(&Snapshot::new(rows), &t);
        let rolled_up_as_failed = plan.iter().any(|c| {
            matches!(
                c,
                Change::Rollup {
                    outcome: RollupOutcome::Failed(..),
                    ..
                }
            )
        });
        assert!(
            rolled_up_as_failed,
            "a mid-flight loop must roll up under the new (truthful-status) rule: {plan:?}"
        );
    }
}
