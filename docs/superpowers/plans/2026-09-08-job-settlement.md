# Job Settlement Module Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One `settlement` module that owns everything from "a step moved" (or a job was created or cancelled) to the last terminal side effect, replacing the three copies of that procedure in `job_recovery.rs`.

**Architecture:** A pool tier of free functions (`settle::decide` pure, `cascade_and_settle`, `dispatch::init`) that the creator and the pool-only tests call, and a state tier `Settlement` struct built on demand from `&AppState` with seven entries and one private-by-convention body `advance`. `CreatedJob` becomes non-`Copy` with a private flag. Four construction-fixed defects (D1–D4) land as separate commits with regression tests after the structure exists.

**Tech Stack:** Rust 2021, tokio, sqlx runtime queries against Postgres, testcontainers for every DB test, `anyhow::Result`.

**Spec:** `docs/superpowers/specs/2026-09-08-job-settlement-design.md` (revision 3, commit 17fd7b7). The spec is binding; where this plan and the spec disagree, the spec wins, except for the two explicit amendments in Global Constraints.

## Global Constraints

- Behaviour policy is **preserving** except D1–D4 (spec §2). Any other observable change is a defect.
- Every log line string, error string, tracing message and `tracing::instrument` attribute moves verbatim. Grep before and after; the container tests assert on several of them.
- The container suites are the oracle: `cargo test -p stroem-server` must be green at the end of every task. No test is deleted or weakened before Task 9, and Task 9 deletes only tests it names a replacing unit test for.
- No AI co-author trailers in commit messages (user's global CLAUDE.md overrides any session attribution note).
- `cargo fmt --all` and `cargo clippy --workspace -- -D warnings` clean at every commit.
- Test environment for every run: `DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true CARGO_INCREMENTAL=0`. Never set `CARGO_TARGET_DIR`. If the disk is short, `rm -rf /Users/ala/.tmp/cargo/debug/incremental` and remove hours-old `postgres` containers.
- Spec amendment A1: `Settlement::advance`, `Settlement::propagate` and `Settlement::reconcile` are `pub` methods (spec §6.3/§6.5 call them private). Three existing container tests drive them directly (`propagate_to_parent_test.rs` via `handle_job_terminal`, `test_propagate_defers_agent_tool_child_before_registration`, `test_reconcile_settled_children_is_idempotent`). Each carries a doc comment "Public so tests can drive a job from an arbitrary row state; production code goes through the entries." The spec's invariant "nothing outside the module reaches a terminal side effect without the claim" still holds because all three run the claim.
- Spec amendment A2: `hooks::fire_hooks` and `hooks::fire_suspended_hooks` stay `pub` in `settlement::hooks` (spec §4 lists hooks.rs as moved; twelve container tests call `fire_hooks` directly).
- The new module path is `crates/stroem-server/src/settlement/`. Public re-exports for tests: `stroem_server::settlement::{Settlement, CreatedJob, BornTerminal, CancelResult, cascade_and_settle, settle_if_all_terminal}` and `stroem_server::settlement::hooks::{fire_hooks, fire_suspended_hooks, HookContext, SuspendedHookContext, FailedStepInfo, HookArtifactMeta}`.

---

## File map

| File | Responsibility after this plan |
|---|---|
| `crates/stroem-server/src/settlement/mod.rs` | `Settlement` struct, `CreatedJob`, `BornTerminal`, `CancelResult`, the seven entries, `advance`, `server_log`, re-exports |
| `crates/stroem-server/src/settlement/settle.rs` | `Settled`, `decide` (pure, unit-tested), `settle_if_all_terminal`, `cascade_and_settle` |
| `crates/stroem-server/src/settlement/dispatch.rs` | `handle_task_steps`, `handle_task_steps_pass`, `fail_task_step`, `handle_approval_steps`, `fire_initial_suspended_hooks`, `init` |
| `crates/stroem-server/src/settlement/terminal.rs` | `drained`, `claim`, `TerminalPlan`, `plan` (pure, unit-tested), `run_terminal_actions`, `build_minimal_task_def`, `get_hook_error_summary`, `extract_first_failure`, `upload_logs_for_job`, `meta_from_job` |
| `crates/stroem-server/src/settlement/propagate.rs` | `Settlement::propagate` (agent barrier + parent-step write + `advance(parent)`) |
| `crates/stroem-server/src/settlement/retry.rs` | `create_retry_job`, retry message helpers, `compute_retry_delay`, `retry_log_line` |
| `crates/stroem-server/src/settlement/hooks.rs` | former `src/hooks.rs`, plus the hook-job path through `build_step` (Task 8) |
| `crates/stroem-server/src/cancellation.rs` | cancelled-jobs set only: `is_cancelled`, `clear_cancelled`, `mark_cancelled_locally` |
| `crates/stroem-server/src/job_creator.rs` | creation only; calls `settlement::dispatch::init`; `build_step` extracted (Task 8) |
| deleted | `src/orchestrator.rs` (Task 1), `src/job_recovery.rs` (Task 3), `src/hooks.rs` (Task 3) |

---

### Task 1: Pool tier — `settle.rs`, `cascade_and_settle`, delete `orchestrator.rs`

**Files:**
- Create: `crates/stroem-server/src/settlement/mod.rs`, `crates/stroem-server/src/settlement/settle.rs`
- Delete: `crates/stroem-server/src/orchestrator.rs`
- Modify: `crates/stroem-server/src/lib.rs`, `crates/stroem-server/src/job_creator.rs:630`, `crates/stroem-server/src/job_creator.rs:1231-1256` (`orchestrate_after_server_step_failure`), `crates/stroem-server/src/job_recovery.rs:235,591` (the two `orchestrator::on_step_completed` calls)
- Test: `crates/stroem-server/tests/orchestrator_test.rs`, `crates/stroem-server/tests/integration_test.rs`, `crates/stroem-server/tests/mcp_test.rs`

**Interfaces:**
- Produces: `settlement::settle::{Settled, decide, settle_if_all_terminal}`, `settlement::cascade_and_settle(pool, job_id, task, workspace_config: &WorkspaceConfig) -> Result<Option<JobStatus>>`. `job_recovery.rs` and `job_creator.rs` call these; nothing else changes in this task.

- [ ] **Step 1: Write the unit tests for `decide`**

Create `crates/stroem-server/src/settlement/settle.rs` with only the test module first:

```rust
//! Settlement: deciding a job's terminal status from its steps.

use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashSet;
use stroem_common::models::job::{JobStatus, StepStatus};
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{JobRepo, JobStepRepo, JobStepRow};
use uuid::Uuid;

/// What settlement decided for a job whose every step is terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    pub status: JobStatus,
    pub output: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use stroem_common::models::workflow::FlowStep;

    fn flow_step(deps: &[&str], continue_on_failure: bool) -> FlowStep {
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            input: HashMap::new(),
            continue_on_failure,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        }
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

    fn row(name: &str, status: &str, output: Option<serde_json::Value>) -> JobStepRow {
        let mut r = JobStepRow::test_default(Uuid::nil(), name);
        r.status = status.to_string();
        r.output = output;
        r
    }

    #[test]
    fn all_completed_completes_with_terminal_step_outputs() {
        let t = task(vec![("a", flow_step(&[], false)), ("b", flow_step(&["a"], false))]);
        let steps = vec![
            row("a", "completed", Some(serde_json::json!({"x": 1}))),
            row("b", "completed", Some(serde_json::json!({"y": 2}))),
        ];
        let s = decide(&t, &steps).unwrap();
        assert_eq!(s.status, JobStatus::Completed);
        assert_eq!(s.output, Some(serde_json::json!({"b": {"y": 2}})));
    }

    #[test]
    fn untolerated_failure_fails() {
        let t = task(vec![("a", flow_step(&[], false))]);
        let s = decide(&t, &[row("a", "failed", None)]).unwrap();
        assert_eq!(s.status, JobStatus::Failed);
        assert_eq!(s.output, None);
    }

    #[test]
    fn tolerated_failure_completes_and_aggregates_the_rest() {
        let t = task(vec![("a", flow_step(&[], true)), ("b", flow_step(&[], false))]);
        let steps = vec![
            row("a", "failed", None),
            row("b", "completed", Some(serde_json::json!(3))),
        ];
        let s = decide(&t, &steps).unwrap();
        assert_eq!(s.status, JobStatus::Completed);
        assert_eq!(s.output, Some(serde_json::json!({"b": 3})));
    }

    #[test]
    fn cancelled_without_failure_cancels() {
        let t = task(vec![("a", flow_step(&[], false)), ("b", flow_step(&[], false))]);
        let steps = vec![row("a", "completed", None), row("b", "cancelled", None)];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Cancelled);
    }

    #[test]
    fn failed_beats_cancelled() {
        let t = task(vec![("a", flow_step(&[], false)), ("b", flow_step(&[], false))]);
        let steps = vec![row("a", "failed", None), row("b", "cancelled", None)];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Failed);
    }

    #[test]
    fn live_step_is_none() {
        let t = task(vec![("a", flow_step(&[], false)), ("b", flow_step(&[], false))]);
        for live in ["pending", "ready", "claimed", "running", "suspended"] {
            let steps = vec![row("a", "completed", None), row("b", live, None)];
            assert!(decide(&t, &steps).is_none(), "{live} must block settlement");
        }
    }

    #[test]
    fn instance_rows_map_to_their_placeholder_flow_step() {
        // placeholder "p" tolerates failure; its instance "p[0]" failed.
        let t = task(vec![("p", flow_step(&[], true))]);
        let steps = vec![row("p", "completed", None), row("p[0]", "failed", None)];
        assert_eq!(decide(&t, &steps).unwrap().status, JobStatus::Completed);
    }

    #[test]
    fn empty_flow_completes_with_no_output() {
        let t = task(vec![]);
        let s = decide(&t, &[]).unwrap();
        assert_eq!(s.status, JobStatus::Completed);
        assert_eq!(s.output, None);
    }
}
```

`JobStepRow::test_default` does not exist yet: add it to `crates/stroem-db/src/repos/job_step.rs` next to the struct, `#[doc(hidden)] pub fn test_default(job_id: Uuid, step_name: &str) -> Self` returning a row with `status: "pending"`, `action_name: "noop"`, `action_type: "script"`, every `Option` `None`, every `Vec`/`String` empty, `retry_attempt: 0`, `retry_jitter: false`, `carried_over: false`, timestamps `chrono::Utc::now()`. Check the struct's exact field list in `job_step.rs` (grep `pub struct JobStepRow`) and fill every field; the compiler will tell you which you missed.

- [ ] **Step 2: Run the unit tests to verify they fail**

Run: `cargo test -p stroem-server settlement::settle -- --nocapture`
Expected: compile error, `decide` not found.

- [ ] **Step 3: Implement `decide`, `settle_if_all_terminal`, `cascade_and_settle`**

Add to `settle.rs` above the test module. `decide` is the body of `orchestrator::settle_if_all_terminal` (`orchestrator.rs:62-159`) with the reads and writes removed:

```rust
/// Pure settlement decision. `None` while any step is non-terminal.
pub fn decide(task: &TaskDef, steps: &[JobStepRow]) -> Option<Settled> {
    let terminal = |s: &JobStepRow| {
        matches!(
            s.status.parse::<StepStatus>().ok(),
            Some(StepStatus::Completed)
                | Some(StepStatus::Failed)
                | Some(StepStatus::Skipped)
                | Some(StepStatus::Cancelled)
        )
    };
    if !steps.iter().all(terminal) {
        return None;
    }

    // Loop instance steps ("process[0]") are not in task.flow — look up by
    // their placeholder name. Instance failures are already folded into the
    // placeholder by the cascade's rollup rule (R6).
    let flow_name = |name: &str| -> String {
        match name.find('[') {
            Some(i) => name[..i].to_string(),
            None => name.to_string(),
        }
    };
    let tolerated = |name: &str| -> bool {
        task.flow
            .get(&flow_name(name))
            .map(|fs| fs.continue_on_failure)
            .unwrap_or(false)
    };

    let untolerated_failure = steps
        .iter()
        .any(|s| s.status == StepStatus::Failed.as_ref() && !tolerated(&s.step_name));
    if untolerated_failure {
        return Some(Settled { status: JobStatus::Failed, output: None });
    }

    if steps.iter().any(|s| s.status == StepStatus::Cancelled.as_ref()) {
        return Some(Settled { status: JobStatus::Cancelled, output: None });
    }

    // Output = outputs of the flow's terminal steps (nothing depends on them).
    let depended_on: HashSet<&str> = task
        .flow
        .values()
        .flat_map(|fs| fs.depends_on.iter().map(|s| s.as_str()))
        .collect();
    let terminal_steps: HashSet<&str> = task
        .flow
        .keys()
        .filter(|name| !depended_on.contains(name.as_str()))
        .map(|s| s.as_str())
        .collect();
    let mut job_output = serde_json::Map::new();
    for s in steps {
        if terminal_steps.contains(s.step_name.as_str()) {
            if let Some(ref output) = s.output {
                job_output.insert(s.step_name.clone(), output.clone());
            }
        }
    }
    let output = if job_output.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(job_output))
    };
    Some(Settled { status: JobStatus::Completed, output })
}

/// If every step of the job is terminal, decide and persist the job's final
/// status and return it; otherwise return `None` and touch nothing.
///
/// Single source of truth for terminal settlement — called from
/// `cascade_and_settle` AND from job creation (`dispatch::init`), so a job that
/// is already terminal at creation gets exactly the same rules.
#[tracing::instrument(skip(pool, task))]
pub async fn settle_if_all_terminal(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
) -> Result<Option<JobStatus>> {
    let steps = JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .context("Failed to get steps for settlement")?;
    let Some(settled) = decide(task, &steps) else {
        return Ok(None);
    };

    // Never overwrite an explicit cancellation. (Task 6 replaces this re-read
    // with the predicated `JobRepo::settle` write.)
    if let Some(j) = JobRepo::get(pool, job_id).await? {
        if j.status == JobStatus::Cancelled.as_ref() {
            tracing::info!(
                "Job {} is already cancelled, skipping status update",
                job_id
            );
            return Ok(Some(JobStatus::Cancelled));
        }
    }

    match settled.status {
        JobStatus::Failed => {
            tracing::info!("Job {} failed (one or more steps failed)", job_id);
            JobRepo::mark_failed(pool, job_id)
                .await
                .context("Failed to mark job as failed")?;
        }
        JobStatus::Cancelled => {
            tracing::info!(
                "Job {} cancelled (a step was cancelled, no untolerated failure)",
                job_id
            );
            JobRepo::mark_cancelled(pool, job_id)
                .await
                .context("Failed to mark job as cancelled")?;
        }
        _ => {
            let failed_count = steps
                .iter()
                .filter(|s| s.status == StepStatus::Failed.as_ref())
                .count();
            if failed_count > 0 {
                tracing::info!(
                    "Job {} completed with {} tolerable failure(s)",
                    job_id,
                    failed_count
                );
            } else {
                tracing::info!("Job {} completed successfully", job_id);
            }
            JobRepo::mark_completed(pool, job_id, settled.output.clone())
                .await
                .context("Failed to mark job as completed")?;
        }
    }
    Ok(Some(settled.status))
}

/// Run the step cascade for `job_id`, then settle the job if every step is
/// terminal. Replaces `orchestrator::on_step_completed`; the workspace config
/// is required because production always has one.
#[tracing::instrument(skip(pool, task, workspace_config))]
pub async fn cascade_and_settle(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
    workspace_config: &WorkspaceConfig,
) -> Result<Option<JobStatus>> {
    crate::cascade::execute(pool, job_id, task, Some(workspace_config))
        .await
        .context("Failed to run step cascade")?;
    settle_if_all_terminal(pool, job_id, task).await
}
```

Note the first `all_steps_terminal` DB round-trip of the old function is replaced by checking the fetched rows; one query fewer, same decision.

Create `crates/stroem-server/src/settlement/mod.rs`:

```rust
//! Job settlement: everything a job owes after one of its steps moves, from
//! the step cascade through terminal handling. See
//! `docs/superpowers/specs/2026-09-08-job-settlement-design.md` and the
//! `### Settlement` section of CLAUDE.md.

pub mod settle;

pub use settle::{cascade_and_settle, settle_if_all_terminal, Settled};
```

In `lib.rs` replace `pub mod orchestrator;` with `pub mod settlement;` (keep alphabetical order). Delete `src/orchestrator.rs`.

- [ ] **Step 4: Switch the three production callers**

`job_creator.rs:630`: `crate::orchestrator::settle_if_all_terminal(pool, job_id, task)` → `crate::settlement::settle_if_all_terminal(pool, job_id, task)`.

`job_creator.rs::orchestrate_after_server_step_failure` (~1231): replace the `crate::orchestrator::on_step_completed(pool, job_id, step_name, task, Some(workspace_config))` call with `crate::settlement::cascade_and_settle(pool, job_id, task, workspace_config)`; keep the surrounding error logging verbatim.

`job_recovery.rs:235` and `:591`: replace `orchestrator::on_step_completed(&state.pool, job_id, step_name, &task, Some(&workspace)).await?;` with `crate::settlement::cascade_and_settle(&state.pool, job_id, &task, &workspace).await?;` (and the parent-leg equivalent with `parent_job_id`, `&parent_task`, `&parent_ws`). Remove `use crate::orchestrator;`.

- [ ] **Step 5: Switch the test call sites**

Add to `tests/orchestrator_test.rs` (after `make_task`), `tests/integration_test.rs` (next to its `setup` helper) and `tests/mcp_test.rs`:

```rust
/// Pool-only stand-in for the orchestrator call: a minimal workspace holding
/// just this task, since the cascade requires a config for rendering.
fn workspace_with(task: &TaskDef) -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::default();
    ws.tasks.insert("test-task".to_string(), task.clone());
    ws
}

