//! Pins the spec §9/§11 migration-behaviour guarantees of
//! `docs/superpowers/specs/2026-10-01-dependency-conditions-design.md`:
//! the "structural catch" pattern (a downstream step's own
//! `continue_on_failure` excusing an upstream failure several hops away,
//! because every path from the failure reaches that flagged step) is
//! retired — only a failing step's OWN flag excuses it now. A chain that
//! used to complete the job under the old design must now fail it, both
//! for a freshly-created job and for a job that was already mid-flight
//! (seeded with some steps already terminal, as if it started running
//! under the old code before the upgrade) when the new cascade/settlement
//! code takes over.
//!
//! Harness borrowed from `dependency_conditions_recalc_pipeline_test.rs`
//! (Task 8): drives the real `cascade::run` to a fixpoint with no real
//! worker, resolving every `ready` row to a terminal status, then checks
//! the real `settlement::settle::decide`.

use serde_json::json;
use std::collections::HashMap;
use stroem_common::depends_on::{AcceptSet, DependsOnEntry, Outcome, StepEntry};
use stroem_common::models::job::JobStatus;
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_db::{JobRow, JobStepRow};
use stroem_server::cascade::{self, Change, Plan};
use stroem_server::render_context::Snapshots;
use stroem_server::settlement::settle;
use uuid::Uuid;

// ─── depends_on entry builders ──────────────────────────────────────────────

/// A bare-name required dependency — sugar for `accept: [completed]`.
fn req(name: &str) -> DependsOnEntry {
    DependsOnEntry::Name(name.to_string())
}

// ─── FlowStep / TaskDef builders ────────────────────────────────────────────

fn flow_step(depends_on: Vec<DependsOnEntry>, continue_on_failure: bool) -> FlowStep {
    FlowStep {
        action: "noop".to_string(),
        name: None,
        description: None,
        depends_on,
        input: HashMap::new(),
        continue_on_failure,
        legacy_continue_when_skipped: None,
        timeout: None,
        when: None,
        for_each: None,
        sequential: false,
        retry: None,
        inline_action: None,
    }
}

/// No `depends_on` flags of its own beyond the bare-name (accept:
/// completed-only) edges to `deps`, and no `continue_on_failure`.
fn fs_no_flags(deps: &[&str]) -> FlowStep {
    flow_step(deps.iter().map(|d| req(d)).collect(), false)
}

/// One explicit-accept edge per `(dependency, accepted outcomes)` pair —
/// the "optional-equivalent" edge shape spec §9 describes for a step that
/// must still run regardless of an upstream failure/omission.
fn fs_with_accept(deps: &[(&str, &[Outcome])]) -> FlowStep {
    flow_step(
        deps.iter()
            .map(|(name, accept)| {
                DependsOnEntry::Step(StepEntry {
                    step: name.to_string(),
                    accept: AcceptSet::Outcomes(accept.to_vec()),
                })
            })
            .collect(),
        false,
    )
}

fn task_with_flow(flow: Vec<(&str, FlowStep)>) -> TaskDef {
    TaskDef {
        name: Some("migration-chain".to_string()),
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

// ─── Harness: drive cascade::run to a fixpoint with no real worker ─────────
// (same shape as dependency_conditions_recalc_pipeline_test.rs — duplicated
// per test file, per this crate's convention; see CLAUDE.md § Settlement.)

fn workspace_with(task: &TaskDef) -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::new();
    ws.tasks.insert(
        task.name
            .clone()
            .expect("task_with_flow always sets a name"),
        task.clone(),
    );
    ws
}

fn seed_all_pending(task: &TaskDef, job_id: Uuid) -> Vec<JobStepRow> {
    task.flow
        .keys()
        .map(|name| JobStepRow::test_default(job_id, name))
        .collect()
}

fn apply_plan(rows: &mut [JobStepRow], plan: &Plan) {
    for change in &plan.changes {
        match change {
            Change::Promote { step } => {
                let r = rows
                    .iter_mut()
                    .find(|r| &r.step_name == step)
                    .unwrap_or_else(|| panic!("promoted step '{step}' not in row set"));
                r.status = "ready".to_string();
            }
            Change::Skip { step, reason } => {
                let r = rows
                    .iter_mut()
                    .find(|r| &r.step_name == step)
                    .unwrap_or_else(|| panic!("skipped step '{step}' not in row set"));
                r.status = "skipped".to_string();
                r.skip_reason = Some(reason.as_str().to_string());
            }
            Change::Fail { step, error } => {
                let r = rows
                    .iter_mut()
                    .find(|r| &r.step_name == step)
                    .unwrap_or_else(|| panic!("failed step '{step}' not in row set"));
                r.status = "failed".to_string();
                r.error_message = Some(error.clone());
            }
            other => panic!("this fixture has no for_each steps — unexpected change: {other:?}"),
        }
    }
}

/// Resolves every `ready` row to a terminal status: the one named in
/// `overrides` for that step, or `completed` by default.
fn resolve_ready(rows: &mut [JobStepRow], overrides: &HashMap<&str, &str>) -> bool {
    let mut any = false;
    for r in rows.iter_mut() {
        if r.status != "ready" {
            continue;
        }
        any = true;
        match overrides
            .get(r.step_name.as_str())
            .copied()
            .unwrap_or("completed")
        {
            "completed" => {
                r.status = "completed".to_string();
                r.output = Some(json!({}));
            }
            "failed" => {
                r.status = "failed".to_string();
                r.error_message = Some(format!("simulated failure: {}", r.step_name));
            }
            other => panic!(
                "unsupported forced outcome '{other}' for step '{}'",
                r.step_name
            ),
        }
    }
    any
}

/// Drives the real cascade (`cascade::run`) to a fixpoint from a given
/// starting row set, resolving each newly-`ready` step per `overrides` (or
/// `completed` by default) until nothing changes.
fn run_to_terminal_from(
    task: &TaskDef,
    job: &JobRow,
    mut rows: Vec<JobStepRow>,
    overrides: &HashMap<&str, &str>,
) -> Vec<JobStepRow> {
    let ws = workspace_with(task);
    let snapshots = Snapshots::default();

    const MAX_TICKS: usize = 50;
    for _ in 0..MAX_TICKS {
        let plan = cascade::run(task, job, &rows, Some(&ws), &snapshots)
            .expect("cascade::run must not error for this fixture");
        let changed = !plan.changes.is_empty();
        apply_plan(&mut rows, &plan);
        let resolved = resolve_ready(&mut rows, overrides);
        if !changed && !resolved {
            return rows;
        }
    }
    panic!("did not reach a fixpoint within {MAX_TICKS} ticks: {rows:?}");
}

fn run_to_terminal(task: &TaskDef, overrides: &HashMap<&str, &str>) -> Vec<JobStepRow> {
    let job = JobRow::test_default();
    let rows = seed_all_pending(task, job.job_id);
    run_to_terminal_from(task, &job, rows, overrides)
}

fn status_of<'a>(rows: &'a [JobStepRow], name: &str) -> &'a str {
    rows.iter()
        .find(|r| r.step_name == name)
        .unwrap_or_else(|| panic!("no such step '{name}' in row set"))
        .status
        .as_str()
}

