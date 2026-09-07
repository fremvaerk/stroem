# Restart From Step — Implementation Plan (Plan B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `POST /api/jobs/{id}/restart {from_step}` creates a new job that carries over every step outside the restart set with its source status/output and reruns the chosen step plus its transitive dependents; the UI exposes it per step.

**Architecture:** A pure `compute_restart_set` produces a `RestartPlan` (used by both `dry_run` and the real run). The creator gains a typed `CreationMode` (`Normal | Rerun | Restart`); in `Restart` mode the creation transaction seeds carried rows via `JobStepRepo::seed_steps_tx`, then the normal post-commit cascade + shared settlement (Plan A) close the job if nothing is left to run. New column `job_step.carried_over` drives the UI. Hooks flag carried failures; stats exclude restart jobs.

**Tech Stack:** Rust (axum, sqlx), Postgres via testcontainers; React 19 + TypeScript + Vitest for the UI.

**Spec:** `docs/superpowers/specs/2026-09-07-restart-from-step-design.md` (rev 2). **Depends on Plan A** (`2026-09-07-creation-settlement-unification.md`) being merged: `CreatedJob`, `settle_if_all_terminal`, `finalize_created_job`, `is_top_level_source`.

## Global Constraints

- Same as Plan A (anyhow/tracing/sqlx runtime queries; fmt + clippy `-D warnings`; Docker env for tests; commit trailer; no wire-format change for workers).
- Migration number: **045** (`044_claim_index_agent.sql` already exists).
- `restart` semantics are exactly spec §4; when in doubt the spec wins over this plan.
- UI package manager is `bun`; tests are Vitest + Testing Library (`cd ui && bun run test`).

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/stroem-db/migrations/045_job_step_carried_over.sql` | `carried_over BOOLEAN NOT NULL DEFAULT FALSE` |
| `crates/stroem-db/src/repos/job_step.rs` | `STEP_COLUMNS`/`JobStepRow.carried_over`; `seed_steps_tx`; stats CTE excludes restart jobs |
| `crates/stroem-db/src/repos/job.rs` | duration stats exclude `source_type = 'restart'` |
| `crates/stroem-server/src/restart.rs` (new) | `Seed`, `RestartPlan`, `compute_restart_set` — pure, unit-tested |
| `crates/stroem-server/src/job_creator.rs` | `CreationMode`; inner takes it; `create_restart_job` |
| `crates/stroem-server/src/web/api/jobs.rs` | `restart_job` handler (+ `dry_run`); step JSON gains `carried_over` |
| `crates/stroem-server/src/web/api/mod.rs` | route; `classify_execute_error` moved here as `pub(crate)` |
| `crates/stroem-server/src/hooks.rs` | `FailedStepInfo.carried_over` |
| `crates/stroem-server/tests/restart_integration_test.rs` (new) | end-to-end tests |
| `ui/src/lib/types.ts`, `ui/src/lib/api.ts` | `carried_over`, `restartJob` |
| `ui/src/components/restart-dialog.tsx` (new) | confirm dialog fed by `dry_run` |
| `ui/src/components/step-detail.tsx`, `step-timeline.tsx`, `ui/src/pages/job-detail.tsx`, `ui/src/lib/eta.ts` | button placement, badge, lineage, ETA |
| docs | guides, API reference, CLAUDE.md, TODO |

---

### Task 1: Migration 045 + `carried_over` on `JobStepRow` + API step JSON + `seed_steps_tx`

**Files:**
- Create: `crates/stroem-db/migrations/045_job_step_carried_over.sql`
- Modify: `crates/stroem-db/src/repos/job_step.rs:10` (`STEP_COLUMNS`), `:13-58` (`JobStepRow`), add `seed_steps_tx` after `create_steps_tx`
- Modify: `crates/stroem-server/src/web/api/jobs.rs:309-335` (step JSON)
- Modify: `ui/src/lib/types.ts:84-115` (`JobStep.carried_over: boolean`) and every `makeStep`/fixture in `ui/src/**/__tests__` that builds a `JobStep` (add `carried_over: false`)
- Test: `crates/stroem-db/tests/integration_test.rs` (append)

**Interfaces:**
- Produces: `pub struct Seed { pub step_name: String, pub status: String, pub output: Option<JsonValue>, pub error_message: Option<String> }` in `stroem_db::repos::job_step` (re-exported from `stroem_db`).
- Produces: `JobStepRepo::seed_steps_tx<'e, E: Executor>(executor: E, job_id: Uuid, seeds: &[Seed]) -> Result<()>` — one UPDATE per seed; bails if a seed matched ≠ 1 row.
- `JobStepRow.carried_over: bool`; API step object `"carried_over": bool`.

- [ ] **Step 1: Write the failing test**

```rust
// ─── Plan B / Task 1: seeding carried-over rows ──────────────────────────────

#[tokio::test]
async fn test_seed_steps_tx_overwrites_status_output_and_flags_row() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = JobRepo::create(&pool, "default", "t", "distributed", None, "api", None, None, None).await?;
    JobStepRepo::create_steps(&pool, &[plain_step(job_id, "a", "ready"), plain_step(job_id, "b", "pending")]).await?;

    let mut tx = pool.begin().await?;
    JobStepRepo::seed_steps_tx(
        &mut *tx,
        job_id,
        &[
            Seed { step_name: "a".into(), status: "completed".into(), output: Some(json!({"k": 1})), error_message: None },
            Seed { step_name: "b".into(), status: "failed".into(), output: None, error_message: Some("old boom".into()) },
        ],
    )
    .await?;
    tx.commit().await?;

    let by: std::collections::HashMap<_, _> = JobStepRepo::get_steps_for_job(&pool, job_id).await?
        .into_iter().map(|s| (s.step_name.clone(), s)).collect();
    assert_eq!(by["a"].status, "completed");
    assert_eq!(by["a"].output.as_ref().unwrap()["k"], 1);
    assert!(by["a"].carried_over);
    assert!(by["a"].ready_at.is_none(), "ready_at cleared on a seeded root row");
    assert!(by["a"].completed_at.is_some());
    assert_eq!(by["b"].status, "failed");
    assert_eq!(by["b"].error_message.as_deref(), Some("old boom"));
    assert!(by["b"].carried_over);
    Ok(())
}

#[tokio::test]
async fn test_seed_steps_tx_unknown_step_aborts() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = JobRepo::create(&pool, "default", "t", "distributed", None, "api", None, None, None).await?;
    JobStepRepo::create_steps(&pool, &[plain_step(job_id, "a", "ready")]).await?;
    let mut tx = pool.begin().await?;
    let err = JobStepRepo::seed_steps_tx(
        &mut *tx, job_id,
        &[Seed { step_name: "ghost".into(), status: "completed".into(), output: None, error_message: None }],
    ).await.unwrap_err();
    assert!(format!("{err:#}").contains("ghost"), "{err:#}");
    Ok(())
}
```