async fn after_step(pool: &PgPool, job_id: Uuid, task: &TaskDef) -> anyhow::Result<()> {
    stroem_server::settlement::cascade_and_settle(pool, job_id, task, &workspace_with(task))
        .await
        .map(|_| ())
}
```

Then, in the three test files, rewrite every call. The pattern is mechanical; use `perl -0pi -e` with these two substitutions and then fix the handful the regex misses by hand:

```
s/(?:stroem_server::orchestrator::)?on_step_completed\(\s*(&pool|pool|&state\.pool),\s*(\w+),\s*"[^"]*",\s*(&?\w+),\s*None,?\s*\)/after_step($1, $2, $3)/g
s/(?:stroem_server::orchestrator::)?on_step_completed\(\s*(&pool|pool|&state\.pool),\s*(\w+),\s*"[^"]*",\s*(&?\w+),\s*Some\((&?[\w.]+)\),?\s*\)/stroem_server::settlement::cascade_and_settle($1, $2, $3, $4).map(|_| ())/g
```

For the second form, `.map(|_| ())` keeps the `?` result type `()`; drop it where the test reads the returned status. The three `stroem_server::orchestrator::settle_if_all_terminal` calls in `orchestrator_test.rs:1679,1708,1727` become `stroem_server::settlement::settle_if_all_terminal`. Remove `use stroem_server::orchestrator;` / `use stroem_server::orchestrator::on_step_completed;` imports. `grep -rn "orchestrator" tests/` must return only comments afterwards.

- [ ] **Step 6: Run the suites**

Run: `cargo test -p stroem-server settlement::settle` then `cargo test -p stroem-server --test orchestrator_test --test integration_test --test mcp_test --test cascade_apply_test`
Expected: all pass; counts unchanged (37 / 340 / 19 / 11) plus the 8 new unit tests.

- [ ] **Step 7: Commit**

```bash
git add -A crates/stroem-server crates/stroem-db
git commit -m "refactor(settlement): pool tier — decide/settle_if_all_terminal/cascade_and_settle, drop the None workspace mode"
```

---

### Task 2: Pool tier — `dispatch.rs` and `init`

**Files:**
- Create: `crates/stroem-server/src/settlement/dispatch.rs`
- Modify: `crates/stroem-server/src/job_creator.rs` (move out `handle_task_steps` `:680-713`, `fail_task_step` `:714-729`, `handle_task_steps_pass` `:730-916`, `handle_approval_steps` `:917-1080`, `fire_initial_suspended_hooks` `:1081-1161`, `orchestrate_after_server_step_failure` `:1231-1256`; replace the `init` block `:614-641`), `crates/stroem-server/src/settlement/mod.rs`, `crates/stroem-server/src/job_recovery.rs` (the four `crate::job_creator::handle_task_steps` / `handle_approval_steps` calls become `crate::settlement::dispatch::…`)

**Interfaces:**
- Consumes: `settlement::cascade_and_settle`, `settlement::settle_if_all_terminal` (Task 1).
- Produces: `settlement::dispatch::{handle_task_steps, handle_approval_steps, fire_initial_suspended_hooks, init}` with the signatures below. `job_creator::create_job_for_task_inner` calls `init`.

- [ ] **Step 1: Move the five functions verbatim**

Create `dispatch.rs` with the file's `use` lines copied from `job_creator.rs` (trim unused ones after the move; clippy will name them). Move the six functions listed above out of `job_creator.rs` with their bodies and doc comments byte-for-byte, except:

- `orchestrate_after_server_step_failure` now calls `crate::settlement::cascade_and_settle` (it already does after Task 1; just move it).
- In `handle_approval_steps`, the render-failure branch that today does `JobStepRepo::mark_failed(...)` followed by `orchestrate_after_server_step_failure(...)` inline calls `fail_task_step(pool, job_id, &step.step_name, &err, task, workspace_config).await?` instead (same two calls, one site; the `?` matches today's error handling — check the existing branch and keep whichever of `?` / log-and-continue it uses).
- Items they need that stay in `job_creator.rs` (`MAX_TASK_DEPTH`, `compute_depth`, `build_step_render_context`, `create_job_for_task_inner`, `CreationMode`, `precheck_literal_connection_inputs`) become `pub(crate)` where they are not already.

- [ ] **Step 2: Add `init`**

At the bottom of `dispatch.rs`:

```rust
/// The creator's post-commit initialisation: everything a freshly committed
/// job owes before it is handed back — root-step cascade, `type: task`
/// dispatch, `type: approval` dispatch, settlement. Returns the settled
/// status when the job reached a terminal state during initialisation.
///
/// Compensation on `Err` stays with the caller (`create_job_for_task_inner`):
/// it belongs to the creation transaction's contract, not to settlement.
pub async fn init(
    pool: &PgPool,
    workspaces: &WorkspaceManager,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    task: &TaskDef,
    defaults: JobDefaults,
) -> Result<Option<JobStatus>> {
    crate::cascade::execute(pool, job_id, task, Some(workspace_config))
        .await
        .context("creation-time step cascade")?;

    handle_task_steps(workspaces, pool, workspace_config, workspace_name, job_id, task, defaults)
        .await?;

    handle_approval_steps(pool, workspace_config, workspace_name, job_id, task)
        .await
        .context("dispatch initial approval steps")?;

    let settled = crate::settlement::settle_if_all_terminal(pool, job_id, task)
        .await
        .context("settle job at creation")?;
    if let Some(ref status) = settled {
        tracing::info!(job_id = %job_id, ?status, "All steps terminal at creation — job settled");
    }
    Ok(settled)
}
```

In `create_job_for_task_inner`, the `let init: Result<Option<JobStatus>> = async { … }.await;` block becomes:

```rust
        let init = crate::settlement::dispatch::init(
            workspaces, pool, workspace_config, workspace_name, job_id, task, defaults,
        )
        .await;
