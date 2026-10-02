//! Per-job redaction (spec 2026-10-02 git refs § 7.4, § 9).
//!
//! Every API outlet that returns a job's input/output, step output or
//! `error_message` masks with the live workspaces' values PLUS the secret
//! values of every pin referenced by the job's **redaction closure**: the
//! whole job tree of every job in its source lineage (the job, and the hook,
//! restart, re-run and task-retry sources it was made from), see
//! [`closure_pins`]. Content is copied between those jobs:
//! - a child's output settles into its parent step;
//! - a parent's or sibling's values render into a child's input;
//! - a hook payload quotes its source's step errors;
//! - a restart carries its source's step output;
//! - a re-run replays its source's `raw_input` (a task retry's is the failed
//!   job's resolved input);
//! - a task retry replays its source's input.
//!
//! So a job's own pins alone would let a copied value through. A pinned
//! commit's sops values (or how its vals references render) can differ from
//! the live config, so the live set alone would let an older or ref-only
//! secret through.
//!
//! A pin that cannot be loaded, a closure that cannot be read, or a closure
//! cut by one of its bounds makes the set incomplete, and an incomplete set
//! is never used. A TRANSIENT failure fails closed ([`RedactionUnavailable`]
//! → 503 / an MCP error, retry later). A PERMANENT one (a pin that can never
//! load, a truncated closure) can never be retried away, so the outlet answers
//! with every content string masked ([`JobRedaction::MaskAll`]).

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;
use stroem_db::{ClosureBounds, ClosurePinRow, JobRepo, JobRow, JobStepRow, RedactionClosure};

use crate::state::AppState;
use crate::workspace::pins::{PinError, PinLoadWithheld, PinRef};
use crate::workspace_set::{
    collect_redaction_values, redact_secrets_in_str, WorkspaceSet, REDACTED,
};

/// Why a job's redaction set is incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableCause {
    /// A pin of the closure could not be loaded.
    Pin { commit: String },
    /// The closure (or the short-circuit probe) could not be read: a DB
    /// error.
    ClosureUnreadable,
    /// A bound refused an edge of the closure, or it holds more than
    /// [`MAX_REDACTION_CLOSURE_JOBS`] jobs: pins beyond the bound are unknown.
    ClosureTruncated,
}

/// The job's redaction set is incomplete: a pin of its closure could not be
/// loaded, or the closure itself could not be read or was cut by a bound.
/// Some secret values are unknown, so callers must not answer with a
/// partial redaction set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionUnavailable {
    pub workspace: String,
    pub cause: UnavailableCause,
    /// `true` = a retry may succeed (answer 503); `false` = it never will
    /// (mask everything). For a pin, [`PinError::is_transient`].
    pub transient: bool,
}

impl RedactionUnavailable {
    pub fn from_pin_error(ws: &str, pin: &PinRef, e: &PinError) -> Self {
        RedactionUnavailable {
            workspace: ws.to_string(),
            cause: UnavailableCause::Pin {
                commit: pin.commit.clone(),
            },
            transient: e.is_transient(),
        }
    }

    /// The closure of a job in `ws` could not be read. Transient: a retry
    /// may read it.
    pub fn closure_unreadable(ws: &str) -> Self {
        RedactionUnavailable {
            workspace: ws.to_string(),
            cause: UnavailableCause::ClosureUnreadable,
            transient: true,
        }
    }

    /// The closure of a job in `ws` was cut by a bound. Permanent: a retry
    /// walks the same rows into the same bound.
    pub fn closure_truncated(ws: &str) -> Self {
        RedactionUnavailable {
            workspace: ws.to_string(),
            cause: UnavailableCause::ClosureTruncated,
            transient: false,
        }
    }
}

