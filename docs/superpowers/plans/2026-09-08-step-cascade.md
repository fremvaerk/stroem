# Step Cascade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the two duplicated promote→skip→expand loops, the DB-crate `when` evaluation and the keyed loop rollup with one pure `cascade::run` over an in-memory snapshot, applied in one transaction with today's guards.

**Architecture:** `crates/stroem-server/src/cascade.rs` holds a closed `Change` enum, a pure `run(task, job, steps, workspace_config) -> Plan` that iterates the phase model (rollup → promote/cascade-skip → skip-unreachable → retire/expand) to a fixpoint on its own snapshot, an `apply(conn, job_id, plan)` that composes transaction-taking stroem-db primitives with affected-row-count checks, and `execute(pool, ...)` that reads, runs, applies in one transaction and re-runs on a guard miss. Both callers (`orchestrator::on_step_completed` and the creation-time init block) switch to `execute`; the four old functions are deleted in one activation commit.

**Tech Stack:** Rust, sqlx runtime queries, Tera via `stroem_common::template`, testcontainers Postgres for integration tests.

**Spec:** `docs/superpowers/specs/2026-09-08-step-cascade-design.md` (rev 8). §4.3 is the rule set, §4.4 the phase model, §4.6 the guards, §4.7 `execute`, §9 the tests, §10 the commit order this plan follows.

**Prerequisite:** the fail-or-retry branch (`docs/superpowers/plans/2026-09-08-fail-or-retry.md`) is merged first. Tasks 1–3 here compile without it; Task 4 (activation) must not be deployed to a fleet that still has a replica without it.

## Global Constraints

- `anyhow::Result` with `.context("...")`; sqlx runtime queries only.
- Multi-statement repo functions take `&mut sqlx::PgConnection` (precedent `seed_steps_tx`, `job_step.rs:301`); single-statement ones are generic `E: sqlx::Executor<'e, Database = sqlx::Postgres>` and callers pass `&mut *tx` / `&mut *conn`.
- Every error-message text is preserved byte for byte: `"when condition error: {:#}"`, `"for_each expression error: {:#}"`, `"for_each produced {} items (max {})"`, `"for_each loop failed: instances {:?} failed"`.
- `job` is inserted into the template context before step outputs (a step named `job` shadows it) — never reorder `build_step_render_context`.
- `on_step_completed`'s signature does not change; about 80 tests pass `None` as workspace config and must keep passing untouched until Task 8.
- No AI co-author trailer in commit messages. `cargo fmt --all` before each commit; `cargo clippy --workspace -- -D warnings` clean.
- Container tests need Docker: `cargo test -p stroem-server --test orchestrator_test`, `--test integration_test <filter>`.

---

### Task 1: `cascade::run` — the pure fixpoint, with its unit suite

**Files:**
- Create: `crates/stroem-server/src/cascade.rs`
- Modify: `crates/stroem-server/src/lib.rs` (add `pub mod cascade;` after `pub mod cancellation;`)
- Modify: `crates/stroem-server/src/job_creator.rs:933` (`const MAX_FOR_EACH_ITEMS` → `pub(crate) const`), `:1155` (`fn parse_for_each_items` → `pub(crate) fn`), `:1181` (`fn render_for_each_template` → `pub(crate) fn`)

**Interfaces:**
- Consumes: `stroem_db::{JobRow, JobStepRow, NewJobStep}`; `stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig}`; `stroem_common::template::evaluate_condition(&str, &Value) -> Result<bool>`; `crate::job_creator::build_step_render_context(&JobRow, &[JobStepRow], &WorkspaceConfig) -> Value`; `crate::job_creator::parse_for_each_items(&str, &Value) -> Result<Vec<Value>>`; `crate::job_creator::MAX_FOR_EACH_ITEMS`.
- Produces:
  ```rust
  pub enum Change { Promote{step}, Skip{step}, Fail{step,error}, Expand{placeholder,instances: Vec<NewJobStep>}, Adopt{placeholder}, Rollup{placeholder,outcome: RollupOutcome} }
  pub enum RollupOutcome { Completed(Value), Failed(String) }
  pub struct Plan { pub changes: Vec<Change> }
  pub fn run(task: &TaskDef, job: &JobRow, steps: &[JobStepRow], workspace_config: Option<&WorkspaceConfig>) -> Result<Plan>
  pub(crate) fn is_terminal(status: &str) -> bool
  ```
  `Adopt` is declared now (so `apply` in Task 2 handles it) but `run` does not emit it until Task 6.

- [ ] **Step 1: Make the three job_creator helpers crate-visible**

In `crates/stroem-server/src/job_creator.rs` change line 933 to `pub(crate) const MAX_FOR_EACH_ITEMS: usize = 10000;`, line 1155 to `pub(crate) fn parse_for_each_items(`, line 1181 to `pub(crate) fn render_for_each_template(`. Run `cargo build -p stroem-server`; expected: clean.

- [ ] **Step 2: Write the failing unit tests**

Create `crates/stroem-server/src/cascade.rs` with only the test module for now (the implementation follows in Step 4):