```

Keep the long comment above it and the `match init { … }` compensation below it verbatim.

Add `pub mod dispatch;` to `settlement/mod.rs`. Update the four calls in `job_recovery.rs` (`:239`, `:270`, `:601`, `:626`, approximately) and `fire_initial_suspended_hooks`'s caller (grep it) to the new path.

- [ ] **Step 3: Run the suites**

Run: `cargo clippy -p stroem-server -- -D warnings && cargo test -p stroem-server --test integration_test --test orchestrator_test --test restart_integration_test`
Expected: green, same counts. `test_task_step_dispatch_failure_cascades_and_fails_job` and `test_approval_dispatch_failure_compensates_the_job` are the two that exercise this task most.

- [ ] **Step 4: Commit**

```bash
git add -A crates/stroem-server
git commit -m "refactor(settlement): move task/approval dispatch and the creation init block into settlement::dispatch"
```

---

### Task 3: State tier — `Settlement`, `advance`, the seven entries; delete `job_recovery.rs`

This is the largest task. Everything below `job_recovery.rs`'s `fail_step` moves; the three copies become one. Read the spec §6 in full before starting.

**Files:**
- Create: `settlement/terminal.rs`, `settlement/propagate.rs`, `settlement/retry.rs`; move `src/hooks.rs` → `settlement/hooks.rs` (`git mv`)
- Modify: `settlement/mod.rs`, `src/state.rs` (add `settlement()`), `src/lib.rs` (remove `pub mod hooks; pub mod job_recovery;`), `src/job_creator.rs` (`CreatedJob` → re-export from settlement, see Step 6; `fire_single_hook` caller path), `src/recovery.rs`, `src/web/worker_api/jobs.rs`, `src/web/api/jobs.rs`, `src/web/api/tasks.rs`, `src/web/hooks.rs`, `src/web/worker_api/event_source.rs`, `src/mcp/tools.rs`, `src/scheduler.rs`, `src/event_source.rs`, `src/cancellation.rs` (only its `handle_job_terminal` call → `state.settlement().advance(job_id)`; the move of `cancel_job` is Task 4)
- Delete: `src/job_recovery.rs`
- Test: `tests/propagate_to_parent_test.rs`, `tests/integration_test.rs`, `tests/mcp_test.rs`

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces: `Settlement`, `AppState::settlement()`, `CreatedJob` (still `Copy` with public flag until Task 5), `BornTerminal`, the entries `step_settled`, `step_failed`, `job_created`, `agent_child_created`, `agent_children_registered`, `worker_completed_job` (`cancel` arrives in Task 4), plus `pub` `advance`, `propagate`, `reconcile` (amendment A1).

- [ ] **Step 1: `terminal.rs` — drain, claim, plan, run**

Move from `job_recovery.rs` verbatim, renaming only the function names and the `state: &AppState` parameter to `s: &Settlement` (with `s.pool`, `s.server_log(...)` in place of `state.pool`, `state.append_server_log(...)`):

- `claim_terminal_handling` (`:39-107`) → `pub(super) async fn claim(s: &Settlement, job: &JobRow) -> bool`
- `drained_for_terminal_handling` (`:109-130`) → `pub(super) async fn drained(s: &Settlement, job_id: Uuid) -> bool`
- `meta_from_job` (`:131-137`), `run_terminal_job_actions` (`:902-958`) → `pub(super) async fn run_terminal_actions(s, job, workspace, task)`, `build_minimal_task_def` (`:960-997`), `get_hook_error_summary` (`:999-1005`), `extract_first_failure` (`:1007-1021`) and its four unit tests, `upload_logs_for_job` (`:1195-1210`).

Then add the pure plan with its tests:

```rust
/// Which terminal side effects a settled job gets. Pure; see spec §6.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Success,
    Error,
    Cancel,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalPlan {
    /// The job has a parent step to mark.
    pub propagate: bool,
    /// Failed, top-level, and `retry_attempt < max_retries`.
    pub retry: bool,
    /// By status; `None` when `retry` is true (hooks fire only once retries
    /// are exhausted) or when the status has no hook kind (`skipped`).
    pub hooks: HookKind,
}