impl std::fmt::Display for RedactionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = if self.transient {
            "transient"
        } else {
            "permanent"
        };
        let ws = &self.workspace;
        match &self.cause {
            UnavailableCause::Pin { commit } => write!(
                f,
                "redaction set unavailable: pin {ws}@{commit} could not be loaded ({kind})"
            ),
            UnavailableCause::ClosureUnreadable => write!(
                f,
                "redaction set unavailable: the redaction closure of a job in {ws} could not be read ({kind})"
            ),
            UnavailableCause::ClosureTruncated => write!(
                f,
                "redaction set unavailable: the redaction closure of a job in {ws} exceeds its bounds ({kind})"
            ),
        }
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

/// [`referenced_pins`] of the job (`own`, in order), then each closure row
/// whose `(workspace, commit)` is not listed yet. Pure.
pub fn merge_pins(
    own: Vec<(String, PinRef)>,
    closure: Vec<ClosurePinRow>,
) -> Vec<(String, PinRef)> {
    let mut seen: BTreeSet<(String, String)> = own
        .iter()
        .map(|(ws, pin)| (ws.clone(), pin.commit.clone()))
        .collect();
    let mut out = own;
    for row in closure {
        if seen.insert((row.workspace.clone(), row.revision.clone())) {
            out.push((
                row.workspace,
                PinRef {
                    git_ref: row.git_ref,
                    commit: row.revision,
                },
            ));
        }
    }
    out
}

/// Restart and re-run links (`source_job_id`), counted together, that the
/// redaction closure follows before it gives up (fail closed). Nothing else
/// caps a chain of them: each one is a user action on a top-level job.
pub const MAX_SOURCE_LINEAGE_HOPS: i32 = 32;

/// Task-retry links the redaction closure follows before it gives up (fail
/// closed). A retry points at the root original (`retry_of_job_id`), so a
/// chain needs more than one retry hop only when restarts or re-runs and
/// retries interleave, and the source-lineage cap bounds that.
pub const MAX_RETRY_LINEAGE_HOPS: i32 = MAX_SOURCE_LINEAGE_HOPS + 1;

/// Jobs a redaction closure may hold before it gives up (fail closed). It also
/// bounds the walk's work on every outlet call.
pub const MAX_REDACTION_CLOSURE_JOBS: i64 = 20_000;

/// The bounds of every redaction closure:
/// - the server's task-nesting cap (`job_creator::MAX_TASK_DEPTH`);
/// - the hook-chain cap (`settlement::hooks::MAX_HOOK_CHAIN_DEPTH`);
/// - [`MAX_SOURCE_LINEAGE_HOPS`], [`MAX_RETRY_LINEAGE_HOPS`] and
///   [`MAX_REDACTION_CLOSURE_JOBS`].
///
/// Hitting any of them makes the closure TRUNCATED, and the outlet masks
/// everything ([`JobRedaction::MaskAll`]); nothing is cut silently. That is
/// reachable: restart and re-run chains are otherwise unbounded, agent task-tool
/// children skip the `MAX_TASK_DEPTH` check, and the hook-chain walk fails
/// open.
pub const CLOSURE_BOUNDS: ClosureBounds = ClosureBounds {
    task_depth: crate::job_creator::MAX_TASK_DEPTH as i32,
    hook_hops: crate::settlement::hooks::MAX_HOOK_CHAIN_DEPTH as i32,
    source_hops: MAX_SOURCE_LINEAGE_HOPS,
    retry_hops: MAX_RETRY_LINEAGE_HOPS,
    max_jobs: MAX_REDACTION_CLOSURE_JOBS,
};

/// Per-request memo for an outlet that redacts several jobs (worker detail).
/// Each piece is computed at most once per request:
/// - the global short-circuit probe;
/// - each job's closure, by job id;
/// - each pin's values, by `(workspace, commit)`, failures included, so a
///   failing pin is not retried for every job.
///
/// A one-job outlet uses a fresh memo.
#[derive(Default)]
pub struct RedactionMemo {
    any_pinned: Option<bool>,
    closures: HashMap<uuid::Uuid, RedactionClosure>,
    pin_values: HashMap<(String, String), Result<Vec<String>, RedactionUnavailable>>,
}