fn promoted_or_completed(rows: &[JobStepRow], name: &str) -> bool {
    matches!(status_of(rows, name), "ready" | "completed")
}

/// Builds a row with an explicit status, otherwise matching
/// `JobStepRow::test_default`. Used to seed a "mid-flight" job whose steps
/// are already partway through their lifecycle, rather than all `pending`.
fn row_with(job_id: Uuid, name: &str, status: &str) -> JobStepRow {
    JobStepRow {
        status: status.to_string(),
        ..JobStepRow::test_default(job_id, name)
    }
}

fn row_skipped_with(job_id: Uuid, name: &str, reason: &str) -> JobStepRow {
    JobStepRow {
        skip_reason: Some(reason.to_string()),
        ..row_with(job_id, name, "skipped")
    }
}

// ─── The scenario: A -> B -> C(flagged) -> D ───────────────────────────────

/// Spec §9's migration scenario. `a` and `b` carry no flags of their own;
/// `c` has `continue_on_failure: true`; `d`'s edge to `c` accepts
/// `completed`/`failed`/`omitted` (the "optional-equivalent" shape), so `d`
/// still runs however `c` lands. Under 0.17.0's retired "structural catch"
/// rule, `c`'s flag used to excuse `a`'s failure too (every path from `a`
/// reaches `c`), completing the job. Under the new rule, only `a`'s own
/// `continue_on_failure` would excuse `a` — it has none, so the job must
/// fail even though `d` still ran.
fn migration_chain() -> TaskDef {
    task_with_flow(vec![
        ("a", fs_no_flags(&[])),
        ("b", fs_no_flags(&["a"])),
        (
            "c",
            FlowStep {
                continue_on_failure: true,
                ..fs_no_flags(&["b"])
            },
        ),
        (
            "d",
            fs_with_accept(&[(
                "c",
                &[Outcome::Completed, Outcome::Failed, Outcome::Omitted],
            )]),
        ),
    ])
}

#[test]
fn structural_catch_chain_now_fails_the_job_fresh() {
    let task = migration_chain();
    let overrides = HashMap::from([("a", "failed")]);
    let rows = run_to_terminal(&task, &overrides);

    assert_eq!(status_of(&rows, "a"), "failed");
    assert_eq!(
        status_of(&rows, "b"),
        "skipped",
        "b requires a (completed-only): {rows:?}"
    );
    assert_eq!(
        status_of(&rows, "c"),
        "skipped",
        "c requires b (completed-only) — c's own continue_on_failure never \
         makes c itself run when c's dependency blocks it: {rows:?}"
    );
    assert!(
        promoted_or_completed(&rows, "d"),
        "d must still run — its edge to c accepts c's omitted outcome: {rows:?}"
    );
    assert_eq!(status_of(&rows, "d"), "completed");

    let settled = settle::decide(&task, &rows).expect("every row is terminal");
    assert_eq!(
        settled.status,
        JobStatus::Failed,
        "a has no flag of its own — the job must fail, matching spec §9's \
         documented, accepted change: {rows:?}"
    );
}

#[test]
fn structural_catch_chain_mid_flight_also_now_fails_the_job() {
    // Same flow, but seeded with b and c already terminal (as if this job
    // started running under the old design before the upgrade) and only d
    // still pending at the moment the new code takes over. Pins that
    // in-flight jobs really do see the new rule on their next cascade
    // (spec §11), not just freshly created ones.
    let task = migration_chain();
    let job = JobRow::test_default();
    let rows = vec![
        row_with(job.job_id, "a", "failed"),
        row_skipped_with(job.job_id, "b", "unreachable"), // already decided pre-upgrade
        row_skipped_with(job.job_id, "c", "unreachable"),
        row_with(job.job_id, "d", "pending"),
    ];

    let rows = run_to_terminal_from(&task, &job, rows, &HashMap::new());

    assert!(
        promoted_or_completed(&rows, "d"),
        "d must still run from the mid-flight seed too: {rows:?}"
    );
    assert_eq!(status_of(&rows, "d"), "completed");

    let settled = settle::decide(&task, &rows).expect("every row is terminal");
    assert_eq!(
        settled.status,
        JobStatus::Failed,
        "a's failure (already on the row at seed time) is still untolerated: {rows:?}"
    );
}