```rust
//! Step cascade: the pure fixpoint that moves a job's non-running steps.
//! See docs/superpowers/specs/2026-09-08-step-cascade-design.md.

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
        JobStepRow { when_condition: Some(when.to_string()), ..row(name, status) }
    }
    fn row_out(name: &str, output: Value) -> JobStepRow {
        JobStepRow { output: Some(output), ..row(name, "completed") }
    }
    fn placeholder(name: &str, status: &str, expr: &str) -> JobStepRow {
        JobStepRow { for_each_expr: Some(expr.to_string()), ..row(name, status) }
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
        FlowStep { continue_on_failure: true, ..fs(deps) }
    }
    fn fs_seq(deps: &[&str]) -> FlowStep {
        FlowStep { sequential: true, ..fs(deps) }
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

    fn ws() -> WorkspaceConfig {
        WorkspaceConfig::new()
    }

    /// Final in-memory status of every row after `run`, by name.
    fn final_statuses(plan: &Plan, rows: &[JobStepRow]) -> HashMap<String, String> {
        let mut snap = Snapshot::new(rows.to_vec());
        for c in &plan.changes {
            snap.apply(c);
        }
        snap.rows.iter().map(|r| (r.step_name.clone(), r.status.clone())).collect()
    }

    fn names(plan: &Plan) -> Vec<String> {
        plan.changes
            .iter()
            .map(|c| match c {
                Change::Promote { step } => format!("promote:{step}"),
                Change::Skip { step } => format!("skip:{step}"),
                Change::Fail { step, .. } => format!("fail:{step}"),
                Change::Expand { placeholder, instances } => {
                    format!("expand:{placeholder}:{}", instances.len())
                }
                Change::Adopt { placeholder } => format!("adopt:{placeholder}"),
                Change::Rollup { placeholder, outcome: RollupOutcome::Completed(_) } => {
                    format!("rollup-ok:{placeholder}")
                }
                Change::Rollup { placeholder, outcome: RollupOutcome::Failed(_) } => {
                    format!("rollup-fail:{placeholder}")
                }
            })
            .collect()
    }

    // ── R1/R2/R3: promotion and skipping ─────────────────────────────

    #[test]
    fn linear_promotion() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"])), ("c", fs(&["b"]))]);
        let rows = vec![row("a", "completed"), row("b", "pending"), row("c", "pending")];
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
        let t = task(vec![("a", fs(&[])), ("b", fs_cof(&["a"])), ("x", fs(&[])), ("y", fs_cof(&["x"]))]);
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
        let t = task(vec![("a", fs(&[])), ("t", fs(&["a"])), ("f", fs(&["a"])), ("e", fs(&["a"]))]);
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
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"])), ("d", fs_cof(&["a"]))]);
        // a failed, b still running: c has a failed dep → R3 skips it; d has cof → stays.
        let rows = vec![row("a", "failed"), row("b", "running"), row("c", "pending"), row("d", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["c"], "skipped");
        assert_eq!(s["d"], "ready", "cof dependent of a failed dep is promoted by R2");
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
                when_condition: Some("{% if a is defined %}true{% else %}false{% endif %}".to_string()),
                ..placeholder("p", "pending", "[1,2]")
            },
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["skip:a", "expand:p:2"]);
    }

    #[test]
    fn when_in_p1_does_not_see_a_skip_from_the_same_phase_until_next_pass() {
        // `a` and `b` are both roots; b's when references a. In P1 both are
        // evaluated on the phase-start snapshot: a is skipped, but b sees no `a`
        // (undefined → error would fail b). Then in pass 2, b sees a as skipped.
        // Use a template that is an error when a is undefined and truthy when
        // a is defined: `{{ a.output }}` renders "" for skipped a (null) → falsy.
        let t = task(vec![("a", fs(&[])), ("b", fs(&[]))]);
        let rows = vec![
            row_when("a", "pending", "false"),
            row_when("b", "pending", "{% if a is defined %}yes{% else %}{{ a.output }}{% endif %}"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        // pass 1: a skipped, b fails (a undefined inside the else branch)
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["a"], "skipped");
        assert_eq!(s["b"], "failed", "b rendered against the phase-start snapshot");
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
            JobStepRow { when_condition: Some("false".into()), ..placeholder("p_when_false", "pending", "[1]") },
            JobStepRow { when_condition: Some("{{ nope.x }}".into()), ..placeholder("p_when_err", "pending", "[1]") },
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
        assert_eq!(too_many_err.unwrap(), "for_each produced 10001 items (max 10000)");
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
            if let Change::Expand { placeholder, instances } = c {
                assert_eq!(instances.len(), 2);
                assert_eq!(instances[0].step_name, format!("{placeholder}[0]"));
                assert_eq!(instances[0].loop_source.as_deref(), Some(placeholder.as_str()));
                assert_eq!(instances[0].loop_index, Some(0));
                assert_eq!(instances[0].loop_total, Some(2));
                assert_eq!(instances[0].loop_item, Some(json!("a")));
                assert!(instances[0].when_condition.is_none());
                assert!(instances[0].for_each_expr.is_none());
                assert_eq!(instances[0].status, "ready");
                let second = if placeholder == "seq" { "pending" } else { "ready" };
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
        let t = task(vec![("x", FlowStep { continue_on_failure: true, ..fs_seq(&[]) })]);
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
    fn rollup_completed_orders_by_index_with_null_gaps_and_cancelled_ok() {
        let t = task(vec![("x", fs(&[]))]);
        let rows = vec![
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 2, "completed", Some(json!("c"))),
            instance("x", 0, "completed", Some(json!("a"))),
            instance("x", 1, "cancelled", None),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        match &plan.changes[..] {
            [Change::Rollup { placeholder, outcome: RollupOutcome::Completed(out) }] => {
                assert_eq!(placeholder, "x");
                assert_eq!(out, &json!(["a", null, "c"]));
            }
            other => panic!("unexpected plan {:?}", names(&Plan { changes: other.to_vec() })),
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
                Change::Rollup { placeholder, outcome: RollupOutcome::Failed(e) } => {
                    fails.push((placeholder.clone(), e.clone()))
                }
                Change::Rollup { placeholder, outcome: RollupOutcome::Completed(_) } => {
                    oks.push(placeholder.clone())
                }
                _ => {}
            }
        }
        assert_eq!(fails, [("x".to_string(), "for_each loop failed: instances [1, 2] failed".to_string())]);
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
        let rows = vec![placeholder("x", "running", "[1]"), instance("x", 0, "completed", Some(json!(1)))];
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
    fn fixpoint_is_idempotent_and_terminates_on_a_wide_dag() {
        let mut flow = vec![("root", fs(&[]))];
        let mut rows = vec![row("root", "completed")];
        for i in 0..300 {
            let name = format!("s{i}");
            let dep = if i == 0 { "root".to_string() } else { format!("s{}", i - 1) };
            flow.push((Box::leak(name.clone().into_boxed_str()), fs(&[Box::leak(dep.into_boxed_str())])));
            rows.push(row(&name, "pending"));
        }
        let t = task(flow);
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["promote:s0"], "only the first is promotable");
        let mut snap = rows.clone();
        snap[1].status = "ready".to_string();
        let again = run(&t, &job(None), &snap, Some(&ws())).unwrap();
        assert!(again.changes.is_empty(), "snapshot at fixpoint yields an empty plan");
    }

    #[test]
    fn job_input_and_secret_reach_when_templates() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"]))]);
        let rows = vec![row("a", "completed"), row_when("b", "pending", "{{ input.fast }}")];
        let plan = run(&t, &job(Some(json!({"fast": true}))), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
        let plan = run(&t, &job(Some(json!({"fast": false}))), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["skip:b"]);
    }

    /// Mirrors orchestrator_test::test_convergence_without_continue_on_failure.
    #[test]
    fn convergence_without_continue_on_failure_scenario() {
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"])), ("c", fs(&["a"])), ("d", fs(&["b", "c"]))]);
        let rows = vec![
            row("a", "completed"),
            row_when("b", "pending", "{% if input.use_fast %}true{% endif %}"),
            row_when("c", "pending", "{% if not input.use_fast %}true{% endif %}"),
            row("d", "pending"),
        ];
        let plan = run(&t, &job(Some(json!({"use_fast": true}))), &rows, Some(&ws())).unwrap();
        let s = final_statuses(&plan, &rows);
        assert_eq!(s["b"], "ready");
        assert_eq!(s["c"], "skipped");
        assert_eq!(s["d"], "pending", "d waits for b");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Add `pub mod cascade;` to `crates/stroem-server/src/lib.rs` after `pub mod cancellation;`.
Run: `cargo test -p stroem-server --lib cascade::tests`
Expected: compile errors (`run`, `Change`, `Plan`, `Snapshot` not found).

- [ ] **Step 4: Implement `run`**

Put this above the `#[cfg(test)]` module in `crates/stroem-server/src/cascade.rs`:

```rust
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};
use stroem_db::{JobRow, JobStepRow, NewJobStep};

use crate::job_creator::{build_step_render_context, parse_for_each_items, MAX_FOR_EACH_ITEMS};

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
    Expand { placeholder: String, instances: Vec<NewJobStep> },
    /// Placeholder pending → running, instances already exist (R0). No insert.
    Adopt { placeholder: String },
    /// running placeholder → completed(output) | failed(error)
    Rollup { placeholder: String, outcome: RollupOutcome },
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

/// In-memory copy of a job's step rows that the fixpoint mutates.
pub(crate) struct Snapshot {
    pub(crate) rows: Vec<JobStepRow>,
    index: HashMap<String, usize>,
}

impl Snapshot {
    pub(crate) fn new(rows: Vec<JobStepRow>) -> Self {
        let index = rows.iter().enumerate().map(|(i, r)| (r.step_name.clone(), i)).collect();
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
            Change::Expand { placeholder, instances } => {
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
            Change::Rollup { placeholder, outcome } => {
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
    !fs.depends_on.is_empty() && fs.depends_on.iter().all(|d| snap.status(d) == Some(SKIPPED))
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
    for ph in snap.rows.iter().filter(|r| is_placeholder(r) && r.status == RUNNING) {
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
                for i in instances.iter().filter(|i| i.status == PENDING) {
                    out.push(Change::Skip { step: i.step_name.clone() });
                }
                continue; // nothing else for this placeholder this phase
            }
            for i in instances.iter().filter(|i| is_terminal(&i.status)) {
                let next_idx = i.loop_index.unwrap_or(0) + 1;
                if let Some(next) = instances
                    .iter()
                    .find(|n| n.loop_index == Some(next_idx) && n.status == PENDING)
                {
                    out.push(Change::Promote { step: next.step_name.clone() });
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
            out.push(Change::Rollup { placeholder: ph.step_name.clone(), outcome });
        }
    }
    out
}

/// P1: R1 cascade-skip, then R2 promote (with `when`).
fn phase_promote(snap: &Snapshot, task: &TaskDef, ctx: Option<&Value>) -> Vec<Change> {
    let mut out = Vec::new();
    for r in snap.rows.iter().filter(|r| r.status == PENDING && !is_placeholder(r)) {
        let Some(fs) = task.flow.get(&r.step_name) else { continue };
        if all_deps_skipped(snap, fs) && !fs.continue_on_failure {
            out.push(Change::Skip { step: r.step_name.clone() });
            continue;
        }
        if !deps_satisfied(snap, fs) {
            continue;
        }
        match (&r.when_condition, ctx) {
            (None, _) => out.push(Change::Promote { step: r.step_name.clone() }),
            (Some(_), None) => {} // no template context: stays pending (today's behaviour)
            (Some(w), Some(ctx)) => match stroem_common::template::evaluate_condition(w, ctx) {
                Ok(true) => out.push(Change::Promote { step: r.step_name.clone() }),
                Ok(false) => out.push(Change::Skip { step: r.step_name.clone() }),
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
    for r in snap.rows.iter().filter(|r| r.status == PENDING && !is_placeholder(r)) {
        let Some(fs) = task.flow.get(&r.step_name) else { continue };
        if !fs.continue_on_failure && any_dep_failed_or_cancelled(snap, fs) {
            out.push(Change::Skip { step: r.step_name.clone() });
        }
    }
    out
}

/// P3: R0 adopt (Task 6) + R4 retire/expand placeholders. Needs a context.
fn phase_placeholders(snap: &Snapshot, task: &TaskDef, ctx: Option<&Value>, job_id: uuid::Uuid) -> Vec<Change> {
    let mut out = Vec::new();
    let Some(ctx) = ctx else { return out };
    for r in snap.rows.iter().filter(|r| r.status == PENDING && is_placeholder(r)) {
        let Some(fs) = task.flow.get(&r.step_name) else { continue };
        // Idempotency guard as today (job_creator.rs:975-978): instances already
        // exist → leave the placeholder alone. Task 6 turns this into `Adopt`.
        if snap.status(&format!("{}[0]", r.step_name)).is_some() {
            continue;
        }
        if !deps_satisfied(snap, fs) {
            if any_dep_failed_or_cancelled(snap, fs) && !fs.continue_on_failure {
                out.push(Change::Skip { step: r.step_name.clone() });
            }
            continue;
        }
        if all_deps_skipped(snap, fs) && !fs.continue_on_failure {
            out.push(Change::Skip { step: r.step_name.clone() });
            continue;
        }
        if let Some(w) = &r.when_condition {
            match stroem_common::template::evaluate_condition(w, ctx) {
                Ok(true) => {}
                Ok(false) => {
                    out.push(Change::Skip { step: r.step_name.clone() });
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
            out.push(Change::Skip { step: r.step_name.clone() });
            continue;
        }
        if items.len() > MAX_FOR_EACH_ITEMS {
            out.push(Change::Fail {
                step: r.step_name.clone(),
                error: format!("for_each produced {} items (max {})", items.len(), MAX_FOR_EACH_ITEMS),
            });
            continue;
        }
        let total = items.len() as i32;
        let instances = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let instance_status = if fs.sequential && i > 0 { StepStatus::Pending } else { StepStatus::Ready };
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
                    required_tags: serde_json::from_value(r.required_tags.clone()).unwrap_or_default(),
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
        out.push(Change::Expand { placeholder: r.step_name.clone(), instances });
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
pub fn run(
    task: &TaskDef,
    job: &JobRow,
    steps: &[JobStepRow],
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<Plan> {
    let pending_placeholders = steps.iter().filter(|r| is_placeholder(r) && r.status == PENDING).count();
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
```