// Never prints the pins' secret values: counts only (CLAUDE.md § Secrets in
// logs, like `Pinned`).
impl std::fmt::Debug for RedactionMemo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded = self.pin_values.values().filter(|v| v.is_ok()).count();
        f.debug_struct("RedactionMemo")
            .field("any_pinned", &self.any_pinned)
            .field("closures", &self.closures.len())
            .field(
                "pin_values",
                &format_args!(
                    "[REDACTED; {loaded} loaded, {} failed]",
                    self.pin_values.len() - loaded
                ),
            )
            .finish()
    }
}

/// Every distinct pin of the job's **redaction closure** (spec § 7.4): the
/// job's own [`referenced_pins`] (from the rows the outlet is about to show),
/// plus the pins of every job whose content can be copied into it. That is
/// the whole job tree (root and all descendants) of every job in the job's
/// source lineage: the job itself and, following parents up and hook,
/// restart, re-run and task-retry sources back, every job it was made from.
/// Bounded by [`CLOSURE_BOUNDS`], fail closed.
///
/// **Short-circuit.** When no row anywhere references a pin, every closure's
/// pin set is empty, so the job's own (empty) pins are the answer and the
/// closure walk is skipped. Truncation cannot hide a pin that does not exist.
/// This holds because the probe runs AFTER the outlet read the rows it will
/// show: a pinned row whose values could be in them existed by then.
#[tracing::instrument(skip_all, fields(job_id = %job.job_id))]
pub async fn closure_pins(
    state: &AppState,
    job: &JobRow,
    steps: &[JobStepRow],
    memo: &mut RedactionMemo,
) -> Result<Vec<(String, PinRef)>, RedactionUnavailable> {
    let own = referenced_pins(job, steps);
    let unreadable = |e: anyhow::Error| {
        // A DB error: no config text, safe to log whole.
        tracing::warn!(
            job_id = %job.job_id,
            "redaction set unavailable: redaction closure unreadable: {e:#}"
        );
        RedactionUnavailable::closure_unreadable(&job.workspace)
    };
    let any_pinned = match memo.any_pinned {
        Some(any) => any,
        None => {
            let any = JobRepo::any_pinned_rows(&state.pool)
                .await
                .map_err(unreadable)?;
            memo.any_pinned = Some(any);
            any
        }
    };
    if !any_pinned {
        return Ok(own);
    }
    let closure = match memo.closures.get(&job.job_id) {
        Some(closure) => closure.clone(),
        None => {
            let closure = JobRepo::redaction_closure_pins(&state.pool, job.job_id, CLOSURE_BOUNDS)
                .await
                .map_err(unreadable)?;
            memo.closures.insert(job.job_id, closure.clone());
            closure
        }
    };
    match closure {
        RedactionClosure::Pins(rows) => Ok(merge_pins(own, rows)),
        RedactionClosure::Truncated => {
            tracing::warn!(
                job_id = %job.job_id,
                "redaction set unavailable: the redaction closure exceeds its bounds; masking everything"
            );
            Err(RedactionUnavailable::closure_truncated(&job.workspace))
        }
    }
}

/// The live redaction set plus the secret values of every pin of the job's
/// redaction closure ([`closure_pins`], spec § 7.4). `Err` = the closure
/// could not be read (transient) or was truncated (permanent), or some pin
/// could not be loaded (the first failing pin decides the error's kind).
/// Outlets normally go through [`job_redaction`], which turns a permanent
/// failure into [`JobRedaction::MaskAll`].
pub async fn job_redaction_values(
    state: &AppState,
    job: &JobRow,
    steps: &[JobStepRow],
) -> Result<Vec<String>, RedactionUnavailable> {
    job_redaction_values_memo(state, job, steps, &mut RedactionMemo::default()).await
}

