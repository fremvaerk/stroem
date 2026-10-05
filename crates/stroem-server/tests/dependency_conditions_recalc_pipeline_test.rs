//! Exercises spec §3 of
//! `docs/superpowers/specs/2026-10-01-dependency-conditions-design.md`
//! (`recalc-pipeline`, the prod incident that motivated this redesign) end
//! to end: all seven target rules, in one flow, through the real cascade
//! (`cascade::run`) and real settlement decision
//! (`settlement::settle::decide`) — the first test to combine Tasks 1-7.
//!
//! There is no worker in this harness (unlike the DB-backed tests in
//! `orchestrator_test.rs`), so `run_to_terminal` below plays that role: it
//! drives `cascade::run` to a fixpoint and, each time a step reaches
//! `ready`, immediately resolves it to a terminal status — either the one
//! the test forces via `overrides`, or `completed` by default.

use serde_json::json;
use std::collections::HashMap;
use stroem_common::depends_on::{AcceptSet, DependsOnEntry, StepEntry, TerminalKeyword};
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

/// A `{step, accept: terminal}` dependency: pure ordering, any outcome
/// satisfies it.
fn ordering(name: &str) -> DependsOnEntry {
    DependsOnEntry::Step(StepEntry {
        step: name.to_string(),
        accept: AcceptSet::Terminal(TerminalKeyword),
    })
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
        git_ref: None,
        inline_action: None,
    }
}

/// Spec §3's full `recalc-pipeline` flow: `build-sessions`, `ai_maintain`,
/// `ai_sources`, the three `ml-prediction-*` branches, `dwell-time`, the
/// three `ml-impressions-*` branches, and `merge-ml` — with exactly the
/// `depends_on`/`accept`/`continue_on_failure` spec §3 specifies for each
/// edge. `continue_on_failure` on `build-sessions`, `ml-prediction-master`,
/// `dwell-time`, `ml-impressions-master`, and `merge-ml` is `false` by
/// inference from spec §3's "the same seven rules... hold here" framing
/// (no flag stated ⇒ no flag), not a value spec §3 states directly for
/// those five — each is exercised by one of the seven rules below (1, 4,
/// 7) so a wrong inference would show up as a test failure, not silently.
fn recalc_pipeline_flow() -> TaskDef {
    let mut flow = HashMap::new();

    // No dependencies, no flags — a failure here must block everything
    // below and fail the job outright (rule 1).
    flow.insert("build-sessions".to_string(), flow_step(vec![], false));

    // continue_on_failure: true (self-scoped — protects the job if
    // ai_maintain itself fails; unrelated to how ai_sources gates on it).
    flow.insert(
        "ai_maintain".to_string(),
        flow_step(vec![req("build-sessions")], true),
    );

    // build-sessions stays required (rule 1 must still hold transitively
    // through ai_sources); ai_maintain is ordering-only (accept: terminal),
    // so its failure OR skip never blocks ai_sources (rules 2/3).
    flow.insert(
        "ai_sources".to_string(),
        flow_step(vec![req("build-sessions"), ordering("ai_maintain")], true),
    );

    // No continue_on_failure: its own failure must fail the job (rule 4).
    flow.insert(
        "ml-prediction-master".to_string(),
        flow_step(vec![req("build-sessions")], false),
    );

    // continue_on_failure: true — a failed beta/stage prediction doesn't
    // fail the job (rules 5/6), but still blocks its OWN impressions step
    // (kept required below, not ordering).
    flow.insert(
        "ml-prediction-beta".to_string(),
        flow_step(vec![req("build-sessions")], true),
    );
    flow.insert(
        "ml-prediction-stage".to_string(),
        flow_step(vec![req("build-sessions")], true),
    );

    // Plain required step, no flags — "a failed dwell does not block
    // impressions" is expressed on the IMPRESSIONS steps' edges to it
    // (ordering-only below), not here.
    flow.insert(
        "dwell-time".to_string(),
        flow_step(vec![req("build-sessions")], false),
    );

    // Required prediction, ordering-only dwell-time.
    flow.insert(
        "ml-impressions-master".to_string(),
        flow_step(
            vec![req("ml-prediction-master"), ordering("dwell-time")],
            false,
        ),
    );

    // Its OWN prediction dependency (ml-prediction-beta) required;
    // dwell-time ordering-only. continue_on_failure: true (protects the
    // job if impressions-beta itself crashes).
    flow.insert(
        "ml-impressions-beta".to_string(),
        flow_step(
            vec![req("ml-prediction-beta"), ordering("dwell-time")],
            true,
        ),
    );

    // Mirrors beta with ITS OWN prediction dependency (ml-prediction-stage).
    flow.insert(
        "ml-impressions-stage".to_string(),
        flow_step(
            vec![req("ml-prediction-stage"), ordering("dwell-time")],
            true,
        ),
    );

    // Requires ml-impressions-master; tolerates beta/stage in any terminal
    // state (rule 7).
    flow.insert(
        "merge-ml".to_string(),
        flow_step(
            vec![
                req("ml-impressions-master"),
                ordering("ml-impressions-beta"),
                ordering("ml-impressions-stage"),
            ],
            false,
        ),
    );

    TaskDef {
        name: Some("recalc-pipeline".to_string()),
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,
        retry: None,
        on_success: vec![],
        on_error: vec![],
        on_suspended: vec![],
        on_cancel: vec![],
    }
}