- [ ] **Step 5: Run the unit suite**

Run: `cargo test -p stroem-server --lib cascade::tests`
Expected: all tests pass. If `fixpoint_is_idempotent_and_terminates_on_a_wide_dag` fails on the `Box::leak` borrow juggling, replace the loop with a `Vec<(String, FlowStep)>` and a `task_owned` helper taking owned strings; the assertion is the point.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/cascade.rs crates/stroem-server/src/lib.rs crates/stroem-server/src/job_creator.rs
git commit -m "feat(cascade): pure cascade::run with the phase model and rules R1-R7; unit suite"
```

---

### Task 2: stroem-db primitives, `apply`, `ApplyError`

**Files:**
- Modify: `crates/stroem-db/src/repos/job_step.rs` (add six `_tx` primitives after `create_steps_tx`, line ~230)
- Modify: `crates/stroem-db/src/repos/job.rs` (add `mark_running_if_pending_tx` after `mark_running_if_pending_server`, line ~446)
- Modify: `crates/stroem-server/src/cascade.rs` (add `Applied`, `ApplyError`, `apply`)
- Test: `crates/stroem-server/tests/cascade_apply_test.rs` (new; copies `setup_db`, `create_job`, `step`, `step_statuses` from `orchestrator_test.rs:24-160`)

**Interfaces:**
- Produces (stroem-db, each one statement, each returning `rows_affected`):
  ```rust
  JobStepRepo::promote_steps_tx<'e,E>(executor: E, job_id: Uuid, names: &[String]) -> Result<u64>
  JobStepRepo::skip_steps_tx<'e,E>(executor: E, job_id: Uuid, names: &[String]) -> Result<u64>
  JobStepRepo::fail_pending_step_tx<'e,E>(executor: E, job_id: Uuid, name: &str, error: &str) -> Result<u64>
  JobStepRepo::start_placeholder_tx<'e,E>(executor: E, job_id: Uuid, name: &str) -> Result<u64>
  JobStepRepo::complete_placeholder_tx<'e,E>(executor: E, job_id: Uuid, name: &str, output: &JsonValue) -> Result<u64>
  JobStepRepo::fail_placeholder_tx<'e,E>(executor: E, job_id: Uuid, name: &str, error: &str) -> Result<u64>
  JobRepo::mark_running_if_pending_tx<'e,E>(executor: E, job_id: Uuid) -> Result<()>
  ```
- Produces (cascade):
  ```rust
  #[derive(Debug, Default, Clone, PartialEq, Eq)]
  pub struct Applied { pub promoted: usize, pub skipped: usize, pub failed: usize, pub expanded: usize, pub adopted: usize, pub rolled_up: usize }
  #[derive(Debug, thiserror::Error)]
  pub enum ApplyError { #[error("cascade guard miss on step '{step}'")] GuardMiss { step: String }, #[error(transparent)] Db(#[from] anyhow::Error) }
  pub async fn apply(conn: &mut sqlx::PgConnection, job_id: Uuid, plan: &Plan) -> Result<Applied, ApplyError>
  ```
  If `thiserror` is not already a dependency of stroem-server (check `crates/stroem-server/Cargo.toml`), implement `Display` and `std::error::Error` by hand instead of adding it.

- [ ] **Step 1: Write the failing integration tests**

Create `crates/stroem-server/tests/cascade_apply_test.rs`:

```rust
use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep};
use stroem_server::cascade::{apply, ApplyError, Change, Plan, RollupOutcome};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

async fn setup_db() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

async fn create_job(pool: &PgPool) -> Uuid {
    JobRepo::create(pool, "default", "test-task", "distributed", None, "api", None, None, None)
        .await
        .expect("create job")
}

fn step(job_id: Uuid, name: &str, status: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "noop".to_string(),
        action_type: "script".to_string(),
        action_image: None,
        action_spec: Some(json!({"script": "true"})),
        input: None,
        status: status.to_string(),
        required_ability: "script".to_string(),
        required_tags: vec!["script".to_string()],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
        for_each_expr: None,
        loop_source: None,
        loop_index: None,
        loop_total: None,
        loop_item: None,
        max_retries: None,
        retry_backoff_secs: None,
        retry_strategy: None,
        retry_jitter: false,
        action_workspace: None,
        action_revision: None,
    }
}

fn placeholder(job_id: Uuid, name: &str, status: &str) -> NewJobStep {
    NewJobStep { for_each_expr: Some("[1,2]".to_string()), ..step(job_id, name, status) }
}

fn instance(job_id: Uuid, source: &str, i: i32, status: &str) -> NewJobStep {
    NewJobStep {
        loop_source: Some(source.to_string()),
        loop_index: Some(i),
        loop_total: Some(2),
        loop_item: Some(json!(i)),
        ..step(job_id, &format!("{source}[{i}]"), status)
    }
}

async fn step_statuses(pool: &PgPool, job_id: Uuid) -> HashMap<String, String> {
    JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .expect("get steps")
        .into_iter()
        .map(|s| (s.step_name, s.status))
        .collect()
}

#[tokio::test]
async fn apply_is_transactional_expand_rolls_back_without_commit() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[placeholder(job_id, "p", "pending")]).await?;
    let plan = Plan {
        changes: vec![Change::Expand {
            placeholder: "p".into(),
            instances: vec![instance(job_id, "p", 0, "ready"), instance(job_id, "p", 1, "ready")],
        }],
    };
    {
        let mut tx = pool.begin().await?;
        let applied = apply(&mut tx, job_id, &plan).await.unwrap();
        assert_eq!(applied.expanded, 1);
        // dropped without commit
    }
    let s = step_statuses(&pool, job_id).await;
    assert_eq!(s["p"], "pending");
    assert!(!s.contains_key("p[0]"), "instances must not exist without the placeholder transition");
    Ok(())
}