pub fn plan(job: &JobRow) -> TerminalPlan {
    let status = job.status.parse::<JobStatus>().ok();
    let propagate = job.parent_job_id.is_some() && job.parent_step_name.is_some();
    let retry = status == Some(JobStatus::Failed)
        && job.parent_job_id.is_none()
        && job.max_retries.is_some_and(|max| job.retry_attempt < max);
    let hooks = if retry {
        HookKind::None
    } else {
        match status {
            Some(JobStatus::Completed) => HookKind::Success,
            Some(JobStatus::Failed) => HookKind::Error,
            Some(JobStatus::Cancelled) => HookKind::Cancel,
            _ => HookKind::None,
        }
    };
    TerminalPlan { propagate, retry, hooks }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn job(status: &str, parent: bool, attempt: i32, max: Option<i32>) -> JobRow {
        let mut j = JobRow::test_default();
        j.status = status.to_string();
        if parent {
            j.parent_job_id = Some(Uuid::new_v4());
            j.parent_step_name = Some("child".to_string());
        }
        j.retry_attempt = attempt;
        j.max_retries = max;
        j
    }

    #[test]
    fn failed_top_level_with_budget_retries_without_hooks() {
        let p = plan(&job("failed", false, 0, Some(2)));
        assert_eq!(p, TerminalPlan { propagate: false, retry: true, hooks: HookKind::None });
    }

    #[test]
    fn failed_top_level_exhausted_fires_error_hooks() {
        let p = plan(&job("failed", false, 2, Some(2)));
        assert_eq!(p, TerminalPlan { propagate: false, retry: false, hooks: HookKind::Error });
    }

    #[test]
    fn failed_child_propagates_and_never_retries() {
        let p = plan(&job("failed", true, 0, Some(2)));
        assert_eq!(p, TerminalPlan { propagate: true, retry: false, hooks: HookKind::Error });
    }

    #[test]
    fn cancelled_fires_cancel_hooks() {
        assert_eq!(plan(&job("cancelled", false, 0, None)).hooks, HookKind::Cancel);
    }

    #[test]
    fn completed_fires_success_hooks() {
        assert_eq!(plan(&job("completed", false, 0, None)).hooks, HookKind::Success);
    }

    #[test]
    fn null_max_retries_never_retries() {
        assert!(!plan(&job("failed", false, 0, None)).retry);
    }
}
```

`JobRow::test_default()` is added to `crates/stroem-db/src/repos/job.rs` the same way as `JobStepRow::test_default` in Task 1 (`#[doc(hidden]`, every `Option` `None`, `status: "pending"`, `source_type: "api"`, `mode: "distributed"`, `retry_attempt: 0`, fresh `job_id`).

`run_terminal_actions` must select hooks by `HookKind`, not by re-deriving from status: change its `crate::hooks::fire_hooks(state, workspace, job, task)` call to `hooks::fire_hooks(s, workspace, job, task)` and keep `fire_hooks`'s own status mapping as is (it returns early for a status with no hooks, which agrees with the plan for every status the plan can carry). The plan's `hooks` field is what `advance` checks to decide whether to call `run_terminal_actions` at all in the retry case; do not add a second status match.

- [ ] **Step 2: `retry.rs`**

Move verbatim from `job_recovery.rs`: `retry_log_line` (`:141-165`), `step_retry_message`, `step_retries_exhausted_message`, `task_retry_message` (`:1024-1068`), `compute_retry_delay` (`:1070-1089`) with their unit tests (`fail_step_log_line_uses_pre_increment_attempt`, `retry_messages_count_executions_consistently`, the five `test_compute_retry_delay_*`), and `try_retry_job` (`:1091-1193`) as:

```rust
/// Create the retry job for a failed top-level job and link the two rows.
/// Returns the created job for the caller to finalize through
/// `Settlement::job_created`. Spec §8.1.
pub(super) async fn create_retry_job(
    s: &Settlement,
    failed_job: &JobRow,
    workspace: &WorkspaceConfig,
    task: &TaskDef,
) -> Result<Option<CreatedJob>>
```

Body identical to `try_retry_job` except: `state.` → `s.`, `crate::config::JobDefaults::from(state.config.as_ref())` → `s.defaults`, `state.append_server_log` → `s.server_log`, the two early `return Ok(false)` become `return Ok(None)`, the final `finalize_created_job(state, created).await; Ok(true)` becomes `Ok(Some(created))`. All strings unchanged.

`retry_log_line` and `compute_retry_delay` stay `pub(crate)`; `web/api/jobs.rs:1089,1110` (approval reject) keep calling them at their new path `crate::settlement::retry::…` until Step 7 converts reject to `step_failed`.

- [ ] **Step 3: `hooks.rs`**

`git mv crates/stroem-server/src/hooks.rs crates/stroem-server/src/settlement/hooks.rs`. Change every `state: &AppState` parameter in `fire_hooks`, `fire_suspended_hooks`, `build_hook_context`, `list_hook_artifacts`, `fire_single_hook` to `s: &Settlement`, with `state.pool` → `s.pool`, `state.workspaces` → `s.workspaces`, `crate::config::JobDefaults::from(state.config.as_ref())` → `s.defaults`, `state.append_server_log` → `s.server_log`, and `crate::job_recovery::finalize_created_job(state, created)` → `Box::pin(s.job_created(created))`. Nothing else in the file changes; `is_top_level_source`, `MAX_HOOK_CHAIN_DEPTH`, `hook_chain_depth`, the context structs and all 25 unit tests stay. Update the `test_app_state_with_workspaces`-based unit tests in the file to build `Settlement` via `state.settlement()`.

- [ ] **Step 4: `propagate.rs`**

```rust
impl Settlement {
    /// Child `child` settled: mark the parent step and advance the parent.
    ///
    /// For `agent_tool` children this is gated on a **registration barrier**
    /// (doc moved verbatim from `job_recovery::propagate_to_parent`).
    ///
    /// Public so tests can drive a job from an arbitrary row state;
    /// production code goes through the entries.
    pub async fn propagate(
        &self,
        child: &JobRow,
        parent_job_id: Uuid,
        parent_step: &str,
    ) -> Result<()> {
        // ← lines 445-573 of job_recovery.rs verbatim (agent branch through the
        //   three JobStepRepo::mark_* calls on the parent step), with
        //   `state.pool` → `self.pool`.
        Box::pin(self.advance(parent_job_id)).await
    }
}
```

The parent-side copy of the twelve steps (`job_recovery.rs:575-755`) is deleted; `advance` is that copy.

- [ ] **Step 5: `mod.rs` — struct, `server_log`, `advance`, entries**