// ─── Harness: drive cascade::run to a fixpoint with no real worker ─────────

fn workspace_with(task: &TaskDef) -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::new();
    ws.tasks.insert("recalc-pipeline".to_string(), task.clone());
    ws
}

fn seed_all_pending(task: &TaskDef, job_id: Uuid) -> Vec<JobStepRow> {
    task.flow
        .keys()
        .map(|name| JobStepRow::test_default(job_id, name))
        .collect()
}

/// Applies a plan's `Promote`/`Skip`/`Fail` changes to the in-memory rows.
/// `recalc-pipeline` has no `for_each` steps, so `Expand`/`Adopt`/`Rollup`
/// are unreachable here — if the gate ever produced one, that would itself
/// be a bug in this fixture.
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
            other => panic!("recalc-pipeline has no for_each steps — unexpected change: {other:?}"),
        }
    }
}

/// Resolves every `ready` row to a terminal status: the one named in
/// `overrides` for that step, or `completed` by default. Returns whether
/// any row was resolved (used by `run_to_terminal` to detect the
/// fixpoint).
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
            "skipped" => {
                // Simulates the step's own choice (its `when` false) —
                // Outcome::Skipped, not Outcome::Omitted.
                r.status = "skipped".to_string();
                r.skip_reason = Some("condition".to_string());
            }
            "cancelled" => {
                r.status = "cancelled".to_string();
            }
            other => panic!(
                "unsupported forced outcome '{other}' for step '{}'",
                r.step_name
            ),
        }
    }
    any
}

/// Drives the real cascade (`cascade::run`) to a fixpoint. There is no
/// worker in this harness: the moment a step becomes `ready`,
/// `resolve_ready` immediately resolves it to a terminal status, exactly
/// as `orchestrator_test.rs`'s DB-backed tests drive steps through
/// `JobStepRepo::mark_completed`/`mark_failed`, just without a database or
/// a `JobStepRepo`/`cascade::apply` round trip (this fixture only needs
/// the pure decision logic in `cascade::run`, not its persistence side).
fn run_to_terminal(task: &TaskDef, overrides: &HashMap<&str, &str>) -> Vec<JobStepRow> {
    let job = JobRow::test_default();
    let ws = workspace_with(task);
    let snapshots = Snapshots::default();
    let mut rows = seed_all_pending(task, job.job_id);

    const MAX_TICKS: usize = 50;
    for _ in 0..MAX_TICKS {
        let plan = cascade::run(task, &job, &rows, Some(&ws), &snapshots)
            .expect("cascade::run must not error for this fixture");
        let changed = !plan.changes.is_empty();
        apply_plan(&mut rows, &plan);
        let resolved = resolve_ready(&mut rows, overrides);
        if !changed && !resolved {
            return rows;
        }
    }
    panic!("recalc-pipeline did not reach a fixpoint within {MAX_TICKS} ticks: {rows:?}");
}

fn status_of<'a>(rows: &'a [JobStepRow], name: &str) -> &'a str {
    rows.iter()
        .find(|r| r.step_name == name)
        .unwrap_or_else(|| panic!("no such step '{name}' in row set"))
        .status
        .as_str()
}

fn skip_reason_of(rows: &[JobStepRow], name: &str) -> Option<String> {
    rows.iter()
        .find(|r| r.step_name == name)
        .unwrap_or_else(|| panic!("no such step '{name}' in row set"))
        .skip_reason
        .clone()
}

// ─── The seven rules (spec §3) ──────────────────────────────────────────────

#[test]
fn rule_1_build_sessions_fails_blocks_everything_below_it() {
    let task = recalc_pipeline_flow();
    let overrides = HashMap::from([("build-sessions", "failed")]);
    let rows = run_to_terminal(&task, &overrides);

    assert_eq!(status_of(&rows, "build-sessions"), "failed");
    for downstream in [
        "ai_maintain",
        "ai_sources",
        "ml-prediction-master",
        "ml-prediction-beta",
        "ml-prediction-stage",
        "dwell-time",
        "ml-impressions-master",
        "ml-impressions-beta",
        "ml-impressions-stage",
        "merge-ml",
    ] {
        assert_eq!(
            status_of(&rows, downstream),
            "skipped",
            "{downstream} must be skipped when build-sessions fails: {rows:?}"
        );
        assert_eq!(
            skip_reason_of(&rows, downstream).as_deref(),
            Some("unreachable"),
            "{downstream} must be omitted (unreachable), matching Outcome::Omitted for a blocked tree"
        );
    }

    let settled = settle::decide(&task, &rows).expect("every row is terminal");
    assert_eq!(
        settled.status,
        JobStatus::Failed,
        "build-sessions has no continue_on_failure — its failure must fail the job"
    );
}

