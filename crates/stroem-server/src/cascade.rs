//! Step cascade: the pure fixpoint that moves a job's non-running steps.
//! See docs/superpowers/specs/2026-09-08-step-cascade-design.md.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_db::{JobRepo, JobRow, JobStepRepo, JobStepRow, NewJobStep};
use uuid::Uuid;

use crate::job_creator::build_step_render_context;

/// One state transition the cascade wants applied. Closed enum.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// pending → ready
    Promote { step: String },
    /// pending → skipped
    Skip { step: String },
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
    Failed(String),
}

/// What one `run` decided, in production order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub changes: Vec<Change>,
}

/// A step is terminal in one of these four statuses.
pub(crate) fn is_terminal(status: &str) -> bool {
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
pub(crate) fn parse_for_each_items(
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
            _ => {
                bail!(
                    "for_each expression must evaluate to a JSON array, got {}",
                    value
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
    let value: serde_json::Value = serde_json::from_str(&rendered).with_context(|| {
        // Detect the common "[object]" rendering that Tera produces for
        // objects/arrays and suggest the fix.
        if rendered.contains("[object]") {
            format!(
                "for_each template rendered to non-JSON: {}. \
                 Hint: Tera renders objects/arrays as \"[object]\". \
                 Use the `json_encode()` filter, e.g. {{{{ step.output | json_encode() }}}}",
                rendered,
            )
        } else {
            format!("for_each template rendered to non-JSON: {}", rendered)
        }
    })?;
    match value {
        serde_json::Value::Array(arr) => Ok(arr),
        _ => bail!(
            "for_each expression must evaluate to a JSON array, got {}",
            value
        ),
    }
}

/// In-memory copy of a job's step rows that the fixpoint mutates.
pub(crate) struct Snapshot {
    pub(crate) rows: Vec<JobStepRow>,
    index: HashMap<String, usize>,
}

impl Snapshot {
    pub(crate) fn new(rows: Vec<JobStepRow>) -> Self {
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

    fn get_mut(&mut self, name: &str) -> Option<&mut JobStepRow> {
        let i = *self.index.get(name)?;
        Some(&mut self.rows[i])
    }

    /// Apply one change to the in-memory rows (§4.4).
    pub(crate) fn apply(&mut self, change: &Change) {
        match change {
            Change::Promote { step } => {
                if let Some(r) = self.get_mut(step) {
                    r.status = READY.to_string();
                }
            }
            Change::Skip { step } => {
                if let Some(r) = self.get_mut(step) {
                    r.status = SKIPPED.to_string();
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
                        RollupOutcome::Failed(e) => {
                            r.status = FAILED.to_string();
                            r.error_message = Some(e.clone());
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

// ── dependency predicates (§4.3) ─────────────────────────────────────

fn deps_satisfied(snap: &Snapshot, fs: &FlowStep) -> bool {
    fs.depends_on.iter().all(|d| match snap.status(d) {
        Some(COMPLETED) | Some(SKIPPED) => true,
        Some(FAILED) | Some(CANCELLED) => fs.continue_on_failure,
        _ => false,
    })
}

fn all_deps_skipped(snap: &Snapshot, fs: &FlowStep) -> bool {
    !fs.depends_on.is_empty()
        && fs
            .depends_on
            .iter()
            .all(|d| snap.status(d) == Some(SKIPPED))
}

fn any_dep_failed_or_cancelled(snap: &Snapshot, fs: &FlowStep) -> bool {
    fs.depends_on
        .iter()
        .any(|d| matches!(snap.status(d), Some(FAILED) | Some(CANCELLED)))
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
                let next_idx = i.loop_index.unwrap_or(0) + 1;
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
            let failed_indices: Vec<i32> = instances
                .iter()
                .filter(|i| i.status == FAILED)
                .map(|i| i.loop_index.unwrap_or(0))
                .collect();
            let outcome = if !failed_indices.is_empty() && !cof {
                RollupOutcome::Failed(format!(
                    "for_each loop failed: instances {:?} failed",
                    failed_indices
                ))
            } else {
                RollupOutcome::Completed(Value::Array(
                    instances
                        .iter()
                        .map(|i| i.output.clone().unwrap_or(Value::Null))
                        .collect(),
                ))
            };
            out.push(Change::Rollup {
                placeholder: ph.step_name.clone(),
                outcome,
            });
        }
    }
    out
}

/// P1: R1 cascade-skip, then R2 promote (with `when`).
fn phase_promote(snap: &Snapshot, task: &TaskDef, ctx: Option<&Value>) -> Vec<Change> {
    let mut out = Vec::new();
    for r in snap
        .rows
        .iter()
        .filter(|r| r.status == PENDING && !is_placeholder(r))
    {
        let Some(fs) = task.flow.get(&r.step_name) else {
            continue;
        };
        if all_deps_skipped(snap, fs) && !fs.continue_on_failure {
            out.push(Change::Skip {
                step: r.step_name.clone(),
            });
            continue;
        }
        if !deps_satisfied(snap, fs) {
            continue;
        }
        match (&r.when_condition, ctx) {
            (None, _) => out.push(Change::Promote {
                step: r.step_name.clone(),
            }),
            (Some(_), None) => {} // no template context: stays pending (today's behaviour)
            (Some(w), Some(ctx)) => match stroem_common::template::evaluate_condition(w, ctx) {
                Ok(true) => out.push(Change::Promote {
                    step: r.step_name.clone(),
                }),
                Ok(false) => out.push(Change::Skip {
                    step: r.step_name.clone(),
                }),
                Err(e) => out.push(Change::Fail {
                    step: r.step_name.clone(),
                    error: format!("when condition error: {:#}", e),
                }),
            },
        }
    }
    out
}

/// P2: R3 skip unreachable.
fn phase_skip_unreachable(snap: &Snapshot, task: &TaskDef) -> Vec<Change> {
    let mut out = Vec::new();
    for r in snap
        .rows
        .iter()
        .filter(|r| r.status == PENDING && !is_placeholder(r))
    {
        let Some(fs) = task.flow.get(&r.step_name) else {
            continue;
        };
        if !fs.continue_on_failure && any_dep_failed_or_cancelled(snap, fs) {
            out.push(Change::Skip {
                step: r.step_name.clone(),
            });
        }
    }
    out
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
        if !deps_satisfied(snap, fs) {
            if any_dep_failed_or_cancelled(snap, fs) && !fs.continue_on_failure {
                out.push(Change::Skip {
                    step: r.step_name.clone(),
                });
            }
            continue;
        }
        if all_deps_skipped(snap, fs) && !fs.continue_on_failure {
            out.push(Change::Skip {
                step: r.step_name.clone(),
            });
            continue;
        }
        if let Some(w) = &r.when_condition {
            match stroem_common::template::evaluate_condition(w, ctx) {
                Ok(true) => {}
                Ok(false) => {
                    out.push(Change::Skip {
                        step: r.step_name.clone(),
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

/// The pure fixpoint (§4.4). Renders templates; never touches the database.
/// `workspace_config == None` reproduces today's "no template context" mode.
// used from Task 2 onward
pub fn run(
    task: &TaskDef,
    job: &JobRow,
    steps: &[JobStepRow],
    workspace_config: Option<&WorkspaceConfig>,
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

        let ctx_a = workspace_config.map(|ws| build_step_render_context(job, &snap.rows, ws));
        let p1 = phase_promote(&snap, task, ctx_a.as_ref());
        apply_all(&mut snap, &p1);
        pass.extend(p1);

        let p2 = phase_skip_unreachable(&snap, task);
        apply_all(&mut snap, &p2);
        pass.extend(p2);

        let ctx_b = workspace_config.map(|ws| build_step_render_context(job, &snap.rows, ws));
        let p3 = phase_placeholders(&snap, task, ctx_b.as_ref(), job.job_id);
        apply_all(&mut snap, &p3);
        pass.extend(p3);

        if pass.is_empty() {
            break;
        }
        changes.extend(pass);
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
    conn: &mut sqlx::PgConnection,
    job_id: Uuid,
    plan: &Plan,
) -> Result<Applied, ApplyError> {
    let mut a = Applied::default();
    let mut i = 0;
    let changes = &plan.changes;
    while i < changes.len() {
        match &changes[i] {
            Change::Promote { .. } => {
                let mut names = Vec::new();
                while let Some(Change::Promote { step }) = changes.get(i) {
                    names.push(step.clone());
                    i += 1;
                }
                let n = JobStepRepo::promote_steps_tx(&mut *conn, job_id, &names).await?;
                expect_rows(n, names.len(), &names.join(","))?;
                a.promoted += names.len();
            }
            Change::Skip { .. } => {
                let mut names = Vec::new();
                while let Some(Change::Skip { step }) = changes.get(i) {
                    names.push(step.clone());
                    i += 1;
                }
                let n = JobStepRepo::skip_steps_tx(&mut *conn, job_id, &names).await?;
                expect_rows(n, names.len(), &names.join(","))?;
                a.skipped += names.len();
            }
            Change::Fail { step, error } => {
                let n = JobStepRepo::fail_pending_step_tx(&mut *conn, job_id, step, error).await?;
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
                let n = JobStepRepo::start_placeholder_tx(&mut *conn, job_id, placeholder).await?;
                expect_rows(n, 1, placeholder)?;
                JobStepRepo::create_steps_tx(&mut *conn, instances).await?;
                JobRepo::mark_running_if_pending_tx(&mut *conn, job_id).await?; // zero rows allowed (R7)
                a.expanded += 1;
                i += 1;
            }
            Change::Adopt { placeholder } => {
                let n = JobStepRepo::start_placeholder_tx(&mut *conn, job_id, placeholder).await?;
                expect_rows(n, 1, placeholder)?;
                JobRepo::mark_running_if_pending_tx(&mut *conn, job_id).await?;
                a.adopted += 1;
                i += 1;
            }
            Change::Rollup {
                placeholder,
                outcome,
            } => {
                let n = match outcome {
                    RollupOutcome::Completed(out) => {
                        JobStepRepo::complete_placeholder_tx(&mut *conn, job_id, placeholder, out)
                            .await?
                    }
                    RollupOutcome::Failed(err) => {
                        JobStepRepo::fail_placeholder_tx(&mut *conn, job_id, placeholder, err)
                            .await?
                    }
                };
                expect_rows(n, 1, placeholder)?;
                a.rolled_up += 1;
                i += 1;
            }
        }
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
#[tracing::instrument(skip(pool, task, workspace_config))]
pub async fn execute(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<Plan> {
    for attempt in 1..=MAX_ATTEMPTS {
        let job = JobRepo::get(pool, job_id).await?.context("Job not found")?;
        let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
        let plan = run(task, &job, &steps, workspace_config)?;
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
            }
            Err(ApplyError::Db(e)) if is_deadlock(&e) => {
                tx.rollback().await.ok();
                tracing::warn!(job_id = %job_id, attempt, "Cascade deadlock (40P01) — re-running");
            }
            Err(ApplyError::Db(e)) => {
                tx.rollback().await.ok();
                return Err(e);
            }
        }
    }
    bail!(
        "cascade guard miss {} times for job {}",
        MAX_ATTEMPTS,
        job_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
    use stroem_db::{JobRow, JobStepRow};
    use uuid::Uuid;

    // ── builders ─────────────────────────────────────────────────────

    fn job(input: Option<Value>) -> JobRow {
        JobRow {
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
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            input: HashMap::new(),
            continue_on_failure: false,
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

    fn names(plan: &Plan) -> Vec<String> {
        plan.changes
            .iter()
            .map(|c| match c {
                Change::Promote { step } => format!("promote:{step}"),
                Change::Skip { step } => format!("skip:{step}"),
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
                    outcome: RollupOutcome::Failed(_),
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert!(plan.changes.is_empty(), "join must wait for r");
    }

    #[test]
    fn failed_dep_skips_dependents_transitively_in_one_run() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"])), ("c", fs(&["b"]))]);
        let rows = vec![row("a", "failed"), row("b", "pending"), row("c", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["b"], "skipped");
        assert_eq!(s["c"], "skipped");
    }

    #[test]
    fn continue_on_failure_promotes_past_failed_and_cancelled_deps() {
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs_cof(&["a"])),
            ("x", fs(&[])),
            ("y", fs_cof(&["x"])),
        ]);
        let rows = vec![
            row("a", "failed"),
            row("b", "pending"),
            row("x", "cancelled"),
            row("y", "pending"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["b"], "ready");
        assert_eq!(s["y"], "ready");
    }

    #[test]
    fn mixed_skipped_and_failed_deps_without_cof_skips() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let rows = vec![row("a", "skipped"), row("b", "failed"), row("c", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(final_statuses(&plan, &rows)["c"], "skipped");
    }

    #[test]
    fn all_deps_skipped_cascade_skips_even_with_truthy_when() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row("a", "skipped"), row_when("b", "pending", "true")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["skip:b"]);
    }

    #[test]
    fn all_deps_skipped_applies_without_workspace_config() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row("a", "skipped"), row("b", "pending")];
        let plan = run(&t, &job(None), &rows, None).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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

    #[test]
    fn when_without_workspace_config_stays_pending() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row("a", "completed"), row_when("b", "pending", "true")];
        let plan = run(&t, &job(None), &rows, None).unwrap();
        assert!(plan.changes.is_empty());
    }

    #[test]
    fn skip_unreachable_ignores_skipped_dep_and_cof() {
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs(&[])),
            ("c", fs(&["a", "b"])),
            ("d", fs_cof(&["a"])),
        ]);
        // a failed, b still running: c has a failed dep → R3 skips it; d has cof → stays.
        let rows = vec![
            row("a", "failed"),
            row("b", "running"),
            row("c", "pending"),
            row("d", "pending"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["c"], "skipped");
        assert_eq!(
            s["d"], "ready",
            "cof dependent of a failed dep is promoted by R2"
        );
    }

    #[test]
    fn rows_absent_from_flow_are_ignored() {
        let t = task(vec![("a", fs(&[]))]);
        let rows = vec![row("a", "completed"), row("ghost", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
            ("p_cof", fs_cof(&["dep_failed"])),
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
            placeholder("p_cof", "pending", "[7]"),
            placeholder("ghost", "pending", "[1]"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, None).unwrap();
        assert!(plan.changes.is_empty(), "R4 needs a config even to retire");
    }

    #[test]
    fn expand_builds_instances_parallel_and_sequential() {
        let t = task(vec![("par", fs(&[])), ("seq", fs_seq(&[]))]);
        let rows = vec![
            placeholder("par", "pending", "[\"a\",\"b\"]"),
            placeholder("seq", "pending", "[\"a\",\"b\"]"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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

    #[test]
    fn expand_renders_tera_string_and_literal_array() {
        let t = task(vec![("a", fs(&[])), ("p", fs(&["a"]))]);
        let rows = vec![
            row_out("a", json!({"items": [1, 2, 3]})),
            placeholder("p", "pending", "{{ a.output.items | json_encode() }}"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, None).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
                ..fs_seq(&[])
            },
        )]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "failed", None),
            instance("x", 1, "cancelled", None),
            instance("x", 2, "pending", None),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
            let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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

    #[test]
    fn rollup_failed_text_and_cof() {
        let t = task(vec![("x", fs(&[])), ("y", fs_cof(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "completed", None),
            instance("x", 1, "failed", None),
            instance("x", 2, "failed", None),
            placeholder("y", "running", "[1]"),
            instance("y", 0, "failed", None),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        let mut fails = vec![];
        let mut oks = vec![];
        for c in &plan.changes {
            match c {
                Change::Rollup {
                    placeholder,
                    outcome: RollupOutcome::Failed(e),
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
            [(
                "x".to_string(),
                "for_each loop failed: instances [1, 2] failed".to_string()
            )]
        );
        assert_eq!(oks, ["y"], "cof loop completes even with a failed instance");
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert!(plan.changes.is_empty());
    }

    #[test]
    fn rollup_works_without_workspace_config() {
        let t = task(vec![("x", fs(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1]"),
            instance("x", 0, "completed", Some(json!(1))),
        ];
        let plan = run(&t, &job(None), &rows, None).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["rollup-ok:x", "promote:after"]);
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["promote:s0"], "only the first is promotable");
        let mut snap = rows.clone();
        snap[1].status = "ready".to_string();
        let again = run(&t, &job(None), &snap, Some(&ws())).unwrap();
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();

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
        let again = run(&t, &job(None), &snap, Some(&ws())).unwrap();
        assert!(again.changes.is_empty());
    }

    #[test]
    fn job_input_and_secret_reach_when_templates() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![
            row("a", "completed"),
            row_when("b", "pending", "{{ input.fast }}"),
        ];
        let plan = run(&t, &job(Some(json!({"fast": true}))), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
        let plan = run(&t, &job(Some(json!({"fast": false}))), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["skip:b"]);

        // `secret` only reaches the context when the workspace actually has one:
        // build_step_render_context omits the key for an empty secrets map.
        let mut with_secret = ws();
        with_secret.secrets.insert("API_KEY".into(), json!("k"));
        let rows = vec![
            row("a", "completed"),
            row_when("b", "pending", "{{ secret.API_KEY == \"k\" }}"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&with_secret)).unwrap();
        assert_eq!(
            names(&plan),
            ["promote:b"],
            "secret reached the when template"
        );
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(
            names(&plan),
            ["fail:b"],
            "without the secret in the config the key is undefined"
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
        )
        .unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["b"], "ready");
        assert_eq!(s["c"], "skipped");
        assert_eq!(s["d"], "pending", "d waits for b");
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must evaluate to a JSON array"),
            "Error should mention 'must evaluate to a JSON array'"
        );
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
    fn test_for_each_object_rendering_suggests_json_encode() {
        // Tera renders objects as "[object]" — the error message should
        // suggest using the json_encode() filter.
        let ctx = json!({"step1": {"output": [{"a": 1}, {"a": 2}]}});
        let result = render_for_each_template("{{ step1.output }}", &ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("[object]"),
            "Error should contain '[object]': {}",
            msg
        );
        assert!(
            msg.contains("json_encode()"),
            "Error should suggest json_encode(): {}",
            msg
        );
    }

    #[test]
    fn test_for_each_non_json_error_without_object_hint() {
        // When the rendered output is non-JSON but not the [object] pattern,
        // the error should not include the json_encode hint.
        let ctx = json!({"step1": {"output": "hello"}});
        let result = render_for_each_template("{{ step1.output }}", &ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("non-JSON"),
            "Error should mention non-JSON: {}",
            msg
        );
        assert!(
            !msg.contains("json_encode"),
            "Error should NOT suggest json_encode for plain strings: {}",
            msg
        );
    }

    #[test]
    fn test_for_each_valid_json_non_array_errors() {
        // A template that renders to valid JSON but not an array should
        // produce a clear "must evaluate to a JSON array" error.
        let ctx = json!({"count": 5});
        let result = render_for_each_template("{{ count }}", &ctx);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("must evaluate to a JSON array"),
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
}