`JobStepRow` has no `ready_at` field today — check `STEP_COLUMNS`; if absent, drop the `ready_at` assertion and instead assert via raw SQL: `sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>("SELECT ready_at FROM job_step WHERE job_id=$1 AND step_name='a'").bind(job_id).fetch_one(&pool).await?.is_none()`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-db --test integration_test seed_steps_tx`
Expected: compile error (`Seed`, `seed_steps_tx`, `carried_over` missing).

- [ ] **Step 3: Implement**

Migration `045_job_step_carried_over.sql`:
```sql
-- Restart From Step (spec 2026-09-07 §5): marks rows copied from a source job
-- (status/output carried over, never executed in this job). The source job
-- itself is job.source_job_id.
ALTER TABLE job_step ADD COLUMN carried_over BOOLEAN NOT NULL DEFAULT FALSE;
```

`job_step.rs`: append `, carried_over` to `STEP_COLUMNS`; add `pub carried_over: bool,` to `JobStepRow`; add:

```rust
/// One carried-over row for a restart job (spec §4.2).
#[derive(Debug, Clone)]
pub struct Seed {
    pub step_name: String,
    /// Terminal status to write: completed | failed | skipped | cancelled.
    pub status: String,
    pub output: Option<JsonValue>,
    pub error_message: Option<String>,
}

impl JobStepRepo {
    /// Overwrite freshly created rows with carried-over terminal state, inside
    /// the creation transaction. Clears every "live" column the creator may
    /// have set (ready_at on root rows) and any execution residue.
    pub async fn seed_steps_tx<'e, E>(executor: E, job_id: Uuid, seeds: &[Seed]) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres> + Copy,
    {
        for seed in seeds {
            let result = sqlx::query(
                r#"
                UPDATE job_step
                SET status = $3, output = $4, error_message = $5,
                    completed_at = NOW(), carried_over = TRUE,
                    ready_at = NULL, retry_at = NULL, started_at = NULL, worker_id = NULL,
                    agent_state = NULL, suspended_at = NULL
                WHERE job_id = $1 AND step_name = $2
                "#,
            )
            .bind(job_id)
            .bind(&seed.step_name)
            .bind(&seed.status)
            .bind(&seed.output)
            .bind(&seed.error_message)
            .execute(executor)
            .await
            .with_context(|| format!("seed step '{}'", seed.step_name))?;
            if result.rows_affected() != 1 {
                bail!(
                    "seed step '{}' matched {} rows (expected 1)",
                    seed.step_name,
                    result.rows_affected()
                );
            }
        }
        Ok(())
    }
}
```
(`E: Copy` — `&mut PgConnection` is not `Copy`; use `&mut *tx` at the call site once per seed instead: make the function take `tx: &mut sqlx::PgConnection` and pass `&mut *tx`. Match how `create_steps_tx` is declared at `:178` and follow the same executor style.) Re-export `Seed` from `crates/stroem-db/src/lib.rs` alongside `NewJobStep`.

`web/api/jobs.rs` step JSON: add `"carried_over": step.carried_over,`.

`ui/src/lib/types.ts`: add `carried_over: boolean;` to `JobStep`; add `carried_over: false,` to every `JobStep` fixture (`ui/src/components/__tests__/step-timeline.test.tsx` `makeStep`, `ui/src/lib/__tests__/eta.test.ts`, others found by `grep -rn "approval_fields: null" ui/src`).

- [ ] **Step 4: Run to verify**

Run: `cargo test -p stroem-db && cargo test -p stroem-server --lib && cd ui && bunx tsc --noEmit && bun run test`
Expected: green; the migration test in stroem-db picks up 045.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-db crates/stroem-server/src/web/api/jobs.rs ui/src
git commit -m "feat(db): job_step.carried_over + seed_steps_tx for restart jobs

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 2: `restart::compute_restart_set` (pure) with unit tests

**Files:**
- Create: `crates/stroem-server/src/restart.rs`
- Modify: `crates/stroem-server/src/lib.rs` (`pub mod restart;`)

**Interfaces:**
- Produces:
```rust
pub struct RestartPlan {
    pub restart_steps: Vec<String>,           // sorted
    pub carried: Vec<stroem_db::Seed>,        // sorted by step_name
    pub carried_failed: Vec<String>,          // carried failed rows NOT tolerated by the current flow
    pub carried_failed_tolerated: Vec<String>,
}
pub enum RestartError { UnknownStep(String), LoopInstance { base: String } }
pub fn compute_restart_set(flow: &HashMap<String, FlowStep>, source_steps: &[JobStepRow], from_step: &str) -> Result<RestartPlan, RestartError>
```

- [ ] **Step 1: Write the failing tests** (in-module `#[cfg(test)]`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use stroem_common::models::workflow::FlowStep;
    use stroem_db::JobStepRow;

    fn fs(deps: &[&str], cof: bool) -> FlowStep {
        FlowStep {
            action: "noop".into(), name: None, description: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            input: Default::default(), continue_on_failure: cof, timeout: None, when: None,
            for_each: None, sequential: false, retry: None, inline_action: None,
        }
    }
    fn row(name: &str, status: &str, output: Option<serde_json::Value>) -> JobStepRow {
        JobStepRow { step_name: name.into(), status: status.into(), output, error_message: None, ..Default::default() }
    }
    fn names(v: &[Seed]) -> Vec<&str> { v.iter().map(|s| s.step_name.as_str()).collect() }

    #[test]
    fn linear_middle_reruns_downstream_carries_upstream() {
        let flow = HashMap::from([("a".into(), fs(&[], false)), ("b".into(), fs(&["a"], false)), ("c".into(), fs(&["b"], false))]);
        let src = [row("a", "completed", Some(json!(1))), row("b", "failed", None), row("c", "skipped", None)];
        let p = compute_restart_set(&flow, &src, "b").unwrap();
        assert_eq!(p.restart_steps, vec!["b", "c"]);
        assert_eq!(names(&p.carried), vec!["a"]);
        assert_eq!(p.carried[0].status, "completed");
        assert_eq!(p.carried[0].output, Some(json!(1)));
    }

    #[test]
    fn diamond_other_branch_carried_as_failed_and_flagged() {
        let flow = HashMap::from([
            ("a".into(), fs(&[], false)), ("b".into(), fs(&["a"], false)),
            ("c".into(), fs(&["a"], false)), ("d".into(), fs(&["b", "c"], false)),
        ]);
        let src = [row("a", "completed", None), row("b", "failed", None), row("c", "failed", None), row("d", "skipped", None)];
        let p = compute_restart_set(&flow, &src, "b").unwrap();
        assert_eq!(p.restart_steps, vec!["b", "d"]);
        assert_eq!(names(&p.carried), vec!["a", "c"]);
        assert_eq!(p.carried_failed, vec!["c"]);
        assert!(p.carried_failed_tolerated.is_empty());
    }

    #[test]
    fn carried_failed_tolerated_by_current_flow_is_split_out() {
        let flow = HashMap::from([("a".into(), fs(&[], false)), ("b".into(), fs(&["a"], true)), ("c".into(), fs(&["a"], false))]);
        let src = [row("a", "completed", None), row("b", "failed", None), row("c", "failed", None)];
        let p = compute_restart_set(&flow, &src, "c").unwrap();
        assert_eq!(p.carried_failed_tolerated, vec!["b"]);
        assert!(p.carried_failed.is_empty());
    }

    #[test]
    fn root_restart_carries_nothing() {
        let flow = HashMap::from([("a".into(), fs(&[], false)), ("b".into(), fs(&["a"], false))]);
        let src = [row("a", "completed", None), row("b", "completed", None)];
        let p = compute_restart_set(&flow, &src, "a").unwrap();
        assert_eq!(p.restart_steps, vec!["a", "b"]);
        assert!(p.carried.is_empty());
    }

    #[test]
    fn step_added_upstream_pulls_existing_dependent_into_restart_set() {
        // Source ran a → c. Current flow inserted b between them: a → b → c.
        let flow = HashMap::from([("a".into(), fs(&[], false)), ("b".into(), fs(&["a"], false)), ("c".into(), fs(&["b"], false)), ("z".into(), fs(&[], false))]);
        let src = [row("a", "completed", None), row("c", "completed", None), row("z", "failed", None)];
        let p = compute_restart_set(&flow, &src, "z").unwrap();
        assert_eq!(p.restart_steps, vec!["b", "c", "z"], "b is new → root; c depends on b → rerun");
        assert_eq!(names(&p.carried), vec!["a"]);
    }

    #[test]
    fn step_removed_from_flow_is_dropped() {
        let flow = HashMap::from([("a".into(), fs(&[], false))]);
        let src = [row("a", "completed", None), row("gone", "failed", None)];
        let p = compute_restart_set(&flow, &src, "a").unwrap();
        assert!(p.carried.is_empty() && p.carried_failed.is_empty());
    }

    #[test]
    fn non_terminal_source_rows_carry_as_cancelled() {
        let flow = HashMap::from([("a".into(), fs(&[], false)), ("b".into(), fs(&[], false)), ("c".into(), fs(&[], false)), ("d".into(), fs(&[], false)), ("x".into(), fs(&[], false))]);
        let src = [row("a", "pending", None), row("b", "ready", None), row("c", "claimed", None), row("d", "suspended", None), row("x", "failed", None)];
        let p = compute_restart_set(&flow, &src, "x").unwrap();
        for s in &p.carried {
            assert_eq!(s.status, "cancelled", "{}", s.step_name);
            assert_eq!(s.error_message.as_deref(), Some("carried over from cancelled source job"));
        }
    }

    #[test]
    fn for_each_placeholder_carried_with_aggregated_output_instances_ignored() {
        let mut lp = fs(&[], false); lp.for_each = Some(json!("{{ x }}"));
        let flow = HashMap::from([("loop".into(), lp), ("after".into(), fs(&["loop"], false)), ("x".into(), fs(&[], false))]);
        let mut inst = row("loop[0]", "completed", Some(json!(1))); inst.loop_source = Some("loop".into());
        let src = [row("loop", "completed", Some(json!([1, 2]))), inst, row("after", "completed", None), row("x", "failed", None)];
        let p = compute_restart_set(&flow, &src, "x").unwrap();
        assert_eq!(names(&p.carried), vec!["after", "loop"]);
        assert_eq!(p.carried.iter().find(|s| s.step_name == "loop").unwrap().output, Some(json!([1, 2])));
    }

    #[test]
    fn loop_instance_and_unknown_step_are_rejected() {
        let flow = HashMap::from([("loop".into(), fs(&[], false))]);
        assert!(matches!(compute_restart_set(&flow, &[], "loop[2]"), Err(RestartError::LoopInstance { base }) if base == "loop"));
        assert!(matches!(compute_restart_set(&flow, &[], "nope"), Err(RestartError::UnknownStep(s)) if s == "nope"));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p stroem-server --lib restart::`
Expected: compile error (module missing).

- [ ] **Step 3: Implement** `crates/stroem-server/src/restart.rs`

```rust
//! Restart From Step — pure planning (spec §4.1–4.2). No I/O.

use std::collections::{BTreeSet, HashMap, HashSet};
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::FlowStep;
use stroem_db::{JobStepRow, Seed};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartError {
    UnknownStep(String),
    LoopInstance { base: String },
}

impl std::fmt::Display for RestartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownStep(s) => write!(f, "Step '{}' is not in the current flow", s),
            Self::LoopInstance { base } => write!(f, "Restart from the loop step '{}', not an instance", base),
        }
    }
}
impl std::error::Error for RestartError {}