#[tokio::test]
async fn apply_sets_timestamps_and_statuses() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "pending"),
            step(job_id, "b", "pending"),
            step(job_id, "c", "pending"),
            placeholder(job_id, "p", "pending"),
            placeholder(job_id, "q", "running"),
            instance(job_id, "q", 0, "completed"),
        ],
    )
    .await?;
    let plan = Plan {
        changes: vec![
            Change::Promote { step: "a".into() },
            Change::Skip { step: "b".into() },
            Change::Fail { step: "c".into(), error: "when condition error: x".into() },
            Change::Expand { placeholder: "p".into(), instances: vec![instance(job_id, "p", 0, "ready")] },
            Change::Rollup { placeholder: "q".into(), outcome: RollupOutcome::Completed(json!([1])) },
        ],
    };
    let mut tx = pool.begin().await?;
    let applied = apply(&mut tx, job_id, &plan).await.unwrap();
    tx.commit().await?;
    assert_eq!(applied.promoted, 1);
    assert_eq!(applied.skipped, 1);
    assert_eq!(applied.failed, 1);
    assert_eq!(applied.expanded, 1);
    assert_eq!(applied.rolled_up, 1);

    let rows = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let by = |n: &str| rows.iter().find(|r| r.step_name == n).unwrap();
    assert_eq!(by("a").status, "ready");
    let ready_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT ready_at FROM job_step WHERE job_id = $1 AND step_name = 'a'")
            .bind(job_id)
            .fetch_one(&pool)
            .await?;
    assert!(ready_at.is_some(), "Promote sets ready_at");
    assert_eq!(by("b").status, "skipped");
    assert!(by("b").completed_at.is_some(), "Skip sets completed_at");
    assert_eq!(by("c").status, "failed");
    assert_eq!(by("c").error_message.as_deref(), Some("when condition error: x"));
    assert!(by("c").completed_at.is_some());
    assert_eq!(by("p").status, "running");
    assert!(by("p").started_at.is_some(), "Expand sets started_at");
    assert_eq!(by("p[0]").status, "ready");
    assert_eq!(by("q").status, "completed");
    assert_eq!(by("q").output, Some(json!([1])));
    assert!(by("q").completed_at.is_some());
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "running", "Expand moves a pending job to running");
    Ok(())
}

#[tokio::test]
async fn apply_guard_miss_returns_error_and_writes_nothing_after_rollback() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "a", "pending"), step(job_id, "b", "cancelled")]).await?;
    let plan = Plan {
        changes: vec![Change::Promote { step: "a".into() }, Change::Promote { step: "b".into() }],
    };
    let mut tx = pool.begin().await?;
    let err = apply(&mut tx, job_id, &plan).await.unwrap_err();
    assert!(matches!(err, ApplyError::GuardMiss { ref step } if step == "b"), "{err}");
    tx.rollback().await?;
    let s = step_statuses(&pool, job_id).await;
    assert_eq!(s["a"], "pending", "the whole plan rolled back");
    assert_eq!(s["b"], "cancelled");
    Ok(())
}

#[tokio::test]
async fn apply_rollup_on_non_running_placeholder_is_a_guard_miss() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[placeholder(job_id, "q", "cancelled")]).await?;
    let plan = Plan {
        changes: vec![Change::Rollup { placeholder: "q".into(), outcome: RollupOutcome::Completed(json!([])) }],
    };
    let mut tx = pool.begin().await?;
    let err = apply(&mut tx, job_id, &plan).await.unwrap_err();
    assert!(matches!(err, ApplyError::GuardMiss { .. }));
    Ok(())
}

#[tokio::test]
async fn apply_job_running_update_may_match_zero_rows() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    sqlx::query("UPDATE job SET status = 'running' WHERE job_id = $1").bind(job_id).execute(&pool).await?;
    JobStepRepo::create_steps(&pool, &[placeholder(job_id, "p", "pending")]).await?;
    let plan = Plan {
        changes: vec![Change::Expand { placeholder: "p".into(), instances: vec![instance(job_id, "p", 0, "ready")] }],
    };
    let mut tx = pool.begin().await?;
    apply(&mut tx, job_id, &plan).await.unwrap();
    tx.commit().await?;
    assert_eq!(step_statuses(&pool, job_id).await["p"], "running");
    Ok(())
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p stroem-server --test cascade_apply_test`
Expected: compile error, `apply`, `ApplyError`, `Applied` not found in `stroem_server::cascade`.

- [ ] **Step 3: stroem-db primitives**

In `crates/stroem-db/src/repos/job_step.rs`, inside `impl JobStepRepo`, after `create_steps_tx`:

```rust
    /// Cascade primitive: pending → ready for every named step. Returns rows affected.
    pub async fn promote_steps_tx<'e, E>(executor: E, job_id: Uuid, names: &[String]) -> Result<u64>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let r = sqlx::query(
            "UPDATE job_step SET status = 'ready', ready_at = NOW() \
             WHERE job_id = $1 AND step_name = ANY($2) AND status = 'pending'",
        )
        .bind(job_id)
        .bind(names)
        .execute(executor)
        .await
        .context("promote_steps_tx")?;
        Ok(r.rows_affected())
    }

    /// Cascade primitive: pending → skipped for every named step. Returns rows affected.
    pub async fn skip_steps_tx<'e, E>(executor: E, job_id: Uuid, names: &[String]) -> Result<u64>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let r = sqlx::query(
            "UPDATE job_step SET status = 'skipped', completed_at = NOW() \
             WHERE job_id = $1 AND step_name = ANY($2) AND status = 'pending'",
        )
        .bind(job_id)
        .bind(names)
        .execute(executor)
        .await
        .context("skip_steps_tx")?;
        Ok(r.rows_affected())
    }

    /// Cascade primitive: pending → failed with an error. Returns rows affected.
    pub async fn fail_pending_step_tx<'e, E>(executor: E, job_id: Uuid, name: &str, error: &str) -> Result<u64>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let r = sqlx::query(
            "UPDATE job_step SET status = 'failed', error_message = $3, completed_at = NOW() \
             WHERE job_id = $1 AND step_name = $2 AND status = 'pending'",
        )
        .bind(job_id)
        .bind(name)
        .bind(error)
        .execute(executor)
        .await
        .context("fail_pending_step_tx")?;
        Ok(r.rows_affected())
    }

    /// Cascade primitive: placeholder pending → running. Returns rows affected.
    pub async fn start_placeholder_tx<'e, E>(executor: E, job_id: Uuid, name: &str) -> Result<u64>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let r = sqlx::query(
            "UPDATE job_step SET status = 'running', started_at = NOW() \
             WHERE job_id = $1 AND step_name = $2 AND status = 'pending'",
        )
        .bind(job_id)
        .bind(name)
        .execute(executor)
        .await
        .context("start_placeholder_tx")?;
        Ok(r.rows_affected())
    }

    /// Cascade primitive: running placeholder → completed with aggregated output.
    pub async fn complete_placeholder_tx<'e, E>(executor: E, job_id: Uuid, name: &str, output: &JsonValue) -> Result<u64>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let r = sqlx::query(
            "UPDATE job_step SET status = 'completed', output = $3, completed_at = NOW() \
             WHERE job_id = $1 AND step_name = $2 AND status = 'running'",
        )
        .bind(job_id)
        .bind(name)
        .bind(output)
        .execute(executor)
        .await
        .context("complete_placeholder_tx")?;
        Ok(r.rows_affected())
    }

    /// Cascade primitive: running placeholder → failed.
    pub async fn fail_placeholder_tx<'e, E>(executor: E, job_id: Uuid, name: &str, error: &str) -> Result<u64>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let r = sqlx::query(
            "UPDATE job_step SET status = 'failed', error_message = $3, completed_at = NOW() \
             WHERE job_id = $1 AND step_name = $2 AND status = 'running'",
        )
        .bind(job_id)
        .bind(name)
        .bind(error)
        .execute(executor)
        .await
        .context("fail_placeholder_tx")?;
        Ok(r.rows_affected())
    }
```

In `crates/stroem-db/src/repos/job.rs`, inside `impl JobRepo`, after `mark_running_if_pending_server`:

```rust
    /// Transaction variant of `mark_running_if_pending_server`. Zero rows is normal
    /// once the job is already running.
    pub async fn mark_running_if_pending_tx<'e, E>(executor: E, job_id: Uuid) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        sqlx::query("UPDATE job SET status = 'running', started_at = NOW() WHERE job_id = $1 AND status = 'pending'")
            .bind(job_id)
            .execute(executor)
            .await
            .context("mark_running_if_pending_tx")?;
        Ok(())
    }
```

- [ ] **Step 4: `apply` in cascade.rs**

Add to `crates/stroem-server/src/cascade.rs` (above the tests; add `use stroem_db::{JobRepo, JobStepRepo};` and `use uuid::Uuid;` to the imports):

```rust
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
    GuardMiss { step: String },
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
        Err(ApplyError::GuardMiss { step: step.to_string() })
    }
}