```rust
pub mod dispatch;
pub mod hooks;
mod propagate;
pub mod retry;
pub mod settle;
pub mod terminal;

pub use settle::{cascade_and_settle, settle_if_all_terminal, Settled};
pub use crate::job_creator::CreatedJob; // moved here in Task 5

use crate::config::JobDefaults;
use crate::events::EventBus;
use crate::job_completion::{JobCompletionEvent, JobCompletionNotifier};
use crate::log_broadcast::LogBroadcast;
use crate::log_storage::LogStorage;
use crate::state::AppState;
use crate::workspace::WorkspaceManager;
use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use stroem_common::models::job::{JobStatus, SourceType, StepStatus};
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{FailOutcome, JobRepo, JobRow, JobStepRepo};
use uuid::Uuid;

/// The settlement module's dependencies: seven of `AppState`'s fields.
#[derive(Clone)]
pub struct Settlement {
    pub(crate) pool: PgPool,
    pub(crate) workspaces: Arc<WorkspaceManager>,
    pub(crate) defaults: JobDefaults,
    pub(crate) log_storage: Arc<LogStorage>,
    pub(crate) log_broadcast: Arc<LogBroadcast>,
    pub(crate) job_completion: Arc<JobCompletionNotifier>,
    pub(crate) cancelled_jobs: Arc<RwLock<HashSet<Uuid>>>,
    pub(crate) event_bus: EventBus,
}

/// An agent tool child that can never deliver a tool result (spec §6.5).
#[derive(Debug)]
pub struct BornTerminal {
    pub job_id: Uuid,
    pub status: String,
}

impl AppState {
    /// Cheap: clones of `Arc`s and a pool handle.
    pub fn settlement(&self) -> Settlement {
        Settlement {
            pool: self.pool.clone(),
            workspaces: self.workspaces.clone(),
            defaults: JobDefaults::from(self.config.as_ref()),
            log_storage: self.log_storage.clone(),
            log_broadcast: self.log_broadcast.clone(),
            job_completion: self.job_completion.clone(),
            cancelled_jobs: self.cancelled_jobs.clone(),
            event_bus: self.event_bus.clone(),
        }
    }
}

impl Settlement {
    /// Private copy of `AppState::append_server_log` (same JSONL shape, same
    /// broadcast, same best-effort NOTIFY).
    pub(crate) async fn server_log(&self, job_id: Uuid, message: &str) {
        // ← body of AppState::append_server_log (state.rs:205-228) verbatim,
        //   `self.` fields are the same names.
    }

    /// Resolve the workspace config and task for a job row, with the
    /// hook / event-source minimal-task fallback. `None` when either is
    /// missing (already logged).
    async fn resolve(&self, job: &JobRow) -> Result<Option<(Arc<WorkspaceConfig>, TaskDef)>> {
        let Some(workspace) = self.workspaces.get_config(&job.workspace).await else {
            tracing::error!("Workspace '{}' not found", job.workspace);
            return Ok(None);
        };
        let task = match workspace.tasks.get(&job.task_name) {
            Some(t) => t.clone(),
            None if job.source_type == SourceType::Hook.as_ref()
                || job.source_type == SourceType::EventSource.as_ref() =>
            {
                terminal::build_minimal_task_def(self, job.job_id).await?
            }
            None => {
                tracing::error!(
                    "Task '{}' not found in workspace '{}'",
                    job.task_name,
                    job.workspace
                );
                return Ok(None);
            }
        };
        Ok(Some((workspace, task)))
    }

    /// Move `job_id` as far as its rows allow. Spec §6.3. Idempotent: every
    /// effect is guarded (cascade guards, settlement re-read, drain gate,
    /// claim).
    ///
    /// Public so tests can drive a job from an arbitrary row state;
    /// production code goes through the entries.
    #[tracing::instrument(skip(self))]
    pub async fn advance(&self, job_id: Uuid) -> Result<()> {
        let Some(job) = JobRepo::get(&self.pool, job_id).await? else {
            tracing::warn!("Job {} not found during orchestration", job_id);
            return Ok(());
        };
        let Some((workspace, task)) = self.resolve(&job).await? else {
            return Ok(());
        };

        // Step 3 — once, not a loop (spec §6.3).
        if !is_terminal(&job.status) {
            cascade_and_settle(&self.pool, job_id, &task, &workspace).await?;

            if let Err(e) = dispatch::handle_task_steps(
                &self.workspaces, &self.pool, &workspace, &job.workspace, job_id, &task, self.defaults,
            )
            .await
            {
                tracing::error!("Failed to handle task steps for job {}: {:#}", job_id, e);
                self.server_log(job_id, &format!("[orchestration] Failed to handle task steps: {:#}", e)).await;
            }
            self.reconcile(job_id).await;
            self.dispatch_approvals(&job, &workspace, &task).await?;
        }

        // Step 4.
        let Some(job) = JobRepo::get(&self.pool, job_id).await? else { return Ok(()) };
        if !is_terminal(&job.status) {
            return Ok(());
        }
        if !terminal::drained(self, job_id).await {
            return Ok(());
        }
        crate::cancellation::clear_cancelled_in(&self.cancelled_jobs, job_id);
        if !terminal::claim(self, &job).await {
            tracing::debug!(job_id = %job_id, "advance: terminal handling already claimed, skipping");
            return Ok(());
        }

        let plan = terminal::plan(&job);
        if plan.propagate {
            let (Some(parent_job_id), Some(ref parent_step)) = (job.parent_job_id, &job.parent_step_name) else { unreachable!() };
            if let Err(e) = self.propagate(&job, parent_job_id, parent_step).await {
                tracing::error!("Failed to propagate child job {} to parent {}: {:#}", job.job_id, parent_job_id, e);
                self.server_log(job_id, &format!("[orchestration] Failed to propagate to parent job {}: {:#}", parent_job_id, e)).await;
            }
        }
        if plan.retry {
            match retry::create_retry_job(self, &job, &workspace, &task).await {
                Ok(Some(created)) => {
                    terminal::upload_logs_for_job(self, &job).await;
                    self.job_completion
                        .notify(JobCompletionEvent { job_id, status: job.status.clone(), output: job.output.clone() })
                        .await;
                    Box::pin(self.job_created(created)).await;
                    return Ok(());
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("Failed to create retry job for {}: {:#}", job_id, e);
                    self.server_log(job_id, &format!("[retry] Failed to create retry job: {:#}", e)).await;
                }
            }
        }
        terminal::run_terminal_actions(self, &job, &workspace, &task).await;
        Ok(())
    }

    /// The approval block of `orchestrate_after_step` (job_recovery.rs:262-322)
    /// verbatim, with `state.` → `self.` and `crate::hooks::` → `hooks::`.
    async fn dispatch_approvals(&self, job: &JobRow, workspace: &WorkspaceConfig, task: &TaskDef) -> Result<()> { /* … */ }

    /// Advance every descendant that settled at creation under a still-running
    /// parent step (`reconcile_settled_children`, job_recovery.rs:781-810,
    /// with `handle_job_terminal` → `self.advance`).
    ///
    /// Public so tests can drive a job from an arbitrary row state.
    pub async fn reconcile(&self, root_job_id: Uuid) { /* … */ }

    // ── Entries (spec §6.2) ────────────────────────────────────────────

    pub async fn step_settled(&self, job_id: Uuid, step_name: &str) -> Result<()> {
        tracing::info!("Orchestrating after step '{}' completed", step_name);
        self.advance(job_id).await
    }

    pub async fn step_failed(&self, job_id: Uuid, step_name: &str, error: &str, expected: &[StepStatus]) -> Result<FailOutcome> {
        let outcome = JobStepRepo::fail_or_retry(&self.pool, job_id, step_name, error, expected, retry::compute_retry_delay)
            .await
            .with_context(|| format!("fail_or_retry for step '{}' of job {}", step_name, job_id))?;
        if let Some(line) = retry::retry_log_line(step_name, &outcome) {
            self.server_log(job_id, &line).await;
        }
        if matches!(outcome, FailOutcome::Failed { .. }) {
            self.step_settled(job_id, step_name).await?;
        }
        Ok(outcome)
    }

    pub async fn job_created(&self, created: CreatedJob) {
        if created.terminal_at_creation {
            if let Err(e) = Box::pin(self.advance(created.job_id)).await {
                tracing::error!(job_id = %created.job_id, "terminal handling after creation failed: {:#}", e);
            }
        }
        self.reconcile(created.job_id).await;
    }

    pub async fn agent_child_created(&self, created: CreatedJob) -> Result<Uuid, BornTerminal> {
        self.reconcile(created.job_id).await;
        let status = JobRepo::get(&self.pool, created.job_id).await.ok().flatten().map(|j| j.status).unwrap_or_else(|| "unknown".to_string());
        if created.terminal_at_creation || is_terminal(&status) {
            return Err(BornTerminal { job_id: created.job_id, status });
        }
        Ok(created.job_id)
    }

    pub async fn agent_children_registered(&self, job_id: Uuid, step_name: &str) {
        // ← body of the replay loop in web/worker_api/jobs.rs (the function
        //   around :1248-1320) verbatim, with propagate_to_parent → self.propagate.
    }

    pub async fn worker_completed_job(&self, job_id: Uuid, output: Option<serde_json::Value>) -> Result<()> {
        JobRepo::mark_completed(&self.pool, job_id, output).await.context("mark job completed")?;
        self.advance(job_id).await
    }
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status.parse::<JobStatus>().ok(),
        Some(JobStatus::Completed) | Some(JobStatus::Failed) | Some(JobStatus::Cancelled) | Some(JobStatus::Skipped)
    )
}
```

`crate::cancellation::clear_cancelled_in(set: &RwLock<HashSet<Uuid>>, job_id)` is a new one-line helper in `cancellation.rs`; the existing `clear_cancelled(state, job_id)` delegates to it. The `step_failed` body is `job_recovery::fail_step` plus the orchestrate call; the `job_created` body is `finalize_created_job` with `handle_job_terminal` → `advance`.

Doc-comment `advance` with the ordering rationale from `orchestrate_after_step`'s inline comments (drain before claim, `clear_cancelled` behind the gate, claim is one-shot so propagate errors never abort the rest, archive after hooks).

- [ ] **Step 6: Switch every caller and delete `job_recovery.rs`**

| site | before | after |
|---|---|---|
| `recovery.rs:93,148,201,269` + the four `orchestrate_after_step` | `fail_step(...)` then `if Failed { orchestrate_after_step }` | `state.settlement().step_failed(job_id, &step_name, &error_msg, &[]).await?` (keep the `tracing::error!` on `Err` as the outer error handling) |
| `web/worker_api/jobs.rs:379-390` (render failure) and `:924-944` (`complete_step`) | same pair | `step_failed`; the success branch of `complete_step` → `state.settlement().step_settled(&job_id, &step_name)` |
| `web/api/jobs.rs:1042` (approve) | `orchestrate_after_step` | `step_settled` |
| `web/api/jobs.rs:1076-1120` (reject) | inline `fail_or_retry` + `retry_log_line` + `orchestrate_after_step` | keep the `[approval] … rejected` log line, then `state.settlement().step_failed(job_id, &step_name, &reason, &[StepStatus::Suspended]).await?` (check the exact `expected` slice the inline call passes and keep it) |
| `web/api/tasks.rs:547`, `web/api/jobs.rs:811`, `web/hooks.rs:143`, `web/worker_api/event_source.rs:129`, `mcp/tools.rs:538`, `scheduler.rs:447`, `event_source.rs:629` | `finalize_created_job(state, created)` | `state.settlement().job_created(created)` |
| `web/worker_api/jobs.rs:1039` (`complete_job`) | `mark_completed` + `handle_job_terminal` | `state.settlement().worker_completed_job(job_id, req.output.map(into_exposed)).await` with the same error log |
| `web/worker_api/jobs.rs:1110` (`agent_task_tool`) | `reconcile_settled_children` + inline terminal check + 500 | `match state.settlement().agent_child_created(created).await { Ok(id) => …, Err(BornTerminal { job_id, status }) => return the same 500 with the same message }` |
| `web/worker_api/jobs.rs:1248-1320` (the replay helper) | loop over `propagate_to_parent` | body moved into `agent_children_registered`; the helper becomes a one-line call from `agent_save_state` and `agent_suspend_step` |
| `web/worker_api/jobs.rs:1204` | `crate::hooks::fire_suspended_hooks(&state, …)` | `crate::settlement::hooks::fire_suspended_hooks(&state.settlement(), …)` |
| `cancellation.rs:127` | `job_recovery::handle_job_terminal(state, job_id)` | `state.settlement().advance(job_id)` (still conditional here; Task 4 makes it unconditional) |
| `job_creator.rs` `fire_initial_suspended_hooks` caller and any `crate::hooks::` path | | `crate::settlement::hooks::` |