#[derive(Debug, Clone, Default)]
pub struct RestartPlan {
    pub restart_steps: Vec<String>,
    pub carried: Vec<Seed>,
    pub carried_failed: Vec<String>,
    pub carried_failed_tolerated: Vec<String>,
}

pub const CARRIED_CANCELLED_MSG: &str = "carried over from cancelled source job";

/// roots = {from_step} ∪ {current steps with no source row};
/// restart_set = roots ∪ transitive_dependents(flow, roots); everything else is carried.
pub fn compute_restart_set(
    flow: &HashMap<String, FlowStep>,
    source_steps: &[JobStepRow],
    from_step: &str,
) -> Result<RestartPlan, RestartError> {
    if let Some(i) = from_step.find('[') {
        return Err(RestartError::LoopInstance { base: from_step[..i].to_string() });
    }
    if !flow.contains_key(from_step) {
        return Err(RestartError::UnknownStep(from_step.to_string()));
    }

    // Placeholder/plain rows only — instances (loop_source set) are never in the flow.
    let source_by_name: HashMap<&str, &JobStepRow> = source_steps
        .iter()
        .filter(|s| s.loop_source.is_none())
        .map(|s| (s.step_name.as_str(), s))
        .collect();

    let mut restart: BTreeSet<String> = BTreeSet::new();
    restart.insert(from_step.to_string());
    for name in flow.keys() {
        if !source_by_name.contains_key(name.as_str()) {
            restart.insert(name.clone());
        }
    }
    // Transitive closure over depends_on (fixed point; flow is a DAG, bounded by |flow|).
    loop {
        let before = restart.len();
        for (name, fs) in flow {
            if !restart.contains(name) && fs.depends_on.iter().any(|d| restart.contains(d)) {
                restart.insert(name.clone());
            }
        }
        if restart.len() == before {
            break;
        }
    }

    let terminal: HashSet<&str> = [
        StepStatus::Completed.as_ref(), StepStatus::Failed.as_ref(),
        StepStatus::Skipped.as_ref(), StepStatus::Cancelled.as_ref(),
    ].into_iter().collect();

    let mut carried = Vec::new();
    let mut carried_failed = Vec::new();
    let mut carried_failed_tolerated = Vec::new();
    let mut names: Vec<&String> = flow.keys().filter(|n| !restart.contains(*n)).collect();
    names.sort();
    for name in names {
        let Some(src) = source_by_name.get(name.as_str()) else { continue };
        let seed = if terminal.contains(src.status.as_str()) {
            Seed {
                step_name: name.clone(),
                status: src.status.clone(),
                output: src.output.clone(),
                error_message: src.error_message.clone(),
            }
        } else {
            Seed {
                step_name: name.clone(),
                status: StepStatus::Cancelled.as_ref().to_string(),
                output: None,
                error_message: Some(CARRIED_CANCELLED_MSG.to_string()),
            }
        };
        if seed.status == StepStatus::Failed.as_ref() {
            if flow[name].continue_on_failure {
                carried_failed_tolerated.push(name.clone());
            } else {
                carried_failed.push(name.clone());
            }
        }
        carried.push(seed);
    }

    Ok(RestartPlan {
        restart_steps: restart.into_iter().collect(),
        carried,
        carried_failed,
        carried_failed_tolerated,
    })
}
```
Add `pub mod restart;` to `crates/stroem-server/src/lib.rs`. `JobStepRow` derives `Default` (verified) so the test `row()` helper compiles.

- [ ] **Step 4: Run to verify**

Run: `cargo test -p stroem-server --lib restart::`
Expected: 9 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/restart.rs crates/stroem-server/src/lib.rs
git commit -m "feat(restart): compute_restart_set — pure restart-set/carry-set planning

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 3: `CreationMode` in the creator + `create_restart_job`

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs` — inner signature (`:106-121`), sentinel block (`:131-162`), job INSERT (`:316-334`), after `create_steps_tx` (`:338`), wrappers, `handle_task_steps_pass` call (`:624-640`)
- Test: `crates/stroem-server/tests/restart_integration_test.rs` (new; copy the harness from `rerun_integration_test.rs:30-300` — `spawn_pg`, `build_test_app`, `execute_task`, `get_job`, plus `worker_req` from Plan A Task 5)