/// Apply `plan.changes` in order inside the caller's transaction (§4.6). Batches
/// runs of consecutive `Promote`s and `Skip`s into one statement each; every
/// step-row statement must affect exactly the rows it names.
pub async fn apply(conn: &mut sqlx::PgConnection, job_id: Uuid, plan: &Plan) -> Result<Applied, ApplyError> {
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
            Change::Expand { placeholder, instances } => {
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
            Change::Rollup { placeholder, outcome } => {
                let n = match outcome {
                    RollupOutcome::Completed(out) => {
                        JobStepRepo::complete_placeholder_tx(&mut *conn, job_id, placeholder, out).await?
                    }
                    RollupOutcome::Failed(err) => {
                        JobStepRepo::fail_placeholder_tx(&mut *conn, job_id, placeholder, err).await?
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
```

Note for the guard-miss test: a batched `Promote` of `a`,`b` where `b` is cancelled affects 1 of 2 rows, so `GuardMiss { step: "a,b" }`. Adjust that test's assertion to `step.contains("b")`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p stroem-server --test cascade_apply_test` and `cargo test -p stroem-server --lib cascade`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/stroem-db/src/repos/job_step.rs crates/stroem-db/src/repos/job.rs crates/stroem-server/src/cascade.rs crates/stroem-server/tests/cascade_apply_test.rs
git commit -m "feat(cascade): transaction primitives and all-or-nothing apply with row-count guards"
```

---

### Task 3: `execute` — read, run, apply in one transaction, re-run on a guard miss

**Files:**
- Modify: `crates/stroem-server/src/cascade.rs`
- Test: `crates/stroem-server/tests/cascade_apply_test.rs` (append)

**Interfaces:**
- Produces: `pub async fn execute(pool: &PgPool, job_id: Uuid, task: &TaskDef, workspace_config: Option<&WorkspaceConfig>) -> Result<Plan>`

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-server/tests/cascade_apply_test.rs` (add `use stroem_server::cascade::execute;`, `use stroem_common::models::workflow::{FlowStep, TaskDef, WorkspaceConfig};` and copy `make_task`/`flow_step` from `orchestrator_test.rs:95-130` verbatim):

```rust
fn make_task(flow: HashMap<String, FlowStep>) -> TaskDef {
    TaskDef {
        name: None,
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

fn flow_step(depends_on: Vec<&str>) -> FlowStep {
    FlowStep {
        action: "noop".to_string(),
        name: None,
        description: None,
        depends_on: depends_on.into_iter().map(str::to_string).collect(),
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

#[tokio::test]
async fn execute_empty_plan_touches_nothing() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "a", "running")]).await?;
    let task = make_task(HashMap::from([("a".to_string(), flow_step(vec![]))]));
    let plan = execute(&pool, job_id, &task, Some(&WorkspaceConfig::new())).await?;
    assert!(plan.changes.is_empty());
    Ok(())
}

/// Guard-miss re-run through `execute`: a stale plan is never applied.
#[tokio::test]
async fn execute_reruns_after_a_guard_miss() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "a", "completed"), step(job_id, "b", "pending")]).await?;
    let task = make_task(HashMap::from([
        ("a".to_string(), flow_step(vec![])),
        ("b".to_string(), flow_step(vec!["a"])),
    ]));
    // Prove the plan is non-empty on this snapshot, then invalidate it.
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let stale = stroem_server::cascade::run(&task, &job, &steps, Some(&WorkspaceConfig::new()))?;
    assert_eq!(stale.changes, vec![Change::Promote { step: "b".into() }]);
    JobStepRepo::cancel_pending_steps(&pool, job_id).await?;

    let plan = execute(&pool, job_id, &task, Some(&WorkspaceConfig::new())).await?;
    assert!(plan.changes.is_empty(), "the re-run sees b cancelled and plans nothing");
    assert_eq!(step_statuses(&pool, job_id).await["b"], "cancelled");
    Ok(())
}

/// Two concurrent executes for the same job: both return, the join is promoted
/// exactly once, final state equals a serial run. Smoke test of the re-run path.
#[tokio::test]
async fn execute_concurrently_promotes_join_once() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(job_id, "l", "completed"), step(job_id, "r", "completed"), step(job_id, "join", "pending")],
    )
    .await?;
    let task = make_task(HashMap::from([
        ("l".to_string(), flow_step(vec![])),
        ("r".to_string(), flow_step(vec![])),
        ("join".to_string(), flow_step(vec!["l", "r"])),
    ]));
    let ws = WorkspaceConfig::new();
    let (p1, p2) = tokio::join!(
        execute(&pool, job_id, &task, Some(&ws)),
        execute(&pool, job_id, &task, Some(&ws)),
    );
    let (p1, p2) = (p1?, p2?);
    let promoted = p1.changes.len() + p2.changes.len();
    assert_eq!(promoted, 1, "exactly one of them applied the promotion");
    assert_eq!(step_statuses(&pool, job_id).await["join"], "ready");
    Ok(())
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p stroem-server --test cascade_apply_test execute`
Expected: compile error, `execute` not found.

- [ ] **Step 3: Implement `execute`**

Add to `crates/stroem-server/src/cascade.rs` (import `anyhow::{bail, Context}` and `sqlx::PgPool`):

```rust
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
    bail!("cascade guard miss {} times for job {}", MAX_ATTEMPTS, job_id)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p stroem-server --test cascade_apply_test`
Expected: all pass (the concurrent test may take a second; both `execute`s must return `Ok`).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/cascade.rs crates/stroem-server/tests/cascade_apply_test.rs
git commit -m "feat(cascade): execute — single-transaction apply with guard-miss and deadlock re-run"
```

---

### Task 4: Activation — switch both callers, delete the four old functions

**Files:**
- Modify: `crates/stroem-server/src/orchestrator.rs:15-117` (`on_step_completed` body)
- Modify: `crates/stroem-server/src/job_creator.rs:618-641` (init loop), delete `expand_for_each_steps` (`:944-1152`) and `check_loop_completion` (`:1216-1353`); move `MAX_FOR_EACH_ITEMS`, `parse_for_each_items`, `render_for_each_template` and their tests (`:1965-2060`) into `cascade.rs`
- Modify: `crates/stroem-server/src/job_recovery.rs:223-238` and `:599-614` (delete the two `check_loop_completion` calls)
- Modify: `crates/stroem-db/src/repos/job_step.rs` delete `promote_ready_steps` (`:672-826`) and `skip_unreachable_steps` (`:855-916`)
- Modify: `crates/stroem-db/tests/integration_test.rs` delete `test_promote_ready_steps` (`:531-760`); `crates/stroem-db/tests/job_step_status_tests.rs` delete `test_skip_unreachable_steps_skips_blocked_pending` (`:531`), `test_skip_unreachable_steps_respects_continue_on_failure` (`:596`), `test_skip_unreachable_steps_does_not_block_on_skipped_dep` (`:852`)
- Modify: `crates/stroem-server/tests/orchestrator_test.rs:990-1050` rewrite `test_convergence_without_continue_on_failure`

**Interfaces:**
- Consumes: `cascade::execute` (Task 3).
- Produces: nothing new. After this task the words `promote_ready_steps`, `skip_unreachable_steps`, `expand_for_each_steps`, `check_loop_completion` appear nowhere in the workspace.

- [ ] **Step 1: Baseline the oracle**

Run: `cargo test -p stroem-server --test orchestrator_test` and `cargo test -p stroem-server --test integration_test cascad` and `cargo test -p stroem-server --test integration_test for_each` and `cargo test -p stroem-db`
Expected: all green. Note the counts.

- [ ] **Step 2: `orchestrator::on_step_completed`**

Replace the body of `on_step_completed` (from `tracing::info!("Orchestrating after step ...` through `settle_if_all_terminal(pool, job_id, task).await?;`) with:

```rust
    tracing::info!("Orchestrating after step '{}' completed", step_name);

    // 1. Move every step the cascade can move: rollup/advance loops, promote,
    //    cascade-skip, retire/expand placeholders — one pure fixpoint, one
    //    transaction (see cascade.rs).
    crate::cascade::execute(pool, job_id, task, workspace_config)
        .await
        .context("Failed to run step cascade")?;

    // 2. Settle the job if every step is terminal.
    settle_if_all_terminal(pool, job_id, task).await?;
    Ok(())
```

Delete the now-unused `job_row` fetch and the `TODO(optimize)` comment. Remove `JobStepRepo` from the file's imports if it becomes unused (it is still used by `settle_if_all_terminal`; keep it if so).

- [ ] **Step 3: creation-time init block (`job_creator.rs:618-641`)**

Replace from `let job_row = JobRepo::get(pool, job_id).await?.context("Job not found")?;` through the end of the `for _iteration` loop with:

```rust
            // Promote/skip/expand root steps. Runs unconditionally: cheap when
            // nothing is promotable, and required for Plan B's seeded jobs.
            crate::cascade::execute(pool, job_id, task, Some(workspace_config))
                .await
                .context("creation-time step cascade")?;
```

- [ ] **Step 4: Remove the two `check_loop_completion` calls**

In `job_recovery.rs`, delete lines 223-238 (the `// Check if this is a loop instance completing` comment through the closing brace of `if let Err(e) = crate::job_creator::check_loop_completion(...)`), and lines 599-614 (the parent-step equivalent inside `propagate_to_parent`). The `on_step_completed` calls that follow each stay.

- [ ] **Step 5: Move the parser helpers and delete the four functions**

1. Cut `pub(crate) const MAX_FOR_EACH_ITEMS`, `pub(crate) fn parse_for_each_items`, `pub(crate) fn render_for_each_template` from `job_creator.rs` and paste them into `cascade.rs` above `Snapshot` (drop the `pub(crate)` on the const; keep it on the two fns). Update the `use crate::job_creator::{...}` line in `cascade.rs` to import only `build_step_render_context`.
2. Cut the parser tests from `job_creator.rs`'s test module (the block from the comment `// parse_for_each_items is private, but accessible within #[cfg(test)]...` at `:1965` to the end of the last `parse_for_each_items` / `render_for_each_template` test, `:2060`) and paste them into `cascade.rs`'s test module.
3. Delete `pub async fn expand_for_each_steps` (`job_creator.rs:944-1152`) and `pub async fn check_loop_completion` (`:1216-1353`) with their doc comments.
4. Delete `pub async fn promote_ready_steps` (`job_step.rs:672-826`, including its TODO comments at `:678-690`) and `pub async fn skip_unreachable_steps` (`:855-916`). If `FlowStep` and `HashMap` become unused imports in `job_step.rs`, remove them; if `stroem_common::template` is no longer referenced, that import goes too.
5. Delete the four DB tests named above.

Run: `grep -rn "promote_ready_steps\|skip_unreachable_steps\|expand_for_each_steps\|check_loop_completion" crates/ docs/src CLAUDE.md`
Expected: hits only in `CLAUDE.md`, `crates/stroem-db/README.md` and comments (Task 7 fixes those); none in Rust.

- [ ] **Step 6: Rewrite `test_convergence_without_continue_on_failure`**

In `crates/stroem-server/tests/orchestrator_test.rs`, the test (starting ~`:990`) currently marks `a` completed, builds `let ctx = json!({"input": {"use_fast": true}});` and calls `JobStepRepo::promote_ready_steps(&pool, job_id, &task.flow, Some(&ctx))`. Replace from `let ws = WorkspaceConfig::new();` through the two `assert!(changed.contains(...))` lines with:

```rust
    let ws = WorkspaceConfig::new();

    // A completes — the job's stored input carries use_fast = true
    JobStepRepo::mark_completed(&pool, job_id, "a", None).await?;
    sqlx::query("UPDATE job SET input = '{\"use_fast\": true}'::jsonb WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;
    let plan = stroem_server::cascade::execute(&pool, job_id, &task, Some(&ws)).await?;
    assert!(
        plan.changes.contains(&stroem_server::cascade::Change::Promote { step: "b".to_string() }),
        "B should be promoted"
    );
    assert!(
        plan.changes.contains(&stroem_server::cascade::Change::Skip { step: "c".to_string() }),
        "C should be skipped"
    );
```

Keep the status assertions that follow. Add `use stroem_server::cascade::Change;` if you prefer the short path.

- [ ] **Step 7: Build, lint, run the oracle**

Run: `cargo build --workspace && cargo clippy --workspace -- -D warnings`
Expected: clean.

Run: `cargo test -p stroem-server --test orchestrator_test && cargo test -p stroem-db && cargo test -p stroem-server --lib`
Expected: every test that passed in Step 1 passes, minus the four deleted DB tests; `test_convergence_without_continue_on_failure` passes.

Run: `cargo test -p stroem-server --test integration_test` (full file, this takes a while)
Expected: green. Pay attention to `test_create_job_for_task_root_when_false_skips_at_creation`, `test_task_step_dispatch_failure_cascades_and_fails_job`, `test_all_skipped_job_at_creation_fires_workspace_hook`, and every `for_each` / `sequential` test.

Run: `cargo test -p stroem-server --test restart_integration_test && cargo test -p stroem-server --test propagate_to_parent_test && cargo test -p stroem-server --test rerun_integration_test && cargo test -p stroem-server --test metrics_test`
Expected: green.

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
git add -A crates/
git commit -m "refactor(cascade): activate cascade::execute in on_step_completed and job creation; delete promote_ready_steps, skip_unreachable_steps, expand_for_each_steps, check_loop_completion"
```

---

### Task 5: Regression tests for the fixes that went live in Task 4

**Files:**
- Test: `crates/stroem-server/tests/orchestrator_test.rs` (append; uses its `setup_db`, `create_job`, `make_task`, `flow_step`, `step`, `step_statuses`, `on_step_completed`)

Add these helpers next to `step_when`:

```rust
fn step_for_each(job_id: Uuid, name: &str, status: &str, expr: &str) -> NewJobStep {
    NewJobStep { for_each_expr: Some(expr.to_string()), ..step(job_id, name, status) }
}
fn step_instance(job_id: Uuid, source: &str, i: i32, status: &str) -> NewJobStep {
    NewJobStep {
        loop_source: Some(source.to_string()),
        loop_index: Some(i),
        loop_total: Some(3),
        loop_item: Some(json!(i)),
        ..step(job_id, &format!("{source}[{i}]"), status)
    }
}
fn flow_step_seq(depends_on: Vec<&str>) -> FlowStep {
    FlowStep { sequential: true, ..flow_step(depends_on) }
}
```

- [ ] **Step 1: Self-healing rollup (fix 2)**

```rust
/// A running placeholder whose instances are all terminal but which no keyed
/// call ever rolled up is rolled up by the next cascade on the job.
#[tokio::test]
async fn test_self_healing_rollup_on_unrelated_cascade() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step_for_each(job_id, "x", "running", "[1,2]"),
            step_instance(job_id, "x", 0, "completed"),
            step_instance(job_id, "x", 1, "completed"),
            step(job_id, "other", "completed"),
        ],
    )
    .await?;
    let task = make_task(HashMap::from([
        ("x".to_string(), flow_step(vec![])),
        ("other".to_string(), flow_step(vec![])),
    ]));
    on_step_completed(&pool, job_id, "other", &task, Some(&WorkspaceConfig::new())).await?;
    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["x"], "completed", "rolled up although no instance just completed");
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    Ok(())
}
```

- [ ] **Step 2: Timeout vs rollup and cancelled placeholder (fix 4)**

```rust
/// A placeholder failed by recovery (timeout) is never overwritten by a later
/// instance completion's rollup.
#[tokio::test]
async fn test_rollup_never_overwrites_failed_or_cancelled_placeholder() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    for terminal in ["failed", "cancelled"] {
        let job_id = create_job(&pool).await;
        JobStepRepo::create_steps(
            &pool,
            &[
                step_for_each(job_id, "x", terminal, "[1]"),
                step_instance(job_id, "x", 0, "completed"),
            ],
        )
        .await?;
        let task = make_task(HashMap::from([("x".to_string(), flow_step(vec![]))]));
        on_step_completed(&pool, job_id, "x[0]", &task, Some(&WorkspaceConfig::new())).await?;
        assert_eq!(step_statuses(&pool, job_id).await["x"], terminal, "{terminal} placeholder untouched");
    }
    Ok(())
}
```

- [ ] **Step 3: R5 behavioural change**

```rust
/// [failed, completed, pending]: the loop stops at the first failure; [2] is
/// skipped and never promoted (spec §4.3, deliberate change from today).
#[tokio::test]
async fn test_sequential_failure_skips_later_pending_instances() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step_for_each(job_id, "x", "running", "[1,2,3]"),
            step_instance(job_id, "x", 0, "failed"),
            step_instance(job_id, "x", 1, "completed"),
            step_instance(job_id, "x", 2, "pending"),
        ],
    )
    .await?;
    let task = make_task(HashMap::from([("x".to_string(), flow_step_seq(vec![]))]));
    on_step_completed(&pool, job_id, "x[1]", &task, Some(&WorkspaceConfig::new())).await?;
    let s = step_statuses(&pool, job_id).await;
    assert_eq!(s["x[2]"], "skipped");
    assert_eq!(s["x"], "failed");
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");
    Ok(())
}
```

- [ ] **Step 4: `on_step_completed` vs creation equivalence, and parent/child**

```rust
/// The same DAG cascaded once at creation and once via on_step_completed ends in
/// the same statuses (both callers go through cascade::execute).
#[tokio::test]
async fn test_creation_and_orchestrator_cascades_agree() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let task = make_task(HashMap::from([
        ("a".to_string(), flow_step(vec![])),
        ("b".to_string(), flow_step_when(vec!["a"], "false")),
        ("c".to_string(), flow_step(vec!["b"])),
        ("d".to_string(), flow_step_cof(vec!["b"])),
    ]));
    let expected = |s: &HashMap<String, String>| {
        assert_eq!(s["b"], "skipped");
        assert_eq!(s["c"], "skipped", "all deps skipped → cascade-skip");
        assert_eq!(s["d"], "ready", "cof survives a skipped dep");
    };
    // via the orchestrator
    let j1 = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(j1, "a", "completed"), step_when(j1, "b", "pending", "false"), step(j1, "c", "pending"), step(j1, "d", "pending")],
    )
    .await?;
    on_step_completed(&pool, j1, "a", &task, Some(&WorkspaceConfig::new())).await?;
    expected(&step_statuses(&pool, j1).await);
    // via execute directly on an identical snapshot (what creation calls)
    let j2 = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(j2, "a", "completed"), step_when(j2, "b", "pending", "false"), step(j2, "c", "pending"), step(j2, "d", "pending")],
    )
    .await?;
    stroem_server::cascade::execute(&pool, j2, &task, Some(&WorkspaceConfig::new())).await?;
    expected(&step_statuses(&pool, j2).await);
    Ok(())
}