Delete `src/job_recovery.rs`; remove `pub mod hooks;` and `pub mod job_recovery;` from `lib.rs`. `grep -rn "job_recovery\|crate::hooks::\|orchestrate_after_step\|handle_job_terminal\|finalize_created_job\|reconcile_settled_children\|propagate_to_parent" crates/stroem-server/src` must be empty except inside doc comments you deliberately kept (update those to the new names).

Tests: `tests/propagate_to_parent_test.rs:16` imports become `use stroem_server::settlement::Settlement;` and calls become `state.settlement().step_settled(...)` / `state.settlement().advance(...)`. `tests/integration_test.rs:25239` → `state.settlement().propagate(&child, job_id, "think")`, `:25667,26145` → `job_created`, `:25681,25682,26155,26159` → `reconcile`; the twelve `stroem_server::hooks::fire_hooks(&state, …)` → `stroem_server::settlement::hooks::fire_hooks(&state.settlement(), …)` (same in `mcp_test.rs:1742`).

- [ ] **Step 7: Run everything**

Run: `cargo fmt --all && cargo clippy --workspace -- -D warnings && cargo test -p stroem-server`
Expected: green; unit test count rises by 6 (`plan_tests`), container counts unchanged.

- [ ] **Step 8: Commit**

```bash
git add -A crates/stroem-server crates/stroem-db
git commit -m "refactor(settlement): state tier — Settlement, advance, six entries; delete job_recovery.rs and the two duplicate terminal blocks"
```

---

### Task 4: Move `cancel_job` into the module

**Files:**
- Modify: `crates/stroem-server/src/cancellation.rs` (remove `cancel_job` `:36-139`, keep `CancelResult`? No: move the enum too), `crates/stroem-server/src/settlement/mod.rs`, callers `web/api/jobs.rs:638-646`, `mcp/tools.rs:762-767`, `recovery.rs:242`, `scheduler.rs:378`, `event_source.rs:186,204,235`; tests `ha_test.rs:866`, `metrics_test.rs:676`

**Interfaces:**
- Produces: `Settlement::cancel(&self, job_id) -> Result<CancelResult>`, `settlement::CancelResult` (moved enum).

- [ ] **Step 1: Move**

Move `CancelResult` and `cancel_job`'s body into `settlement/mod.rs` as `pub async fn cancel(&self, job_id: Uuid) -> Result<CancelResult>`, verbatim with `state.` → `self.`, `state.append_server_log` → `self.server_log`, `Box::pin(cancel_job(state, child.job_id))` → `Box::pin(self.cancel(child.job_id))`, and the final block

```rust
    if !has_running_steps {
        if let Err(e) = crate::job_recovery::handle_job_terminal(state, job_id).await { … }
    }
```

replaced by the unconditional

```rust
    // Unconditional: the drain gate inside `advance` returns early while a
    // worker still owns a step, which is exactly the old `!has_running_steps`
    // condition, now in one place (spec §6.7).
    if let Err(e) = self.advance(job_id).await {
        tracing::error!(
            "Failed to handle terminal state for cancelled job {}: {:#}",
            job_id,
            e
        );
    }
```

`cancellation.rs` keeps `is_cancelled`, `clear_cancelled`, `clear_cancelled_in`, and its ten unit tests. Update the eight callers to `state.settlement().cancel(job_id)` and `crate::settlement::CancelResult::…`; the two tests likewise.

- [ ] **Step 2: Run**

Run: `cargo clippy --workspace -- -D warnings && cargo test -p stroem-server --test integration_test --test ha_test --test metrics_test`
Expected: green. `cascading_cancel_counts_parent_exactly_once`, `test_cancel_cascade_fires_parent_on_cancel_hook_exactly_once`, `test_cancel_job_with_suspended_step` and `test_terminal_handling_waits_for_live_steps_to_drain` are the discriminating ones.

- [ ] **Step 3: Commit**

```bash
git add -A crates/stroem-server
git commit -m "refactor(settlement): cancel is a settlement entry; cancellation.rs keeps only the cancelled-jobs set"
```

---

### Task 5: `CreatedJob` privatization

**Files:**
- Modify: `crates/stroem-server/src/settlement/mod.rs` (define `CreatedJob` here), `crates/stroem-server/src/job_creator.rs:38-42,80-115,221-255` (delete the two `_id` wrappers; construct via `CreatedJob::new`), every test using `create_job_for_task(` / `create_child_job_for_task(` (grep `tests/`)

**Interfaces:**
- Produces: `settlement::CreatedJob { pub job_id, terminal_at_creation (private) }`, `CreatedJob::new(job_id, terminal_at_creation)` `pub(crate)`.

- [ ] **Step 1: Move and privatize**

In `settlement/mod.rs`:

```rust
/// Result of job creation. Consume it with `Settlement::job_created` (or
/// `agent_child_created` for agent tool children); nothing else can act on
/// the terminal-at-creation flag.
///
/// Residual hole: `create_job_for_task_detailed(..).await?.job_id` moves the
/// id out and drops the struct without finalizing it; `#[must_use]` does not
/// catch field access. Reviewers: every creation site ends in `job_created`.
#[must_use = "pass to Settlement::job_created or agent_child_created"]
#[derive(Debug)]
pub struct CreatedJob {
    pub job_id: Uuid,
    terminal_at_creation: bool,
}