**Interfaces:**
- Produces:
```rust
pub enum CreationMode<'a> {
    Normal,
    Rerun { source_job_id: Uuid },
    Restart { source: &'a JobRow, from_step: &'a str, plan: &'a crate::restart::RestartPlan },
}
pub async fn create_restart_job(
    workspaces: &WorkspaceManager, pool: &PgPool, workspace_config: &WorkspaceConfig,
    workspace_name: &str, source: &JobRow, plan: &RestartPlan, from_step: &str,
    source_id: Option<&str>, revision: Option<&str>, defaults: JobDefaults,
) -> Result<CreatedJob>
```
- Inner takes `mode: CreationMode<'a>` in place of `source_job_id: Option<Uuid>`. `create_job_for_task(_detailed)` map `source_job_id.map(|id| CreationMode::Rerun{..}).unwrap_or(CreationMode::Normal)`; `create_child_job_for_task` and `handle_task_steps_pass` pass `CreationMode::Normal`.

- [ ] **Step 1: Write the failing tests** (`tests/restart_integration_test.rs`)

Build a workspace with three script actions (`a`, `b`, `c` all `type: script`, `cmd: "true"`) and a task `line` with flow `a → b → c`, where `b` has `input: {upstream: "{{ a.output.val }}"}`. Register a worker; drive steps through `/worker/jobs/claim` and `/worker/jobs/{id}/steps/{s}/complete`.

```rust
async fn run_source_job_failing_at_b(app: &TestApp) -> Result<Uuid> {
    let (st, body) = execute_task(app, "default", "line", json!({"input": {"note": "n1"}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let worker = register_worker(app).await?;
    complete_next(app, worker, job_id, "a", json!({"output": {"val": "A-OUT"}})).await?;   // helper: claim then complete
    complete_next(app, worker, job_id, "b", json!({"exit_code": 1, "error": "b broke"})).await?;
    assert_eq!(get_job(app, &job_id.to_string()).await?["status"], "failed");
    Ok(job_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_from_middle_carries_upstream_and_reruns_downstream() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    let source_steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let ws = app.workspace.clone(); // TestApp keeps the WorkspaceConfig
    let plan = stroem_server::restart::compute_restart_set(&ws.tasks["line"].flow, &source_steps, "b").unwrap();

    let created = stroem_server::job_creator::create_restart_job(
        &app.mgr, &app.pool, &ws, "default", &source, &plan, "b", Some("tester"), None, JobDefaults::default(),
    ).await?;
    assert!(!created.terminal_at_creation);

    let new = JobRepo::get(&app.pool, created.job_id).await?.unwrap();
    assert_eq!(new.source_type, "restart");
    assert_eq!(new.source_job_id, Some(source_id));
    assert_eq!(new.restart_from_step.as_deref(), Some("b"));
    assert_eq!(new.raw_input, source.raw_input);
    assert_eq!(new.input, source.input, "replayed raw_input resolves to the same input");

    let by: HashMap<_, _> = JobStepRepo::get_steps_for_job(&app.pool, created.job_id).await?
        .into_iter().map(|s| (s.step_name.clone(), s)).collect();
    assert_eq!(by["a"].status, "completed"); assert!(by["a"].carried_over);
    assert_eq!(by["a"].output.as_ref().unwrap()["val"], "A-OUT");
    assert!(by["a"].started_at.is_none());
    assert_eq!(by["b"].status, "ready", "promoted immediately: its dep is carried completed");
    assert!(!by["b"].carried_over);
    assert_eq!(by["c"].status, "pending");

    // Worker claims b: the carried output must render into b's input.
    let worker = register_worker(&app).await?;
    let claim = claim(&app, worker).await?;
    assert_eq!(claim["job_id"], created.job_id.to_string());
    assert_eq!(claim["step_name"], "b");
    assert_eq!(claim["input"]["upstream"], "A-OUT");
    complete(&app, created.job_id, "b", json!({"output": {"ok": true}})).await?;
    complete_next(&app, worker, created.job_id, "c", json!({"output": {"done": 1}})).await?;
    let j = get_job(&app, &created.job_id.to_string()).await?;
    assert_eq!(j["status"], "completed");
    assert_eq!(j["output"]["c"]["done"], 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_set_entirely_skipped_settles_failed_at_creation() -> Result<()> {
    // a → b → c, source: a failed, b/c skipped. Restart from c: b carried skipped,
    // a carried failed → c has all deps skipped → cascade-skipped → job failed at creation.
    let app = build_test_app("default", line_workspace()).await?;
    let (st, body) = execute_task(&app, "default", "line", json!({"input": {"note": "n1"}})).await?;
    assert_eq!(st, StatusCode::OK);
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let worker = register_worker(&app).await?;
    complete_next(&app, worker, source_id, "a", json!({"exit_code": 1, "error": "a broke"})).await?;
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    assert_eq!(source.status, "failed");
    let steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let plan = stroem_server::restart::compute_restart_set(&app.workspace.tasks["line"].flow, &steps, "c").unwrap();
    assert_eq!(plan.carried_failed, vec!["a"]);

    let created = stroem_server::job_creator::create_restart_job(
        &app.mgr, &app.pool, &app.workspace, "default", &source, &plan, "c", None, None, JobDefaults::default(),
    ).await?;
    assert!(created.terminal_at_creation);
    let new = JobRepo::get(&app.pool, created.job_id).await?.unwrap();
    assert_eq!(new.status, "failed");
    let by: HashMap<_, _> = JobStepRepo::get_steps_for_job(&app.pool, created.job_id).await?
        .into_iter().map(|s| (s.step_name.clone(), s)).collect();
    assert_eq!(by["c"].status, "skipped");
    assert!(!by["c"].carried_over);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_rejects_legacy_source_without_raw_input() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;
    sqlx::query("UPDATE job SET raw_input = NULL WHERE job_id = $1").bind(source_id).execute(&app.pool).await?;
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    let steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let plan = stroem_server::restart::compute_restart_set(&app.workspace.tasks["line"].flow, &steps, "b").unwrap();
    let err = stroem_server::job_creator::create_restart_job(
        &app.mgr, &app.pool, &app.workspace, "default", &source, &plan, "b", None, None, JobDefaults::default(),
    ).await.unwrap_err();
    assert!(format!("{err:#}").contains("predates Re-run prefill"), "{err:#}");
    Ok(())
}
```