/// A child job's cascade and its parent's cascade run concurrently; both complete.
#[tokio::test]
async fn test_parent_and_child_cascades_run_concurrently() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let parent = create_job(&pool).await;
    let child = JobRepo::create_with_parent(
        &pool, "default", "child-task", "distributed", None, "task", None, Some(parent), Some("spawn"), None, None,
    )
    .await?;
    JobStepRepo::create_steps(&pool, &[step(parent, "spawn", "running"), step(parent, "after", "pending")]).await?;
    JobStepRepo::create_steps(&pool, &[step(child, "c1", "completed"), step(child, "c2", "pending")]).await?;
    let ptask = make_task(HashMap::from([
        ("spawn".to_string(), flow_step(vec![])),
        ("after".to_string(), flow_step(vec!["spawn"])),
    ]));
    let ctask = make_task(HashMap::from([
        ("c1".to_string(), flow_step(vec![])),
        ("c2".to_string(), flow_step(vec!["c1"])),
    ]));
    let ws = WorkspaceConfig::new();
    let (a, b) = tokio::join!(
        on_step_completed(&pool, parent, "spawn", &ptask, Some(&ws)),
        on_step_completed(&pool, child, "c1", &ctask, Some(&ws)),
    );
    a?;
    b?;
    assert_eq!(step_statuses(&pool, child).await["c2"], "ready");
    assert_eq!(step_statuses(&pool, parent).await["after"], "pending", "spawn still running");
    Ok(())
}
```

If `JobRepo::create_with_parent`'s argument list differs from the one above, copy the call from `propagate_to_parent_test.rs` (grep `create_with_parent(`).

- [ ] **Step 5: Run and commit**

Run: `cargo test -p stroem-server --test orchestrator_test`
Expected: all pass.

```bash
cargo fmt --all
git add crates/stroem-server/tests/orchestrator_test.rs
git commit -m "test(cascade): self-healing rollup, guarded placeholder writes, sequential failure precedence, caller equivalence, parent/child"
```

---

### Task 6: Adoption (R0, fix 5)

**Files:**
- Modify: `crates/stroem-server/src/cascade.rs` (`phase_placeholders` guard → `Adopt`; two unit tests)
- Test: `crates/stroem-server/tests/orchestrator_test.rs` (append)

- [ ] **Step 1: Failing unit tests**

Add to `cascade.rs` tests:

```rust
    #[test]
    fn pending_placeholder_with_existing_instances_is_adopted() {
        let t = task(vec![("p", fs(&[]))]);
        let rows = vec![
            placeholder("p", "pending", "[1,2]"),
            instance("p", 0, "completed", Some(json!(1))),
            instance("p", 1, "completed", Some(json!(2))),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["adopt:p", "rollup-ok:p"], "adopted in P3, rolled up in the next pass");
    }

    #[test]
    fn adoption_needs_a_workspace_config_like_expansion() {
        let t = task(vec![("p", fs(&[]))]);
        let rows = vec![placeholder("p", "pending", "[1]"), instance("p", 0, "completed", None)];
        let plan = run(&t, &job(None), &rows, None).unwrap();
        assert!(plan.changes.is_empty());
    }
```

Run: `cargo test -p stroem-server --lib adopt`; expected: `pending_placeholder_with_existing_instances_is_adopted` fails (plan is empty).

- [ ] **Step 2: Implement R0**

In `phase_placeholders`, replace the idempotency guard block with:

```rust
        // R0: instances already exist (a past crash between insert and the
        // placeholder transition) → adopt the placeholder instead of leaving it
        // pending forever. Never re-expands.
        if snap.status(&format!("{}[0]", r.step_name)).is_some() {
            out.push(Change::Adopt { placeholder: r.step_name.clone() });
            continue;
        }
```

Run: `cargo test -p stroem-server --lib cascade`; expected: all pass.

- [ ] **Step 3: Integration test**

Append to `orchestrator_test.rs`:

```rust
/// Placeholders stranded by the pre-cascade crash hole (instances inserted, placeholder
/// still pending) are adopted and roll up normally.
#[tokio::test]
async fn test_adopts_partially_expanded_placeholder() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step_for_each(job_id, "x", "pending", "[1,2]"),
            step_instance(job_id, "x", 0, "completed"),
            step_instance(job_id, "x", 1, "completed"),
        ],
    )
    .await?;
    let task = make_task(HashMap::from([("x".to_string(), flow_step(vec![]))]));
    on_step_completed(&pool, job_id, "x[1]", &task, Some(&WorkspaceConfig::new())).await?;
    let s = step_statuses(&pool, job_id).await;
    assert_eq!(s["x"], "completed");
    assert_eq!(JobStepRepo::get_steps_for_job(&pool, job_id).await?.len(), 3, "no re-expansion");
    Ok(())
}
```

Run: `cargo test -p stroem-server --test orchestrator_test adopts`; expected: pass.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add crates/stroem-server/src/cascade.rs crates/stroem-server/tests/orchestrator_test.rs
git commit -m "feat(cascade): adopt partially expanded placeholders (R0)"
```