/// [`job_redaction_values`] with a caller-held [`RedactionMemo`].
#[tracing::instrument(skip_all, fields(job_id = %job.job_id, workspace = %job.workspace))]
pub async fn job_redaction_values_memo(
    state: &AppState,
    job: &JobRow,
    steps: &[JobStepRow],
    memo: &mut RedactionMemo,
) -> Result<Vec<String>, RedactionUnavailable> {
    let set = WorkspaceSet::load(&state.workspaces, &job.workspace, None).await;
    let mut values = collect_redaction_values(&set);
    for (ws, pin) in closure_pins(state, job, steps, memo).await? {
        let key = (ws.clone(), pin.commit.clone());
        let pin_values = match memo.pin_values.get(&key) {
            Some(cached) => cached.clone(),
            None => {
                let loaded = match state.workspaces.pins().ensure(&ws, &pin.commit).await {
                    // The pin's complete set (R5): its secrets AND the
                    // `secret: true` properties of its connections,
                    // whichever workspace types them.
                    Ok(pinned) => Ok(state.workspaces.pin_redaction_values(&ws, &pinned).await),
                    Err(e) => {
                        let unavailable = RedactionUnavailable::from_pin_error(&ws, &pin, &e);
                        tracing::warn!(
                            job_id = %job.job_id,
                            workspace = %ws,
                            commit = %pin.commit,
                            transient = unavailable.transient,
                            "redaction set unavailable: {}",
                            pin_failure_log_text(&ws, &pin, &e)
                        );
                        Err(unavailable)
                    }
                };
                memo.pin_values.insert(key, loaded.clone());
                loaded
            }
        };
        values.extend(pin_values?);
    }
    Ok(values)
}

/// How an outlet treats a job's content once its pins are known.
#[derive(Clone, PartialEq, Eq)]
pub enum JobRedaction {
    /// Every referenced pin loaded: mask these values wherever they occur.
    Values(Vec<String>),
    /// A referenced pin can never load (permanent [`PinError`]), or the
    /// closure was cut by a bound: some secret values are unknowable for
    /// good, so every content string is masked whole. Identifiers, statuses
    /// and timestamps stay readable.
    MaskAll,
}

// Never prints the values: a count only (CLAUDE.md § Secrets in logs).
impl std::fmt::Debug for JobRedaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobRedaction::Values(values) => {
                write!(f, "Values([REDACTED; {}])", values.len())
            }
            JobRedaction::MaskAll => f.write_str("MaskAll"),
        }
    }
}

impl JobRedaction {
    /// The outlet split: a set → [`Values`](Self::Values); a PERMANENT pin
    /// failure → [`MaskAll`](Self::MaskAll) (a retry would fail the same
    /// way); a TRANSIENT one stays `Err` — the outlet answers 503 / an MCP
    /// error "redaction set unavailable, retry".
    pub fn from_result(
        result: Result<Vec<String>, RedactionUnavailable>,
    ) -> Result<JobRedaction, RedactionUnavailable> {
        match result {
            Ok(values) => Ok(JobRedaction::Values(values)),
            Err(e) if e.transient => Err(e),
            Err(_) => Ok(JobRedaction::MaskAll),
        }
    }

    pub fn masks_all(&self) -> bool {
        matches!(self, JobRedaction::MaskAll)
    }

    pub fn apply_str(&self, s: &str) -> String {
        match self {
            JobRedaction::Values(secrets) => redact_str(s, secrets),
            JobRedaction::MaskAll => REDACTED.to_string(),
        }
    }

    pub fn apply_value(&self, v: &mut Value) {
        match self {
            JobRedaction::Values(secrets) => redact_value_tree(v, secrets),
            JobRedaction::MaskAll => mask_value_tree(v),
        }
    }

    /// [`redact_job_response`] or [`mask_job_response`].
    pub fn apply_job_response(&self, job: &mut Value) {
        match self {
            JobRedaction::Values(secrets) => redact_job_response(job, secrets),
            JobRedaction::MaskAll => mask_job_response(job),
        }
    }
}