Extend `TestApp` in this new file with `workspace: WorkspaceConfig` and `mgr: WorkspaceManager` (clone `workspace` before moving it into `from_config`; `WorkspaceManager::from_config` returns the value to store — build it once, clone it into `AppState::new`, keep one in `TestApp`; if `WorkspaceManager` is not `Clone`, wrap in `Arc` and pass `&*app.mgr`). Helpers `register_worker`, `claim`, `complete`, `complete_next` are thin wrappers over `worker_req` (see Plan A Task 5) — `complete_next` asserts the claimed `step_name` equals the expected one.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p stroem-server --test restart_integration_test`
Expected: compile error (`create_restart_job`, `CreationMode` missing).

- [ ] **Step 3: Implement**

In `job_creator.rs`:

```rust
/// How a job comes into being. Replaces the positional `source_job_id`, which
/// used to mean both "resolve Re-run sentinels against this job" and "persist
/// this lineage pointer".
pub enum CreationMode<'a> {
    Normal,
    /// User clicked Re-run: `••••••` sentinels in `input` are replaced from the
    /// source's `raw_input`; `source_job_id` is persisted.
    Rerun { source_job_id: Uuid },
    /// Restart From Step (spec 2026-09-07): `input` is the source's `raw_input`
    /// replayed through the normal pipeline; carried rows are seeded in the
    /// creation transaction; lineage + `restart_from_step` are persisted.
    Restart {
        source: &'a JobRow,
        from_step: &'a str,
        plan: &'a crate::restart::RestartPlan,
    },
}
```

Inner: replace `source_job_id: Option<Uuid>` with `mode: CreationMode<'a>`. Replace the sentinel block with:

```rust
        let mut effective_input = input;
        let (lineage_source_job_id, restart_from_step): (Option<Uuid>, Option<&str>) = match &mode {
            CreationMode::Normal => (None, None),
            CreationMode::Rerun { source_job_id } => {
                let src_id = *source_job_id;
                let source_job = stroem_db::JobRepo::get(pool, src_id).await
                    .context("fetch source job for re-run")?
                    .ok_or_else(|| anyhow::anyhow!("Source job {} not found", src_id))?;
                if source_job.workspace != workspace_name {
                    bail!("Source job {} belongs to workspace '{}', cannot Re-run into '{}'",
                          src_id, source_job.workspace, workspace_name);
                }
                let source_raw = source_job.raw_input
                    .ok_or_else(|| anyhow::anyhow!("Source job {} predates Re-run prefill (no raw_input)", src_id))?;
                effective_input = stroem_common::template::resolve_rerun_sentinels(&effective_input, &source_raw, &task.input)
                    .context("resolve re-run sentinels")?;
                (Some(src_id), None)
            }
            CreationMode::Restart { source, from_step, .. } => {
                if source.workspace != workspace_name {
                    bail!("Source job {} belongs to workspace '{}', cannot restart into '{}'",
                          source.job_id, source.workspace, workspace_name);
                }
                // `input` passed by create_restart_job IS the source raw_input; nothing to resolve.
                (Some(source.job_id), Some(*from_step))
            }
        };
```
Job INSERT: bind `lineage_source_job_id` and `restart_from_step` (replacing `source_job_id` and `None`). After `create_steps_tx`:

```rust
        if let CreationMode::Restart { plan, .. } = &mode {
            JobStepRepo::seed_steps_tx(&mut *tx, job_id, &plan.carried)
                .await
                .context("seed carried-over steps")?;
        }
```
(Plan A already made the post-commit cascade unconditional, so nothing else is needed for seeded rows to be promoted/settled.)

Wrappers: `create_job_for_task_detailed` → `match source_job_id { Some(id) => CreationMode::Rerun { source_job_id: id }, None => CreationMode::Normal }`; `create_child_job_for_task` and `handle_task_steps_pass` → `CreationMode::Normal`.

New:
```rust
/// Restart From Step. `plan` comes from `restart::compute_restart_set` (also
/// used by the dry-run endpoint). Input = source `raw_input` replayed through
/// merge_defaults + resolve_connection_inputs (spec §4.4). Legacy sources
/// without `raw_input` are rejected exactly like Re-run.
#[allow(clippy::too_many_arguments)]
pub async fn create_restart_job(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    source: &JobRow,
    plan: &crate::restart::RestartPlan,
    from_step: &str,
    source_id: Option<&str>,
    revision: Option<&str>,
    defaults: JobDefaults,
) -> Result<CreatedJob> {
    let raw = source.raw_input.clone().ok_or_else(|| {
        anyhow::anyhow!("Source job {} predates Re-run prefill (no raw_input)", source.job_id)
    })?;
    create_job_for_task_inner(
        workspaces, pool, workspace_config, workspace_name, &source.task_name, raw,
        "restart", source_id, None, None, revision,
        CreationMode::Restart { source, from_step, plan }, None, defaults,
    )
    .await
}
```

- [ ] **Step 4: Run to verify**

Run: `cargo test -p stroem-server --test restart_integration_test && cargo test -p stroem-server --test rerun_integration_test && cargo test -p stroem-server --test integration_test task_action`
Expected: green (rerun path unchanged in behaviour).

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/job_creator.rs crates/stroem-server/tests/restart_integration_test.rs
git commit -m "feat(restart): CreationMode {Normal, Rerun, Restart} and create_restart_job with carried-row seeding

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 4: More restart semantics tests (for_each, task, approval, flow change, input replay)

**Files:**
- Test: `crates/stroem-server/tests/restart_integration_test.rs` (append)

These lock spec §4.2/§4.3 behaviours that Task 3's code already provides; they are separate so a reviewer can reject a semantic without rejecting the mechanism. Each test builds its own workspace variant.

- [ ] **Step 1: Write the tests**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn carried_for_each_placeholder_exposes_aggregated_output_without_instances() -> Result<()> {
    // flow: seed(script, output {items:[1,2]}) → loop(for_each "{{ seed.output.items | json_encode() }}", script) → after(script, input {n: "{{ loop.output | length }}"}) ; plus tail(script) depends_on after — restart from tail.
    // Run source to completion via the worker (loop expands to loop[0], loop[1]).
    // Fail `tail` in the source. Restart from `tail`.
    // Assert: new job has rows seed/loop/after/tail only (no loop[0]/loop[1]); loop.carried_over && loop.output == [..2 items..]; tail ready; claim tail → complete → job completed.
}