---

### Task 7: Documentation

**Files:**
- Create: `CONTEXT.md`
- Modify: `CLAUDE.md` (overview link; new `### Step Cascade`; edits under Conditional Flow Steps, For-Each Loops, Task Actions)
- Modify: `crates/stroem-db/README.md:86` (remove the orchestration API paragraph)
- Modify: `docs/internal/TODO.md`
- Modify: `docs/src/content/docs/guides/*` — the `for_each` guide (grep `sequential` under `docs/src/content/docs`)

- [ ] **Step 1: `CONTEXT.md`**

```markdown
# Strøm — Domain Vocabulary

Terms used in code, specs and CLAUDE.md. CLAUDE.md holds how things work; this file
holds what the words mean. Keep both in sync when a term is added or sharpened.

- **Job** — one execution of a task; owns a set of step rows.
- **Step** — one row of a job; moves `pending → ready → claimed → running → {completed, failed, skipped, cancelled}` (or `suspended` for approval gates).
- **Step cascade** — the fixpoint that moves a job's non-running steps after a change: rollup and sequential advance of loops, promotion, cascade-skip, skip-unreachable, placeholder retirement and expansion. Implemented once in `crates/stroem-server/src/cascade.rs` (`run` pure, `apply` in one transaction, `execute` the entry point).
- **Placeholder** — the `for_each` step row that stands in for its instances; `for_each_expr` is set.
- **Instance** — a row created by expanding a placeholder: `name[i]`, `loop_source = name`, `loop_index = i`.
- **Retirement** — a placeholder leaving `pending` without expanding (skipped or failed).
- **Adoption** — a `pending` placeholder whose instances already exist being moved to `running` without re-expanding.
- **Rollup** — a `running` placeholder becoming `completed` (array of instance outputs) or `failed` once every instance is terminal.
- **Plan** — the ordered list of changes one cascade `run` produced.
- **Change** — one state transition the cascade wants applied (`Promote`, `Skip`, `Fail`, `Expand`, `Adopt`, `Rollup`).
- **Guard miss** — an apply statement that affected fewer rows than the plan named; the plan is rolled back and re-run.
- **Settlement** — deciding a job's terminal status once every step is terminal (`orchestrator::settle_if_all_terminal`); the subject of a later design.
- **Terminal handling** — the once-only side effects after settlement: propagation to the parent, hooks, metrics, log archive.
```