/// [`job_redaction_values`] through [`JobRedaction::from_result`]: `Err` only
/// for a TRANSIENT failure.
pub async fn job_redaction(
    state: &AppState,
    job: &JobRow,
    steps: &[JobStepRow],
) -> Result<JobRedaction, RedactionUnavailable> {
    job_redaction_memo(state, job, steps, &mut RedactionMemo::default()).await
}

/// [`job_redaction`] with a caller-held [`RedactionMemo`] (worker detail).
#[tracing::instrument(skip_all, fields(job_id = %job.job_id, workspace = %job.workspace))]
pub async fn job_redaction_memo(
    state: &AppState,
    job: &JobRow,
    steps: &[JobStepRow],
    memo: &mut RedactionMemo,
) -> Result<JobRedaction, RedactionUnavailable> {
    JobRedaction::from_result(job_redaction_values_memo(state, job, steps, memo).await)
}

/// Redact a job's `output` (webhook responses). Loads the job's steps for
/// their pins and applies the same split as job detail: a permanent pin
/// failure masks every string of the output (`Ok`); only a TRANSIENT one is
/// an error — a [`RedactionUnavailable`] with `transient: true` inside the
/// `anyhow` error, which callers `downcast_ref` to answer 503.
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
    job_redaction(state, job, &steps)
        .await?
        .apply_value(&mut output);
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
    map_strings(value, &|s| redact_str(s, secrets));
}

/// Replace every string of a JSON tree with the mask; numbers, booleans and
/// nulls are kept.
pub fn mask_value_tree(value: &mut Value) {
    map_strings(value, &|_| REDACTED.to_string());
}