#[tokio::test(flavor = "multi_thread")]
async fn carried_task_step_keeps_output_and_creates_no_child() -> Result<()> {
    // flow: sub(type: task → child task with one script step) → tail(script). Source: run child to completion via worker (child job appears with parent_job_id), then fail tail. Restart from tail.
    // Assert: new `sub` row completed+carried_over with the child's output; JobRepo::get_child_jobs(new_job_id) is empty.
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_from_task_step_creates_child_under_new_job() -> Result<()> {
    // Same flow; restart from `sub`. Assert a child job exists with parent_job_id == new job id and parent_step_name == "sub".
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_from_approval_step_suspends_new_job() -> Result<()> {
    // flow: a(script) → gate(type: approval, message "ok?") → b(script). Source: a completes, gate approved via POST /api/jobs/{id}/steps/gate/approve, b fails. Restart from gate.
    // Assert: new gate row status "suspended", suspended_at set; a carried.
}

#[tokio::test(flavor = "multi_thread")]
async fn flow_change_new_upstream_step_forces_rerun_of_dependent() -> Result<()> {
    // Source ran with flow a → c (c completed, z failed, z independent). Then swap the TestApp's workspace to a → b → c (+ z) using WorkspaceManager reload/from_config (build a second TestApp sharing the same Postgres pool: factor `build_test_app_with_pool(pool, ws)`).
    // Restart from z. Assert restart_steps == [b, c, z]; a carried; b ready.
}

#[tokio::test(flavor = "multi_thread")]
async fn input_replay_rotated_connection_yields_new_value() -> Result<()> {
    // Workspace with a connection `db` {host: "old"} and task input `conn: {type: postgres}`. Source job created with input {conn: "db"} → job.input.conn.host == "old".
    // Mutate workspace connection host to "new" (second TestApp on the same pool). Restart → new job.input.conn.host == "new" (raw_input replay), source untouched.
}
```

Write each body fully following the Task 3 helpers (`execute_task`, `register_worker`, `claim`, `complete`, `complete_next`, `get_job`); for approval use `api_request`-style POST to `/api/jobs/{id}/steps/gate/approve` with `json!({"approved": true})` (see `integration_test.rs` approval tests around `:19586` for the exact body shape). For the for_each source run, claim in a loop until no step is returned, completing each with `json!({"output": {"i": 1}})`; the placeholder is settled by the server.

- [ ] **Step 2: Run** — `cargo test -p stroem-server --test restart_integration_test` — Expected: all pass without further production changes. If any fails, the failure is a real semantic bug: fix it in `restart.rs`/`job_creator.rs` under TDD and note it in the commit.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/tests/restart_integration_test.rs
git commit -m "test(restart): for_each/task/approval carry-over, flow change, connection replay

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 5: `POST /api/jobs/{id}/restart` (+ `dry_run`), ACL, errors, route

**Files:**
- Modify: `crates/stroem-server/src/web/api/jobs.rs` (add handler after `cancel_job`), `crates/stroem-server/src/web/api/mod.rs` (route + move `classify_execute_error` here as `pub(crate) fn classify_execute_error`), `crates/stroem-server/src/web/api/tasks.rs` (use the moved fn; keep its unit tests, moving them along)
- Test: `crates/stroem-server/tests/restart_integration_test.rs` (append), plus one ACL test in `integration_test.rs` using `setup_with_auth_and_acl`

**Interfaces:**
- Request `RestartJobRequest { from_step: String, #[serde(default)] dry_run: bool }`.
- Responses per spec §6.1: dry-run `200 {restart_steps, carried_over, carried_failed, carried_failed_tolerated}`; real `201 {job_id, restart_steps, carried_over, carried_failed}`.

- [ ] **Step 1: Write the failing tests**

```rust
async fn restart_req(app: &TestApp, job_id: Uuid, body: JsonValue) -> Result<(StatusCode, JsonValue)> { /* POST /api/jobs/{job_id}/restart, like execute_task */ }

#[tokio::test(flavor = "multi_thread")]
async fn restart_endpoint_dry_run_then_real_run() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;
    let (st, body) = restart_req(&app, source_id, json!({"from_step": "b", "dry_run": true})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["restart_steps"], json!(["b", "c"]));
    assert_eq!(body["carried_over"], json!(["a"]));
    assert_eq!(body["carried_failed"], json!([]));
    assert_eq!(JobRepo::list(&app.pool, Some("default"), None, None, None, 100, 0).await?.len(), 1, "dry run creates nothing");

    let (st, body) = restart_req(&app, source_id, json!({"from_step": "b"})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    assert_eq!(body["restart_steps"], json!(["b", "c"]));
    let new = get_job(&app, &new_id.to_string()).await?;
    assert_eq!(new["source_type"], "restart");
    assert_eq!(new["restart_from_step"], "b");
    assert_eq!(new["source_job_id"], source_id.to_string());
    let a = new["steps"].as_array().unwrap().iter().find(|s| s["step_name"] == "a").unwrap();
    assert_eq!(a["carried_over"], true);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_endpoint_rejections() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    // running source → 409
    let (_, body) = execute_task(&app, "default", "line", json!({"input": {"note": "x"}})).await?;
    let running: Uuid = body["job_id"].as_str().unwrap().parse()?;
    assert_eq!(restart_req(&app, running, json!({"from_step": "a"})).await?.0, StatusCode::CONFLICT);
    // finished source
    let source_id = run_source_job_failing_at_b(&app).await?;
    assert_eq!(restart_req(&app, source_id, json!({"from_step": "nope"})).await?.0, StatusCode::BAD_REQUEST);
    assert_eq!(restart_req(&app, source_id, json!({"from_step": "b[0]"})).await?.0, StatusCode::BAD_REQUEST);
    assert_eq!(restart_req(&app, Uuid::new_v4(), json!({"from_step": "a"})).await?.0, StatusCode::NOT_FOUND);
    sqlx::query("UPDATE job SET raw_input = NULL WHERE job_id = $1").bind(source_id).execute(&app.pool).await?;
    let (st, body) = restart_req(&app, source_id, json!({"from_step": "b"})).await?;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("predates"));
    Ok(())
}
```
Redaction test (same file): add a workspace secret `token: "s3cr3t-value"` to `line_workspace()`; complete source step `a` with `output: {"leak": "s3cr3t-value"}`; restart from `b`; `GET /api/jobs/{new}` → the carried `a` step's `output.leak` must equal `"••••••"` (redaction applies to carried rows exactly as to executed ones).

ACL test (in `integration_test.rs`, next to the other `setup_with_auth_and_acl` tests): a View-only user gets 403 on `/restart`, a Deny user 404, and with auth on but no token 401. Model on the existing `cancel` ACL tests (grep `Insufficient permissions to cancel`).

- [ ] **Step 2: Run to verify they fail** — route missing → 404/405 for every call.

- [ ] **Step 3: Implement**

Move `classify_execute_error` (and its `#[cfg(test)] mod classify_execute_error_tests`) from `tasks.rs` to `web/api/mod.rs` as `pub(crate) fn classify_execute_error`; import it in `tasks.rs` (`use super::classify_execute_error;`).

`jobs.rs`:
```rust
#[derive(Debug, Deserialize)]
pub struct RestartJobRequest {
    pub from_step: String,
    #[serde(default)]
    pub dry_run: bool,
}

/// POST /api/jobs/:id/restart — Restart From Step (spec 2026-09-07 §6.1).
#[tracing::instrument(skip(state, auth_user, req))]
pub async fn restart_job(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(id): Path<String>,
    Json(req): Json<RestartJobRequest>,
) -> Result<Response, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;

    // Explicit 401: check_job_acl returns Run when no user is supplied.
    let source_id = match (state.config.auth.is_some(), &auth_user) {
        (false, _) => None,
        (true, Some(user)) => Some(user.claims.email.clone()),
        (true, None) => return Err(AppError::Unauthorized("Authentication required".into())),
    };

    let source = JobRepo::get(&state.pool, job_id).await.context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    match check_job_acl(&state, &auth_user, &source.workspace, &source.task_name).await? {
        TaskPermission::Deny => return Err(AppError::not_found("Job")),
        TaskPermission::View => return Err(AppError::Forbidden("Insufficient permissions to restart this job".into())),
        TaskPermission::Run => {}
    }

    let terminal = matches!(source.status.parse::<JobStatus>().ok(),
        Some(JobStatus::Completed) | Some(JobStatus::Failed) | Some(JobStatus::Cancelled) | Some(JobStatus::Skipped));
    if !terminal {
        return Err(AppError::Conflict("Job is still running".into()));
    }
    if source.raw_input.is_none() {
        return Err(AppError::BadRequest("Source job predates Re-run prefill (no raw_input)".into()));
    }

    let workspace = state.get_workspace(&source.workspace).await.ok_or_else(|| {
        AppError::BadRequest(format!("Workspace '{}' is not loaded", source.workspace))
    })?;
    let task = workspace.tasks.get(&source.task_name).ok_or_else(|| {
        AppError::BadRequest(format!("Task '{}' no longer exists in workspace '{}'", source.task_name, source.workspace))
    })?;

    let source_steps = JobStepRepo::get_steps_for_job(&state.pool, job_id).await.context("get source steps")?;
    let plan = crate::restart::compute_restart_set(&task.flow, &source_steps, &req.from_step)
        .map_err(|e| AppError::BadRequest(match e {
            crate::restart::RestartError::UnknownStep(s) =>
                format!("Step '{}' is not in the current flow of task '{}'", s, source.task_name),
            other => other.to_string(),
        }))?;

    let carried_names: Vec<&str> = plan.carried.iter().map(|s| s.step_name.as_str()).collect();
    if req.dry_run {
        return Ok(Json(json!({
            "restart_steps": plan.restart_steps,
            "carried_over": carried_names,
            "carried_failed": plan.carried_failed,
            "carried_failed_tolerated": plan.carried_failed_tolerated,
        })).into_response());
    }

    let revision = state.workspaces.get_revision(&source.workspace);
    let created = crate::job_creator::create_restart_job(
        &state.workspaces, &state.pool, &workspace, &source.workspace, &source, &plan,
        &req.from_step, source_id.as_deref(), revision.as_deref(),
        crate::config::JobDefaults::from(state.config.as_ref()),
    )
    .await
    .map_err(super::classify_execute_error)?;

    crate::job_creator::fire_initial_suspended_hooks(&state, &workspace, &source.workspace, &source.task_name, created.job_id).await;
    crate::job_recovery::finalize_created_job(&state, created).await;

    Ok((StatusCode::CREATED, Json(json!({
        "job_id": created.job_id.to_string(),
        "restart_steps": plan.restart_steps,
        "carried_over": carried_names,
        "carried_failed": plan.carried_failed,
    }))).into_response())
}
```
Route in `mod.rs`: `.route("/jobs/{id}/restart", post(jobs::restart_job))` next to `cancel`. Imports in `jobs.rs`: `JobStatus`, `StatusCode` (check existing `use` lines).

- [ ] **Step 4: Run to verify** — `cargo test -p stroem-server --test restart_integration_test --test integration_test restart` and `cargo test -p stroem-server --lib classify_execute_error` — Expected: green.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/web/api crates/stroem-server/tests
git commit -m "feat(api): POST /api/jobs/{id}/restart with dry_run, Run ACL, 409/400 guards

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 6: Hooks — `carried_over` on `FailedStepInfo`

**Files:**
- Modify: `crates/stroem-server/src/hooks.rs:47-52` (struct) and the `failed_steps` mapping in `build_hook_context` (~`:330-345`)
- Test: `crates/stroem-server/tests/restart_integration_test.rs` (append)

- [ ] **Step 1: Failing test** — workspace with `on_error` hook whose action is a script with `input: {failed: "{{ hook.failed_steps | json_encode() }}"}`. Source `line` fails at `a` (b, c skipped). Restart from `c` → settles failed at creation → hook job created; read the hook job's step `input.failed` JSON and assert `[{step_name: "a", carried_over: true, ...}]`.

- [ ] **Step 2: Run** — FAIL: `carried_over` key absent.

- [ ] **Step 3: Implement** — add `pub carried_over: bool,` to `FailedStepInfo`; in the mapping add `carried_over: s.carried_over,`. Grep for other `FailedStepInfo {` constructions (tests in `hooks.rs`) and add the field.

- [ ] **Step 4: Run** — green. Also `cargo test -p stroem-server hooks`.

- [ ] **Step 5: Commit** — `feat(hooks): flag carried-over failures in hook.failed_steps`.

---

### Task 7: Duration statistics and ETA exclusions

**Files:**
- Modify: `crates/stroem-db/src/repos/job.rs:1005-1026` and `:1044-1060` (add `AND source_type <> 'restart'`), `crates/stroem-db/src/repos/job_step.rs:1160-1164` (CTE: add `AND source_type <> 'restart'`)
- Modify: `ui/src/lib/eta.ts` (suppress the flat fallback for `source_type === "restart"`)
- Test: `crates/stroem-server/tests/duration_stats_test.rs` (append), `ui/src/lib/__tests__/eta.test.ts` (append)

- [ ] **Step 1: Failing tests** — stats test: insert two completed `api` jobs (durations 10s, 20s) and one completed `restart` job (1s) for the same task via raw SQL (the file already seeds rows this way); `GET /api/workspaces/default/tasks/{t}/stats` → `sample_size == 2`, `min_ms == 10000`. ETA test: `computeEta` for a `restart` job with a running step lacking stats returns `null` (no flat fallback); with full step stats returns the step-weighted value.

- [ ] **Step 2: Run** — FAIL (sample_size 3 / eta non-null).

- [ ] **Step 3: Implement** — SQL predicates as listed; in `eta.ts` after computing `taskP50`: `const isRestart = job.source_type === "restart";` and in the flat-fallback branch `if (isRestart) return null;` (also skip the overrun branch for restart jobs — elapsed vs full-task p50 is meaningless there). `JobDetail` already has `source_type`.

- [ ] **Step 4: Run** — `cargo test -p stroem-server --test duration_stats_test && cd ui && bun run test eta` — green.

- [ ] **Step 5: Commit** — `fix(stats): exclude restart jobs from duration stats; no flat ETA fallback for restart jobs`.

---

### Task 8: UI — `restartJob`, dialog, button placement, badge, lineage

**Files:**
- Modify: `ui/src/lib/api.ts` (add `restartJob`), `ui/src/lib/types.ts` (`RestartPlanResponse`)
- Create: `ui/src/components/restart-dialog.tsx`
- Modify: `ui/src/components/step-detail.tsx`, `ui/src/components/step-timeline.tsx` (`StepRow` badge, `LoopGroup` header button; new props `jobStatus`, `canRestart`), `ui/src/pages/job-detail.tsx` (fetch `getTask` for `can_execute`, lineage entry, pass props)
- Test: `ui/src/components/__tests__/restart-dialog.test.tsx` (new), `ui/src/components/__tests__/step-timeline.test.tsx` (append)

- [ ] **Step 1: Failing tests**

`restart-dialog.test.tsx`: render `<RestartDialog open plan={{restart_steps:["b","c"], carried_over:["a"], carried_failed:["z"], carried_failed_tolerated:[]}} taskName="line" stepName="b" onConfirm={fn} onCancel={fn} />` → text contains "Reruns 2 step(s)", "b, c", "1 step(s) are carried over", and the warning mentions "z"; with `carried_failed: []` the warning is absent; clicking Confirm calls `onConfirm`.

`step-timeline.test.tsx`: (1) a step with `carried_over: true` renders a "carried over" badge and no duration badge; (2) `LoopGroup` header renders a "Restart from here" button when `jobStatus="failed"` and `canRestart` and not when `jobStatus="running"`; (3) instance rows never render the button.

- [ ] **Step 2: Run** — `cd ui && bun run test` — FAIL (component/props missing).

- [ ] **Step 3: Implement**

`api.ts`:
```ts
export interface RestartPlanResponse {
  job_id?: string;
  restart_steps: string[];
  carried_over: string[];
  carried_failed: string[];
  carried_failed_tolerated?: string[];
}
export async function restartJob(jobId: string, fromStep: string, dryRun: boolean): Promise<RestartPlanResponse> {
  return apiFetch<RestartPlanResponse>(`/api/jobs/${encodeURIComponent(jobId)}/restart`, {
    method: "POST",
    body: JSON.stringify({ from_step: fromStep, dry_run: dryRun }),
  });
}
```

`restart-dialog.tsx` — a shadcn `Dialog` (check `ui/src/components/ui/` for `dialog.tsx`; if absent, `bunx shadcn@latest add dialog`) with props `{ open, plan, taskName, stepName, busy, onConfirm, onCancel }` rendering the §7.1 copy; the failure warning only when `plan.carried_failed.length > 0`.

`step-detail.tsx` — new props `jobStatus: string`, `canRestart: boolean`, `sourceJobId?: string | null`. Compute `const isTerminalJob = ["completed","failed","cancelled"].includes(jobStatus)`. Render a `Restart from here` button (`isTerminalJob && canRestart && step.loop_source == null`) that calls `restartJob(jobId, step.step_name, true)`, stores the plan, opens `RestartDialog`; on confirm calls `restartJob(..., false)` and `navigate(`/jobs/${res.job_id}`)`; errors → `alert(message)` (matches the page's cancel handling). When `step.carried_over`: skip the logs `useEffect` entirely and render in the Logs tab: `Carried over from job <Link to=/jobs/{sourceJobId}> — logs and artifacts live there.` (or the no-link variant when `sourceJobId` is null).

`step-timeline.tsx` — `StepTimelineProps` gains `jobStatus: string; canRestart: boolean; sourceJobId?: string | null; onRestart?: (stepName: string) => void`. `StepRow` badge cluster: `{step.carried_over && <span className="…muted badge…">carried over</span>}`; suppress the duration/p50 badges when `carried_over`. `LoopGroup` header: render the same `Restart from here` button (stopPropagation so it does not toggle instances) when `isTerminalJob && canRestart`, calling `onRestart(placeholder.step_name)`. Thread `jobStatus`/`canRestart`/`sourceJobId` down to `StepDetail`. The dialog state for the loop-header path lives in `StepTimeline` (one `RestartDialog` instance) so the header button and `StepDetail` share one component.

`job-detail.tsx` — add `getTask(job.workspace, job.task_name)` to the load path to obtain `can_execute` (default `true` when the field is undefined, i.e. ACL off); pass `jobStatus={job.status} canRestart={canExecute} sourceJobId={job.source_job_id}` to `StepTimeline`; add the lineage entry:
```tsx
...(job.source_job_id && job.source_type === "restart"
  ? [{ label: "Restart of", value: (<span className="text-xs">
        <Link to={`/jobs/${job.source_job_id}`} className="font-mono text-primary hover:underline">{job.source_job_id.substring(0, 8)}</Link>
        {job.restart_from_step && <> from <span className="font-mono">{job.restart_from_step}</span></>}
      </span>) }]
  : []),
```

- [ ] **Step 4: Run** — `cd ui && bun run lint && bunx tsc --noEmit && bun run test` — green.

- [ ] **Step 5: Commit** — `feat(ui): Restart from here (step detail + loop header), carried-over badge, restart lineage`.

---

### Task 9: Docs, TODO, CLAUDE.md, release notes draft, full verification

**Files:**
- Modify: `CLAUDE.md` § *Job Lineage* (replace the "*reserved*" sentence), `docs/internal/TODO.md`, `docs/src/content/docs/guides/` (job/re-run page and hooks page), `docs/src/content/docs/reference/api.md`
- Update the spec header: `**Status:** Implemented (date)`.

- [ ] **Step 1: CLAUDE.md** — under `### Job Lineage`:
```
- **`source_job_id` + `source_type = 'restart'` + `restart_from_step`** — Restart From Step (spec `docs/superpowers/specs/2026-09-07-restart-from-step-design.md`). `restart::compute_restart_set` → `RestartPlan`; `job_creator::create_restart_job` uses `CreationMode::Restart` to seed carried rows (`job_step.carried_over = true`, `JobStepRepo::seed_steps_tx`) inside the creation transaction; the normal post-commit cascade + `settle_if_all_terminal` close the job when the restart set is empty/skipped. Input = source `raw_input` replayed (never the resolved `input`); revision = current. Restart jobs are top-level for hooks and EXCLUDED from duration stats; carried failures are flagged in `hook.failed_steps[].carried_over`. Endpoint `POST /api/jobs/{id}/restart` (`dry_run` for the UI preview). Known limits: state snapshots = latest at claim time; artifacts not carried.
```
- [ ] **Step 2: User docs** — "Restart from a step" section (what reruns, what is carried, the four limitations, "restart from an earlier step to rerun other failures"); API reference entry with both response shapes; hooks page: `carried_over` on `failed_steps`.
- [ ] **Step 3: TODO.md** — nothing new to close from this plan (P1–P6, P8 closed by hotfixes/Plan A); add follow-ups: artifact carry-over, state pinning, P7 retry input replay if still open.
- [ ] **Step 4: Full verification**
```bash
cargo fmt --check --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --no-fail-fast
cd ui && bun run lint && bunx tsc --noEmit && bun run test
```
- [ ] **Step 5: Commit** — `docs: Restart From Step user guide, API reference, CLAUDE.md`.

Release notes for the version that ships Plan B must include: the new endpoint/UI; `rerun`/`restart` workspace-hook behaviour (if not already shipped with Plan A); restart jobs excluded from stats; migration 045.