impl CreatedJob {
    pub(crate) fn new(job_id: Uuid, terminal_at_creation: bool) -> Self {
        Self { job_id, terminal_at_creation }
    }
}
```

Remove the struct from `job_creator.rs`; replace its two literal constructions with `CreatedJob::new(job_id, true)` / `CreatedJob::new(job_id, settled.is_some())`; `use crate::settlement::CreatedJob;`. Delete `create_job_for_task` and `create_child_job_for_task` (the `_id`-returning wrappers, `job_creator.rs:80-115` and `:221-255`). Convert their test callers: `let job_id = create_job_for_task(...)` → `let job_id = create_job_for_task_detailed(...).await?.job_id;` where the test only needs the id and never expected finalization, or `let created = …; state.settlement().job_created(created).await;` where it did (read each test; the ones that later assert on hooks or metrics need the second form). `agent_child_created` and `job_created` already take the struct by value.

- [ ] **Step 2: Run**

Run: `cargo clippy --workspace -- -D warnings && cargo test -p stroem-server`
Expected: green; no `unused_must_use` warnings.

- [ ] **Step 3: Commit**

```bash
git add -A crates/stroem-server
git commit -m "refactor(settlement): CreatedJob is non-Copy with a private flag; drop the id-returning creation wrappers"
```

---

### Task 6: D1 — one failure policy, with regression test

D1 is already in force after Task 3 (one body). This task pins it.

**Files:**
- Test: `crates/stroem-server/tests/integration_test.rs`

- [ ] **Step 1: Write the regression test**

Model it on `test_task_step_dispatch_failure_cascades_and_fails_job` (`integration_test.rs:24316`) and `test_child_settled_at_creation_propagates_to_parent` (`:24670`), which together already build a parent with a `type: task` step and an `on_error` hook. New test:

```rust
/// D1 (spec §6.4): a parent whose `type: task` step dispatch fails during the
/// PARENT leg of propagation (child settles → parent step marked → parent
/// advances → its next task step names an unknown task) still settles, fires
/// its on_error hook and increments the completion counter exactly once.
/// Before the settlement module the parent leg returned the dispatch error
/// with `?`, skipping the parent's drain, claim and terminal actions forever.
#[tokio::test]
async fn test_parent_dispatch_failure_after_child_settles_still_runs_terminal_actions() -> Result<()> {
    // Workspace: task `parent` with flow { first: type task → `child`, second (depends_on first): type task → `missing` }.
    // `child` is a one-step script task. `parent` has on_error: [{ action: "record" }] where `record` is a script action.
    // 1. Execute `parent`; assert `first` dispatched a child job.
    // 2. Complete the child's step through the worker API; this propagates.
    // 3. Assert: parent job status == "failed"; parent step `second` == "failed" with error containing "Task 'missing' not found";
    //    exactly one hook job with source_id == parent id and task_name == "_hook:record";
    //    parent's metrics_recorded_at IS NOT NULL (claim taken).
    Ok(())
}
```

Fill the body using the same helpers those two tests use (`setup_multi_workspace_with` or the single-workspace `setup`, the worker-API `complete_step` call shape from `test_recovery_propagates_to_parent`). The assertion that discriminates is the hook job's existence: under the old parent leg it was never created.

- [ ] **Step 2: Prove the test discriminates**

The old behaviour no longer exists on the branch, so reproduce it for one run: temporarily insert `return Err(anyhow::anyhow!("simulated dispatch abort"));` directly after the `[orchestration] Failed to handle task steps` server-log line in `advance` (that is what the old parent leg's `?` did), run the test, confirm it fails on the missing hook job, then revert the line. Say in the report that you did this and what the failure message was.

- [ ] **Step 3: Run and commit**

Run: `cargo test -p stroem-server --test integration_test test_parent_dispatch_failure`
Expected: pass.

```bash
git add crates/stroem-server/tests/integration_test.rs
git commit -m "test(settlement): D1 — parent dispatch failure after child settlement still runs terminal actions"
```

---

### Task 7: D2 — `JobRepo::settle`

**Files:**
- Modify: `crates/stroem-db/src/repos/job.rs` (add `settle` next to `mark_completed`), `crates/stroem-db/README.md`, `crates/stroem-server/src/settlement/settle.rs` (`settle_if_all_terminal`), `crates/stroem-server/src/settlement/mod.rs` (`worker_completed_job`)
- Test: `crates/stroem-db/tests/integration_test.rs`

- [ ] **Step 1: Write the repo test**

In `crates/stroem-db/tests/integration_test.rs`, next to the existing `JobRepo` tests:

```rust
#[tokio::test]
async fn settle_writes_once_and_never_overwrites_a_terminal_row() -> Result<()> {
    let (pool, _c) = setup().await?;
    let job_id = JobRepo::create(&pool, "ws", "t", "distributed", None, "api", None, None, None).await?;

    // running → completed with output
    assert!(JobRepo::mark_running_if_pending(&pool, job_id).await?);
    let wrote = JobRepo::settle(&pool, job_id, JobStatus::Completed, Some(serde_json::json!({"k": 1}))).await?;
    assert!(wrote);
    let j = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(j.status, "completed");
    assert_eq!(j.output, Some(serde_json::json!({"k": 1})));
    let first_completed_at = j.completed_at.unwrap();

    // a second settle is a no-op
    assert!(!JobRepo::settle(&pool, job_id, JobStatus::Failed, None).await?);
    let j = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(j.status, "completed");
    assert_eq!(j.completed_at.unwrap(), first_completed_at);

    // cancelled rows are never overwritten (spec §7 regression)
    let job2 = JobRepo::create(&pool, "ws", "t", "distributed", None, "api", None, None, None).await?;
    assert!(JobRepo::cancel(&pool, job2).await?);
    assert!(!JobRepo::settle(&pool, job2, JobStatus::Completed, None).await?);
    assert_eq!(JobRepo::get(&pool, job2).await?.unwrap().status, "cancelled");
    Ok(())
}
```

Check the exact `JobRepo::create` arity in the file's other tests and match it.

- [ ] **Step 2: Implement**

```rust
    /// Predicated settlement write (spec §7): moves a `pending`/`running` job
    /// to `status` and returns whether the row was written. A `false` means
    /// the row was already terminal — typically an explicit cancellation —
    /// and must not be overwritten. `output` is `COALESCE`d so `failed` and
    /// `cancelled` (which pass `None`) never clear an existing output.
    pub async fn settle(
        pool: &PgPool,
        job_id: Uuid,
        status: JobStatus,
        output: Option<JsonValue>,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE job
            SET status = $2, output = COALESCE($3, output), completed_at = NOW()
            WHERE job_id = $1 AND status IN ('pending', 'running')
            "#,
        )
        .bind(job_id)
        .bind(status.as_ref())
        .bind(output)
        .execute(pool)
        .await
        .context("Failed to settle job")?;
        Ok(result.rows_affected() > 0)
    }
```

(`JobStatus` is in `stroem_common::models::job`; add the import if missing.)

In `settle_if_all_terminal`, delete the "Never overwrite an explicit cancellation" re-read and replace the three-arm `match` with:

```rust
    let wrote = JobRepo::settle(pool, job_id, settled.status.clone(), settled.output.clone())
        .await
        .context("Failed to settle job")?;
    if !wrote {
        let current = JobRepo::get(pool, job_id).await?.map(|j| j.status);
        tracing::info!(job_id = %job_id, ?current, "job already terminal, settlement not written");
        return Ok(current.and_then(|s| s.parse::<JobStatus>().ok()));
    }
    // keep the three tracing::info! lines by status exactly as before, then:
    Ok(Some(settled.status))
```

In `worker_completed_job`, replace `JobRepo::mark_completed(...)` with `JobRepo::settle(&self.pool, job_id, JobStatus::Completed, output)` and ignore the bool (a worker completing an already-cancelled job must not resurrect it; `advance` then runs the drain and claim as before).

Document `settle` in `crates/stroem-db/README.md` under the `JobRepo` list.

- [ ] **Step 3: Run**

Run: `cargo test -p stroem-db settle_writes_once && cargo test -p stroem-server --test orchestrator_test --test integration_test`
Expected: green. `test_settle_cancelled_step_without_failure_marks_job_cancelled` and the cancel tests are the ones that exercise the no-overwrite path.

- [ ] **Step 4: Commit**

```bash
git add -A crates/stroem-db crates/stroem-server
git commit -m "feat(settlement): D2 — predicated JobRepo::settle replaces the cancellation re-read"
```

---

### Task 8: D3 — task-level retry persisted at creation, decided on every path

**Files:**
- Modify: `crates/stroem-db/src/repos/job.rs` (`create_with_parent_tx` / `create_with_parent_tx_id` gain `max_retries: Option<i32>`; `create_with_parent` and `create` pass `None`), `crates/stroem-server/src/job_creator.rs` (the creation insert), `crates/stroem-server/src/settlement/retry.rs` (drop `max_retries` from the linking `UPDATE`), `crates/stroem-db/README.md`, `CLAUDE.md` § Retry Mechanism (the "non-functional" bullet), `docs/internal/TODO.md`
- Test: `crates/stroem-server/tests/integration_test.rs`

- [ ] **Step 1: Write the three regression tests**

Next to `test_task_retry_creates_new_job_on_failure` (`integration_test.rs:23218`), which seeds `max_retries` with raw SQL. The new tests must contain **no** `UPDATE job SET max_retries`:

```rust
/// D3 (spec §8.1): a task with `retry: { max_attempts: 2 }` persists
/// `job.max_retries = 1` at creation and, when its step fails through the
/// worker path, produces a retry job — with no raw-SQL seeding.
#[tokio::test]
async fn test_task_retry_is_persisted_at_creation_and_fires_on_worker_failure() -> Result<()> { /* copy the body of test_task_retry_creates_new_job_on_failure, delete the UPDATE, add: assert_eq!(job.max_retries, Some(1)) right after creation */ Ok(()) }

/// D3: the same task whose only root step is `type: task` naming an unknown
/// task fails AT CREATION (compensation path) and still gets a retry job.
/// Before the settlement module task retry existed only on the worker path.
#[tokio::test]
async fn test_task_retry_fires_for_a_job_that_fails_at_creation() -> Result<()> {
    // task `flaky` { retry: { max_attempts: 2 }, flow: { first: action `call-missing` (type task, task: "does-not-exist") } }
    // execute → created.terminal_at_creation; job_created runs advance.
    // assert: original status failed; exactly one job with retry_of_job_id == original and source_type == "retry".
    Ok(())
}