fn map_strings(value: &mut Value, f: &dyn Fn(&str) -> String) {
    match value {
        Value::String(s) => *s = f(s),
        Value::Object(map) => {
            for v in map.values_mut() {
                map_strings(v, f);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                map_strings(v, f);
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
    walk_job_content(job, &|v| redact_value_tree(v, secrets));
}

/// [`redact_job_response`]'s walk with every content string masked whole —
/// the answer when a referenced pin is permanently unloadable. Identifiers,
/// statuses and timestamps stay intact.
pub fn mask_job_response(job: &mut Value) {
    walk_job_content(job, &mask_value_tree);
}

/// Apply `f` to every content value of a serialised job: each top-level value
/// but the identifier keys, and each step entry's value but the step
/// identifier keys. A non-array `steps` is content too.
fn walk_job_content(job: &mut Value, f: &dyn Fn(&mut Value)) {
    let Value::Object(map) = job else {
        f(job);
        return;
    };
    for (key, v) in map.iter_mut() {
        if key == "steps" {
            match v {
                Value::Array(steps) => {
                    for step in steps.iter_mut() {
                        walk_object_except(step, STEP_IDENTIFIER_KEYS, f);
                    }
                }
                other => f(other),
            }
        } else if !JOB_IDENTIFIER_KEYS.contains(&key.as_str()) {
            f(v);
        }
    }
}

fn walk_object_except(v: &mut Value, skip: &[&str], f: &dyn Fn(&mut Value)) {
    match v {
        Value::Object(map) => {
            for (k, x) in map.iter_mut() {
                if !skip.contains(&k.as_str()) {
                    f(x);
                }
            }
        }
        other => f(other),
    }
}

/// The log text for a pin failure. A `PinLoadFailed` carries the raw loader
/// chain, which can quote secret values — the very values this pin's set
/// would mask, unknowable here — so it is logged as the fixed
/// [`PinLoadWithheld`] sentence. Every other variant carries no config text.
fn pin_failure_log_text(ws: &str, pin: &PinRef, e: &PinError) -> String {
    match e {
        PinError::PinLoadFailed { .. } => PinLoadWithheld::new(ws, pin).to_string(),
        other => other.to_string(),
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

    /// Final review M3 (CLAUDE.md § Secrets in logs): neither the per-request
    /// memo nor a redaction set prints the secret values it holds.
    #[test]
    fn redaction_debug_never_prints_secret_values() {
        const SECRET: &str = "m3-plaintext-secret-value";
        let values = JobRedaction::Values(vec![SECRET.to_string()]);
        let debug = format!("{values:?}");
        assert!(!debug.contains(SECRET), "{debug}");
        assert!(debug.contains("Values"), "{debug}");
        assert!(
            !format!("{:?}", JobRedaction::MaskAll).is_empty(),
            "MaskAll still prints"
        );

        let mut memo = RedactionMemo::default();
        memo.pin_values
            .insert(("etl".into(), SHA_A.into()), Ok(vec![SECRET.to_string()]));
        memo.any_pinned = Some(true);
        let debug = format!("{memo:?}");
        assert!(!debug.contains(SECRET), "{debug}");
        let debug = format!("{memo:#?}");
        assert!(!debug.contains(SECRET), "{debug}");
    }

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

    // ── permanent vs transient pin failures (fix round 1) ──────────────

    fn pin_23() -> PinRef {
        PinRef {
            git_ref: "release/2.3".to_string(),
            commit: SHA_A.to_string(),
        }
    }

    #[test]
    fn redaction_unavailable_kind_follows_the_pin_error() {
        let transient = PinError::PinUnavailable {
            workspace: "etl".into(),
            message: "network down".into(),
        };
        let permanent = [
            PinError::CommitNotFound {
                workspace: "etl".into(),
                commit: SHA_A.into(),
            },
            PinError::NotGit {
                workspace: "etl".into(),
            },
            PinError::PinLoadFailed {
                workspace: "etl".into(),
                commit: SHA_A.into(),
                message: "bad yaml".into(),
            },
        ];
        let u = RedactionUnavailable::from_pin_error("etl", &pin_23(), &transient);
        assert!(u.transient);
        assert_eq!(u.workspace, "etl");
        assert_eq!(
            u.cause,
            UnavailableCause::Pin {
                commit: SHA_A.to_string()
            }
        );
        for e in &permanent {
            assert!(
                !RedactionUnavailable::from_pin_error("etl", &pin_23(), e).transient,
                "{e:?} is permanent"
            );
        }
    }

    #[test]
    fn pin_failure_log_text_withholds_the_loader_message() {
        let e = PinError::PinLoadFailed {
            workspace: "etl".into(),
            commit: SHA_A.into(),
            message: "Variable `secret.token` = s3cr3t-in-chain".into(),
        };
        let text = pin_failure_log_text("etl", &pin_23(), &e);
        assert!(!text.contains("s3cr3t-in-chain"), "{text}");
        assert!(text.contains("etl@release/2.3"), "{text}");

        // Every other variant carries no config text: logged as is.
        let e = PinError::PinUnavailable {
            workspace: "etl".into(),
            message: "connection refused".into(),
        };
        assert!(pin_failure_log_text("etl", &pin_23(), &e).contains("connection refused"));
    }

    /// A DB error reading the closure: the set is unknown, so the outlet
    /// fails closed (503), never masks-all, and the text names no pin.
    #[test]
    fn closure_unreadable_is_transient_and_names_no_pin() {
        let u = RedactionUnavailable::closure_unreadable("etl");
        assert!(u.transient);
        assert_eq!(u.cause, UnavailableCause::ClosureUnreadable);
        assert_eq!(JobRedaction::from_result(Err(u.clone())), Err(u.clone()));
        let text = u.to_string();
        assert!(text.contains("redaction closure"), "{text}");
        assert!(text.contains("etl"), "{text}");
        let pin = RedactionUnavailable::from_pin_error(
            "etl",
            &pin_23(),
            &PinError::NotGit {
                workspace: "etl".into(),
            },
        );
        assert!(pin.to_string().contains(&format!("etl@{SHA_A}")), "{pin}");
    }

    /// A closure cut by a bound: its pins beyond the bound are unknown for
    /// good (a retry walks into the same bound), so the outlet masks
    /// everything rather than answering with a partial set or a 503 loop.
    #[test]
    fn closure_truncated_is_permanent_and_masks_everything() {
        let u = RedactionUnavailable::closure_truncated("etl");
        assert!(!u.transient);
        assert_eq!(u.cause, UnavailableCause::ClosureTruncated);
        assert_eq!(
            JobRedaction::from_result(Err(u.clone())),
            Ok(JobRedaction::MaskAll)
        );
        let text = u.to_string();
        assert!(text.contains("exceeds its bounds"), "{text}");
        assert!(text.contains("permanent"), "{text}");
    }

    /// Every bound of the closure is set, and the depth bound is exactly the
    /// creation cap: the deepest tree `type: task` dispatch allows (a child
    /// with `MAX_TASK_DEPTH` ancestors) fits without truncation.
    #[test]
    fn closure_bounds_match_the_server_caps() {
        assert_eq!(
            CLOSURE_BOUNDS.task_depth,
            crate::job_creator::MAX_TASK_DEPTH as i32
        );
        assert_eq!(
            CLOSURE_BOUNDS.hook_hops,
            crate::settlement::hooks::MAX_HOOK_CHAIN_DEPTH as i32
        );
        assert_eq!(CLOSURE_BOUNDS.source_hops, MAX_SOURCE_LINEAGE_HOPS);
        assert_eq!(CLOSURE_BOUNDS.retry_hops, MAX_SOURCE_LINEAGE_HOPS + 1);
        assert_eq!(CLOSURE_BOUNDS.max_jobs, 20_000);
    }

    #[test]
    fn job_redaction_splits_on_the_error_kind() {
        let values = JobRedaction::from_result(Ok(vec!["s".to_string()]));
        assert_eq!(values, Ok(JobRedaction::Values(vec!["s".to_string()])));

        let permanent = RedactionUnavailable {
            workspace: "etl".into(),
            cause: UnavailableCause::Pin {
                commit: SHA_A.into(),
            },
            transient: false,
        };
        assert_eq!(
            JobRedaction::from_result(Err(permanent)),
            Ok(JobRedaction::MaskAll)
        );

        let transient = RedactionUnavailable {
            workspace: "etl".into(),
            cause: UnavailableCause::Pin {
                commit: SHA_A.into(),
            },
            transient: true,
        };
        assert_eq!(
            JobRedaction::from_result(Err(transient.clone())),
            Err(transient)
        );
    }

    #[test]
    fn job_redaction_mask_all_masks_every_string() {
        let mask = JobRedaction::MaskAll;
        assert!(mask.masks_all());
        assert_eq!(mask.apply_str("exit 1: anything"), REDACTED);

        let mut v = json!({"a": "x", "n": 3, "list": ["y", true, null]});
        mask.apply_value(&mut v);
        assert_eq!(
            v,
            json!({"a": REDACTED, "n": 3, "list": [REDACTED, true, null]})
        );

        let values = JobRedaction::Values(vec!["tok".to_string()]);
        assert!(!values.masks_all());
        assert_eq!(values.apply_str("a tok b"), format!("a {REDACTED} b"));
    }

    #[test]
    fn mask_job_response_masks_content_and_keeps_identifiers() {
        let child = json!({"id": "c1", "workspace": "etl", "task_name": "t",
                           "status": "completed", "created_at": "2026-10-02T00:00:00Z"});
        let mut v = json!({
            "job_id": "j1",
            "workspace": "etl",
            "task_name": "nightly",
            "status": "failed",
            "revision": SHA_A,
            "created_at": "2026-10-02T00:00:00Z",
            "input": {"env": "prod"},
            "output": {"r": "anything"},
            "retry_attempt": 0,
            "steps": [{
                "step_name": "gate",
                "status": "completed",
                "started_at": "2026-10-02T00:00:00Z",
                "approval_message": "approve?",
                "error_message": "exit 1",
                "output": {"approval_message": "approve?"},
                "child_jobs": [child.clone()],
                "retry_attempt": 2
            }]
        });
        mask_job_response(&mut v);
        assert_eq!(v["job_id"], json!("j1"));
        assert_eq!(v["workspace"], json!("etl"));
        assert_eq!(v["task_name"], json!("nightly"));
        assert_eq!(v["status"], json!("failed"));
        assert_eq!(v["revision"], json!(SHA_A));
        assert_eq!(v["created_at"], json!("2026-10-02T00:00:00Z"));
        assert_eq!(v["input"]["env"], json!(REDACTED));
        assert_eq!(v["output"]["r"], json!(REDACTED));
        let step = &v["steps"][0];
        assert_eq!(step["step_name"], json!("gate"));
        assert_eq!(step["status"], json!("completed"));
        assert_eq!(step["started_at"], json!("2026-10-02T00:00:00Z"));
        assert_eq!(step["child_jobs"], json!([child]));
        assert_eq!(step["retry_attempt"], json!(2));
        assert_eq!(step["approval_message"], json!(REDACTED));
        assert_eq!(step["error_message"], json!(REDACTED));
        assert_eq!(step["output"]["approval_message"], json!(REDACTED));
    }

    /// A `steps` value that is not an array is content like any other: it is
    /// walked, never passed through raw.
    #[test]
    fn job_response_walks_a_non_array_steps_value() {
        let mut v = json!({"steps": {"odd": "tok-1"}});
        redact_job_response(&mut v, &["tok-1".to_string()]);
        assert_eq!(v["steps"]["odd"], json!(REDACTED));
        let mut v = json!({"steps": "raw"});
        mask_job_response(&mut v);
        assert_eq!(v["steps"], json!(REDACTED));
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

    // ── merge_pins (redaction closure) ─────────────────────────────────

    fn closure_row(ws: &str, git_ref: &str, commit: &str) -> ClosurePinRow {
        ClosurePinRow {
            workspace: ws.to_string(),
            git_ref: git_ref.to_string(),
            revision: commit.to_string(),
        }
    }

    #[test]
    fn merge_pins_keeps_own_pins_first_and_adds_unseen_closure_pins() {
        let own = vec![("etl".to_string(), pin_23())];
        let closure = vec![
            // Same (workspace, commit) as an own pin, under another ref name.
            closure_row("etl", "refs/heads/release/2.3", SHA_A),
            closure_row("billing", "v4.1.0", SHA_B),
            // Same commit, other workspace: a different pin.
            closure_row("docs", "main", SHA_A),
            closure_row("billing", "v4.1.0", SHA_B),
        ];
        let got: Vec<(String, String, String)> = merge_pins(own, closure)
            .into_iter()
            .map(|(ws, p)| (ws, p.git_ref, p.commit))
            .collect();
        assert_eq!(
            got,
            vec![
                ("etl".into(), "release/2.3".into(), SHA_A.into()),
                ("billing".into(), "v4.1.0".into(), SHA_B.into()),
                ("docs".into(), "main".into(), SHA_A.into()),
            ]
        );
    }

    #[test]
    fn merge_pins_of_an_empty_closure_is_the_jobs_own() {
        assert!(merge_pins(vec![], vec![]).is_empty());
        let own = vec![("etl".to_string(), pin_23())];
        assert_eq!(merge_pins(own.clone(), vec![]), own);
        let only_closure = merge_pins(vec![], vec![closure_row("etl", "main", SHA_B)]);
        assert_eq!(only_closure.len(), 1);
        assert_eq!(only_closure[0].1.commit, SHA_B);
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