- [ ] **Step 2: CLAUDE.md**

Under `## Project Overview`, after the first paragraph, add: `Domain vocabulary lives in `CONTEXT.md`; read it alongside this file.`

Under `## Key Patterns`, before `### Conditional Flow Steps`, add:

```markdown
### Step Cascade
- `crates/stroem-server/src/cascade.rs` — one pure fixpoint replaces the old promote→skip→expand loops and the keyed loop rollup. `run(task, job, steps, workspace_config) -> Plan` never touches the DB; `apply(conn, job_id, plan)` composes stroem-db `_tx` primitives with affected-row-count checks; `execute(pool, ...)` reads, runs, applies in ONE transaction and re-runs on a guard miss (max 3).
- **Phase order per pass is part of the interface**: P0 rollup/advance (R5/R6) → context A → P1 cascade-skip + promote with `when` (R1/R2) → P2 skip-unreachable (R3) → context B → P3 adopt + retire/expand (R0/R4). A `when` in P1 does not see a skip from the same phase until the next pass; expansion in P3 does. Do not reorder.
- Callers: `orchestrator::on_step_completed` (worker completion, approval, recovery, propagation) and the creation-time init block in `job_creator.rs`. Both call `execute` then continue as before. `check_loop_completion`, `expand_for_each_steps`, `promote_ready_steps`, `skip_unreachable_steps` no longer exist.
- `execute` commits before returning; settlement, task/approval dispatch and propagation run after it, outside any transaction. Never wrap `execute` in a larger transaction.
- Guards: `Promote`/`Skip`/`Fail` require `pending`; `Expand`/`Adopt` require the placeholder `pending`; `Rollup` requires `running` (so a cancelled or timed-out placeholder is never overwritten). The R7 job-row `UPDATE` may match zero rows. A `Fail` for a placeholder is guarded where the old code was not.
- `run` renders templates (which may call the `vals` subprocess); that is why it runs before the apply transaction opens.
- Known, unchanged from before the cascade: two cascades on one job can race (lost work, `job_step.rs` TODO history); a same-status `output`/`error_message` rewrite between snapshot and apply is not detected; `try_retry_job`'s transaction (job row then steps) inverts the cascade's order (steps then job row). All three are closed by `docs/superpowers/specs/2026-09-08-cascade-concurrency-hardening-design.md`.
- New context variables in `build_step_render_context` must be inserted BEFORE completed-step outputs (a step named `job` shadows `job`).
```

Under `### Conditional Flow Steps`, replace the truthiness line with: `Truthy if non-empty and, after trim and lowercase, not "false", "0", "null" or "none".` and replace `Condition-false → skipped` etc. only if wording conflicts; add `Evaluated in cascade phase P1 (see Step Cascade).`

Under `### For-Each Loops`, replace the two bullets **Failed/cancelled dependency** and **Placeholder resolution lives inside the orchestrator cascade** with:

```markdown
- **Placeholder lifecycle has one owner**: `cascade.rs` rules R0 (adopt), R4 (retire/expand), R5 (sequential advance), R6 (rollup). A placeholder whose dependency failed or was cancelled (without `continue_on_failure`) is retired `skipped` by R4; rollup and sequential advance are global rules re-evaluated on every cascade of the job (self-healing), not keyed on the completing instance.
- **Sequential failure stops the loop immediately**: any `failed`/`cancelled` instance without `continue_on_failure` skips every pending instance, even a successor of a later completed instance (`[failed, completed, pending]` → `[2]` skipped).
- Only `failed` instances fail a loop; `cancelled` instances count as terminal but do not. Rollup output has one element per existing instance, `null` where the instance produced none.
```

Under `### Task Actions`, in the **Dispatch failures re-orchestrate** bullet, leave as is (still true). Nothing else references the deleted functions; run `grep -n "expand_for_each_steps\|check_loop_completion\|promote_ready_steps\|skip_unreachable_steps" CLAUDE.md` and fix any leftover.

- [ ] **Step 3: DB README, TODO.md, user guide**

- `crates/stroem-db/README.md` around line 86: delete the sentences advertising `promote_ready_steps` / `skip_unreachable_steps` as the orchestration API; replace with `Step orchestration lives in the server crate (`cascade.rs`); this crate exposes only the transaction primitives it applies.`
- `docs/internal/TODO.md`: close any item about the double `get_steps_for_job` fetch or the TOCTOU in `promote_ready_steps` (grep `promote_ready_steps`, `TOCTOU`); add under Architecture:
  ```markdown
  - [ ] Cascade concurrency hardening (advisory lock, row-locked verification, cancellation/task-retry lock order) — spec `docs/superpowers/specs/2026-09-08-cascade-concurrency-hardening-design.md`.
  - [ ] Remove the `Option<&WorkspaceConfig>` mode from `on_step_completed` (test-only; ~80 call sites pass `None`) — settlement branch.
  - [ ] Move `build_step_render_context` out of `job_creator.rs` (render-context candidate); `cascade.rs` and `job_creator.rs` currently reference each other.
  - [ ] Recovery's `mark_failed` on timed-out steps is unguarded and can overwrite a `completed` row.
  ```
- In the `for_each` user guide, under the `sequential` description, add one sentence: `If an instance fails (and `continue_on_failure` is not set) every remaining instance is skipped immediately and the loop step fails.`

- [ ] **Step 4: Commit**

```bash
git add CONTEXT.md CLAUDE.md crates/stroem-db/README.md docs/internal/TODO.md docs/src/content/docs
git commit -m "docs: CONTEXT.md vocabulary, Step Cascade section, for_each guide, TODO follow-ups"
```

---

### Task 8: Prune container tests that now have a `run` twin

**Files:**
- Modify: `crates/stroem-server/tests/orchestrator_test.rs`

- [ ] **Step 1: Identify candidates**

Prune only tests whose **every** assertion is on step statuses after one `on_step_completed` and which have a twin in `cascade::tests` (Task 1). Keep every test that also asserts job status, job output, timestamps, or "no settlement while live" (`:371`, `:1724`, `:1753`, `:1772`) and every test added in Tasks 5–6. Candidates by name (verify each before deleting): `test_linear_dag_step_promotion`, `test_cascading_skip_multi_level`, `test_conditional_step_skipped_when_condition_false`, `test_conditional_skip_cascades_to_downstream`, `test_sibling_when_branches_truthy_and_falsy_evaluated_together`, `test_when_condition_error_marks_step_failed`, `test_all_deps_skipped_cascade_skip`, `test_truthy_when_overridden_by_all_deps_skipped_cascade`, `test_mixed_skipped_and_failed_dep_blocks_without_cof`, `test_cancelled_dep_with_cof_promotes_step`, `test_diamond_dag_join_waits_for_both_branches`.

- [ ] **Step 2: Delete them, run the file, commit**

Run: `cargo test -p stroem-server --test orchestrator_test`
Expected: green.

```bash
git add crates/stroem-server/tests/orchestrator_test.rs
git commit -m "test(cascade): prune container tests fully covered by cascade::run unit twins"
```

---

## Self-review

- **Spec coverage.** §4.2 interface: Task 1 (`Change`, `RollupOutcome`, `Plan`, `run`), Task 2 (`Applied`, `ApplyError`, `apply`), Task 3 (`execute`). §4.3 R1–R7: Task 1 phases (R0 in Task 6). §4.4 phase order and termination bound: Task 1 `run`. §4.5 context built twice per pass: Task 1. §4.6 guards and row-count policy, Expand order, R7 exemption: Task 2. §4.7 execute, guard-miss re-run, 40P01: Task 3. §4.8 callers: Task 4 steps 2–4, test rewrite step 6. §4.10 fixes: 1 Task 2 tests, 2 Task 5 step 1, 3 Task 2/3 tests, 4 Task 5 step 2, 5 Task 6. §6.2 deletions: Task 4 step 5. §7 docs: Task 7. §9.1: Task 1 tests (every bullet has at least one test; adoption in Task 6). §9.2: Tasks 2, 3, 5, 6. §10 commit order: Tasks 1–8 map to commits 1–8.
- **Placeholders.** None. The one deferred behaviour (R0) is explicit code in Task 1 with the Task 6 replacement shown.
- **Type consistency.** `Change` variant field names (`step`, `placeholder`, `instances`, `outcome`, `error`) identical across Tasks 1, 2, 4, 6. `apply(conn: &mut sqlx::PgConnection, ...)` called with `&mut tx` (deref coercion from `Transaction`) in Tasks 2 and 3. Primitive names match between the stroem-db additions and `apply`. `is_terminal` is `pub(crate)` and used only inside `cascade.rs`.