/// D3: with `max_attempts: 3`, a retry job that itself fails at creation is
/// finalized through `job_created` and gets a SECOND retry (spec §6.3 4.d.ii).
#[tokio::test]
async fn test_retry_job_that_fails_at_creation_is_retried_again() -> Result<()> {
    // same fixture with max_attempts: 3 → assert three jobs in the chain: original, retry 1 (retry_attempt 1), retry 2 (retry_attempt 2), and no fourth.
    Ok(())
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p stroem-server --test integration_test test_task_retry_is_persisted test_task_retry_fires_for test_retry_job_that_fails`
Expected: the first fails on `max_retries == None`; the other two find no retry job.

- [ ] **Step 3: Implement**

`job.rs`: add `max_retries: Option<i32>` as the last parameter of `create_with_parent_tx_id` and `create_with_parent_tx`, bind it as `$15` in the `INSERT` column list (`…, restart_from_step, max_retries`); `create_with_parent` and `create` pass `None`. Update every caller (`grep -rn "create_with_parent_tx" crates/`).

`job_creator.rs`: at the creation insert, pass

```rust
            task.retry
                .as_ref()
                .map(|r| i32::try_from(r.max_attempts - 1).expect("max_attempts fits i32")),
```

(same convention and comment as the step-level `max_retries` at `job_creator.rs:519`).

`retry.rs::create_retry_job`: the linking statement becomes `UPDATE job SET retry_of_job_id = $1, retry_attempt = $2 WHERE job_id = $3` (creation already wrote `max_retries`); keep `retry_job_id` back-link and `retry_at` as they are.

Docs: `crates/stroem-db/README.md` (new parameter); CLAUDE.md § Retry Mechanism: replace the "Task-level retry is currently non-functional in production" bullet with "`job.max_retries` is written at creation from `task.retry.max_attempts - 1` for every creation mode (child jobs carry it too but never retry: `terminal::plan` gates on top-level). The retry decision is part of the terminal plan, so it applies on every path into terminal handling, including jobs that fail at creation."; `docs/internal/TODO.md`: mark the task-retry entry `[x]`.

- [ ] **Step 4: Run**

Run: `cargo clippy --workspace -- -D warnings && cargo test -p stroem-db && cargo test -p stroem-server --test integration_test task_retry retry_job`
Expected: the three new tests and the five existing task-retry tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A crates CLAUDE.md docs/internal/TODO.md
git commit -m "feat(settlement): D3 — persist job.max_retries at creation; task retry decided on every terminal path"
```

---

### Task 9: D4 — hook jobs through the shared step builder

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs:499-560` (extract `build_step`), `crates/stroem-server/src/settlement/hooks.rs` (`fire_single_hook`, the non-task branch at former `hooks.rs:590-640`)
- Test: `crates/stroem-server/tests/integration_test.rs`

- [ ] **Step 1: Write the regression test**

Next to `test_hook_fires_on_job_success` (`:11692`):

```rust
/// D4 (spec §8.2): a single-action hook job's step carries the action's retry
/// config and the server default step timeout, like every other step.
/// Before, `fire_single_hook` hand-built the row with those fields `None`.
#[tokio::test]
async fn test_hook_job_step_gets_action_retry_and_default_timeout() -> Result<()> {
    // workspace: action `notify` { type script, retry: { max_attempts: 3, delay: 1s } }, task `t` { on_success: [{ action: notify }] }.
    // server config: default_step_timeout = 30s (see how test_retry_job_inherits_defaults builds a config with defaults).
    // run `t` to completion through the worker API, then find the hook job (task_name "_hook:notify") and its step "hook":
    // assert step.max_retries == Some(2), step.retry_backoff_secs == Some(1), step.timeout_secs == Some(30), step.status == "ready".
    Ok(())
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p stroem-server --test integration_test test_hook_job_step_gets`
Expected: fails on `max_retries == None`.

- [ ] **Step 3: Extract `build_step` and use it**

In `job_creator.rs`, lift the `NewJobStep { … }` literal at `:499-560` into

```rust
/// The one place a `NewJobStep` is built from a flow step and its resolved
/// action. Used by job creation and by hook-job creation (spec §8.2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_step(
    job_id: Uuid,
    step_name: &str,
    action_name: String,
    flow_step: &FlowStep,
    action: &ActionDef,
    input: Option<serde_json::Value>,
    status: StepStatus,
    defaults: JobDefaults,
    action_workspace: Option<String>,
    action_revision: Option<String>,
) -> NewJobStep
```

whose body is the literal verbatim, with `action_spec`, `required_ability`, `required_tags`, `runner` and `retry` computed inside from `action`/`flow_step` (move those five `let`s in). The creation loop calls it with `Some(serde_json::to_value(&flow_step.input).unwrap_or_default())` as `input` and the status it computed.

In `settlement/hooks.rs::fire_single_hook`, replace the hand-written `NewJobStep { … }` with

```rust
    let flow_step = FlowStep {
        action: hook.action.clone(),
        name: None,
        description: None,
        depends_on: vec![],
        input: HashMap::new(),
        continue_on_failure: false,
        timeout: None,
        when: None,
        for_each: None,
        sequential: false,
        retry: None,
        inline_action: None,
    };
    let step = crate::job_creator::build_step(
        job_id,
        "hook",
        hook.action.clone(),
        &flow_step,
        action,
        Some(rendered_input),
        StepStatus::Ready,
        defaults,
        None,
        None,
    );
```

and wrap the `JobRepo::create` + `JobStepRepo::create_steps` pair in one transaction (`create_with_parent_tx` with a `tx`, then a `create_steps_tx` if one exists, else keep `create_steps` on the pool after the commit — check `job_step.rs` for a `_tx` variant). After the insert, `Box::pin(s.job_created(CreatedJob::new(job_id, false))).await;` so the hook job is reconciled like every other created job. The `tracing::info!("Fired hook job …")` line stays.

- [ ] **Step 4: Run**

Run: `cargo clippy --workspace -- -D warnings && cargo test -p stroem-server --test integration_test hook`
Expected: the new test and the eleven hook tests pass; `test_hook_job_completes_through_orchestrator` in particular.

- [ ] **Step 5: Commit**

```bash
git add -A crates/stroem-server
git commit -m "feat(settlement): D4 — hook jobs built through job_creator::build_step and finalized via job_created"
```

---

### Task 10: Documentation, glossary, TODO, pruning

**Files:**
- Modify: `CONTEXT.md`, `CLAUDE.md`, `docs/internal/TODO.md`, `crates/stroem-db/README.md` (verify Tasks 7–8 entries), `crates/stroem-server/tests/*.rs` (pruning only)

- [ ] **Step 1: `CONTEXT.md`**

Replace the two existing entries (Settlement, Terminal handling) and add Claim, Drain gate, Reconcile, Advance with the exact wording of spec §3.

- [ ] **Step 2: `CLAUDE.md`**

Add a `### Settlement` section after `### Step Cascade` covering: the module layout table from this plan's File map; the seven entries and what each replaces; the `advance` order (cascade-and-settle → task dispatch → reconcile → approval dispatch → suspended hooks; then drain → clear cancel signal → claim → plan → propagate → retry-or-hooks → notify → close and archive); drain before claim and why (moved from § Task Actions); the reconcile CTE's three predicates (moved from § Task Actions); the `CreatedJob` obligation and residual hole; `JobRepo::settle`; task retry now functional (pointer to § Retry Mechanism); hook jobs via `build_step`; the `None` workspace-config mode removed and the `workspace_with` test helper. Replace the settlement paragraphs in § Task Actions ("Settlement is shared…", "Drain before claiming…", "Post-commit initialisation errors…" keeps its first sentence and points at `settlement::dispatch::init`), § Prometheus Metrics (the `claim_terminal_handling` bullet → "incremented inside `settlement::terminal::claim` — see § Settlement"), and § Agent Actions (the two barrier bullets keep their content but name `Settlement::propagate`, `agent_child_created`, `agent_children_registered`) with one-line pointers. Update § Step Cascade's "Callers" bullet (`orchestrator::on_step_completed` → `settlement::cascade_and_settle`; creation init → `settlement::dispatch::init`).

- [ ] **Step 3: `TODO.md`**

Mark the task-retry entry done (if Task 8 did not), add "Code Quality: `CreatedJob` residual hole — `…await?.job_id` drops the finalize obligation; consider returning the id only from `job_created`", keep the hardening-spec entry.

- [ ] **Step 4: Pruning**

Only tests fully covered by a new unit test may go. Candidates: `orchestrator_test.rs::test_settle_tolerated_failure_completes_with_aggregated_output` and `test_settle_cancelled_step_without_failure_marks_job_cancelled` are covered by `settle::tests::{tolerated_failure_completes_and_aggregates_the_rest, cancelled_without_failure_cancels}` — but they also exercise the DB write, so **keep them**. Delete nothing unless you can name the unit test that asserts every line of the container test; the expected outcome of this step is "nothing pruned" and a ledger note saying so.

- [ ] **Step 5: Full verification and commit**

Run: `cargo fmt --check --all && cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: green across the workspace (the known flaky `log_storage::tests` race aside; re-run once if it trips).

```bash
git add CONTEXT.md CLAUDE.md docs/internal/TODO.md crates/stroem-db/README.md
git commit -m "docs(settlement): glossary, CLAUDE.md Settlement section, TODO"
```

---

## Self-review

**Spec coverage.** §3 → Task 10. §4 layout → Tasks 1–4. §5.1 `decide`+tests → Task 1. §5.2 required config + `workspace_with` → Task 1. §5.3 dispatch + `init` → Task 2. §6.1 struct on demand → Task 3. §6.2 seven entries → Tasks 3 (six) and 4 (`cancel`). §6.3 `advance` incl. the retry `job_created` call → Task 3. §6.4 D1 → Task 6. §6.5 propagate, agent entries → Task 3. §6.6 plan + tests → Task 3. §6.7 unconditional advance in cancel → Task 4. §7 → Task 7. §8.1 → Task 8. §8.2 → Task 9. §9 → Task 5. §10 → Tasks 7, 8, 10. §11 tests → each task; pruning → Task 10. §12 order matches (D1 is Task 6 rather than folded into Task 3, because its test needs the cancel move from Task 4 to be settled first).

**Placeholders.** The `advance` listing marks three bodies with "← … verbatim" pointing at exact source lines; those are moves, not gaps. Task 6 and Task 8 test skeletons carry comments describing the fixture and the assertions instead of full bodies because they are copies of named neighbouring tests with one change each; the implementer is told which test to copy.

**Type consistency.** `cascade_and_settle(pool, job_id, task, &WorkspaceConfig) -> Result<Option<JobStatus>>` everywhere. `create_retry_job -> Result<Option<CreatedJob>>` matches its use in `advance` (`Ok(Some(created))`). `JobRepo::settle -> Result<bool>` matches both users. `CreatedJob::new` is `pub(crate)` and used by `job_creator.rs` and `settlement/hooks.rs`, both in-crate. `terminal::plan(&JobRow)` one argument, matching spec revision 3.