#[test]
fn rules_2_and_3_ai_maintain_fails_or_skips_ai_sources_still_runs() {
    for ai_maintain_outcome in ["failed", "skipped"] {
        let task = recalc_pipeline_flow();
        let overrides = HashMap::from([("ai_maintain", ai_maintain_outcome)]);
        let rows = run_to_terminal(&task, &overrides);

        assert_eq!(status_of(&rows, "build-sessions"), "completed");
        assert_eq!(
            status_of(&rows, "ai_maintain"),
            ai_maintain_outcome,
            "sanity: ai_maintain must actually reach the forced outcome"
        );
        assert_eq!(
            status_of(&rows, "ai_sources"),
            "completed",
            "ai_sources must still run when ai_maintain is {ai_maintain_outcome}: {rows:?}"
        );
    }
}

#[test]
fn rule_4_ml_prediction_master_fails_fails_the_job() {
    let task = recalc_pipeline_flow();
    let overrides = HashMap::from([("ml-prediction-master", "failed")]);
    let rows = run_to_terminal(&task, &overrides);

    assert_eq!(status_of(&rows, "ml-prediction-master"), "failed");
    assert_eq!(
        status_of(&rows, "ml-impressions-master"),
        "skipped",
        "ml-impressions-master requires ml-prediction-master: {rows:?}"
    );
    assert_eq!(
        status_of(&rows, "merge-ml"),
        "skipped",
        "merge-ml requires ml-impressions-master: {rows:?}"
    );

    let settled = settle::decide(&task, &rows).expect("every row is terminal");
    assert_eq!(
        settled.status,
        JobStatus::Failed,
        "ml-prediction-master has no continue_on_failure — its failure must fail the job"
    );
}

#[test]
fn rules_5_and_6_beta_stage_prediction_fails_survives_job_but_impressions_skip() {
    for variant in ["beta", "stage"] {
        let task = recalc_pipeline_flow();
        let pred = format!("ml-prediction-{variant}");
        let imp = format!("ml-impressions-{variant}");
        let overrides = HashMap::from([(pred.as_str(), "failed")]);
        let rows = run_to_terminal(&task, &overrides);

        assert_eq!(status_of(&rows, &pred), "failed");
        assert_eq!(
            status_of(&rows, &imp),
            "skipped",
            "{imp} must skip — not run on empty prediction data: {rows:?}"
        );
        assert_eq!(skip_reason_of(&rows, &imp).as_deref(), Some("unreachable"));
        // The pipeline continues around the tolerated failure: merge-ml
        // only has an ordering (accept: terminal) edge to this variant's
        // impressions step, so it still runs.
        assert_eq!(
            status_of(&rows, "merge-ml"),
            "completed",
            "merge-ml must still run past the tolerated {variant} failure: {rows:?}"
        );

        let settled = settle::decide(&task, &rows).expect("every row is terminal");
        assert_ne!(
            settled.status,
            JobStatus::Failed,
            "ml-prediction-{variant}'s continue_on_failure must catch its own failure"
        );
    }
}

#[test]
fn rule_7_merge_ml_requires_master_tolerates_beta_and_stage() {
    let task = recalc_pipeline_flow();
    let overrides = HashMap::from([
        ("ml-impressions-beta", "failed"),
        ("ml-impressions-stage", "failed"),
    ]);
    let rows = run_to_terminal(&task, &overrides);

    assert_eq!(status_of(&rows, "ml-impressions-master"), "completed");
    assert_eq!(status_of(&rows, "ml-impressions-beta"), "failed");
    assert_eq!(status_of(&rows, "ml-impressions-stage"), "failed");
    assert_eq!(
        status_of(&rows, "merge-ml"),
        "completed",
        "merge-ml requires only ml-impressions-master, and tolerates beta/stage in any terminal state: {rows:?}"
    );

    // Both failures are on steps with their own continue_on_failure: true,
    // so they must not fail the job either — pins that value against
    // regression (the step-status assertions above only check gating, not
    // accounting; a job that failed here would mean the fixture's
    // continue_on_failure for these two steps was wrong, or regressed).
    let settled = settle::decide(&task, &rows).expect("every row is terminal");
    assert_ne!(
        settled.status,
        JobStatus::Failed,
        "job must not fail — both impressions failures are tolerated by their own continue_on_failure: {rows:?}"
    );
}

// Sanity check that the model actually discriminates, not just permissive
// gating: an UNFORCED run (no overrides at all) must complete everything.
#[test]
fn happy_path_everything_completes_with_no_forced_outcomes() {
    let task = recalc_pipeline_flow();
    let rows = run_to_terminal(&task, &HashMap::new());
    for name in task.flow.keys() {
        assert_eq!(
            status_of(&rows, name),
            "completed",
            "{name} should complete on the happy path: {rows:?}"
        );
    }
    let settled = settle::decide(&task, &rows).expect("every row is terminal");
    assert_eq!(settled.status, JobStatus::Completed);
}
