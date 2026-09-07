# Creation-Time Settlement Unification — Implementation Plan (Plan A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a job that reaches a terminal state *at creation time* behave exactly like one that reaches it through the orchestrator: same status rules, aggregated output, hooks, log archive, metrics, and parent propagation — and stop the two ways such jobs currently get stuck.

**Architecture:** Extract the orchestrator's terminal-settlement block into one shared function that both the orchestrator and the job creator call. The creator returns a `terminal_at_creation` flag; a single `finalize_created_job` helper (which has `AppState`) runs terminal handling once and reconciles child jobs that settled synchronously. Hooks get one shared top-level classifier that includes `rerun` (and, for Plan B, `restart`).

**Tech Stack:** Rust (axum, sqlx runtime queries, tokio), Postgres via testcontainers in tests.

**Spec:** `docs/superpowers/specs/2026-09-07-restart-from-step-design.md` §4.3, §6.2, §6.3 and §13 items P3, P4, P5, P6, P8. This plan is the prerequisite half; Plan B (`2026-09-07-restart-from-step.md`) builds on it.

## Global Constraints

- `anyhow::Result` + `.context()` everywhere; `tracing` for logs; sqlx **runtime** queries (`sqlx::query`, never macros).
- Every task: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, and the named tests must pass before commit.
- Integration tests need Docker: `export DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true CARGO_INCREMENTAL=0` (see memory note on disk pressure; run `orb start` if the socket is missing).
- Commit messages: conventional prefix, no AI co-author line, end with `Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5`.
- Do not change the worker ↔ server wire format.

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `crates/stroem-db/src/repos/job.rs` | + `mark_cancelled` (sets `completed_at`, unlike `update_status`) |
| `crates/stroem-db/src/repos/job_step.rs` | + `fail_non_terminal_steps` (used when post-commit initialisation fails) |
| `crates/stroem-server/src/orchestrator.rs` | + `pub async fn settle_if_all_terminal(...)`; `on_step_completed` delegates to it |
| `crates/stroem-server/src/job_creator.rs` | `CreatedJob`, `create_job_for_task_detailed`, unified settle, post-commit error wrap |
| `crates/stroem-server/src/job_recovery.rs` | + `finalize_created_job`, `reconcile_settled_children`; callers wired |
| `crates/stroem-server/src/hooks.rs` | + `is_top_level_source`; both matchers use it; `rerun`/`restart` included |
| `crates/stroem-server/src/web/api/tasks.rs`, `mcp/tools.rs`, `scheduler.rs`, `web/hooks.rs`, `event_source.rs`, `hooks.rs` | call `finalize_created_job` after creating a job |
| `crates/stroem-server/tests/orchestrator_test.rs`, `tests/integration_test.rs`, `crates/stroem-db/tests/integration_test.rs` | tests |

---

### Task 1: DB helpers — `JobRepo::mark_cancelled`, `JobStepRepo::fail_non_terminal_steps`

**Files:**
- Modify: `crates/stroem-db/src/repos/job.rs` (after `mark_failed`, ~line 480)
- Modify: `crates/stroem-db/src/repos/job_step.rs` (after `cancel_pending_steps`, ~line 876)
- Test: `crates/stroem-db/tests/integration_test.rs` (append)

**Interfaces:**
- Produces: `JobRepo::mark_cancelled(pool: &PgPool, job_id: Uuid) -> Result<()>` — `status='cancelled', completed_at=NOW()`.
- Produces: `JobStepRepo::fail_non_terminal_steps(pool: &PgPool, job_id: Uuid, error: &str) -> Result<u64>` — every `pending|ready|claimed|running|suspended` step → `failed` with `error_message`, returns rows affected.

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-db/tests/integration_test.rs` (reuse `setup_db`, `JobRepo::create`, `NewJobStep` pattern already in the file; `agent_step` helper exists from the 2026-09-07 hotfix — copy its shape for a `script` step):

```rust
// ─── Plan A / Task 1: settlement helpers ─────────────────────────────────────

fn plain_step(job_id: Uuid, name: &str, status: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "noop".to_string(),
        action_type: "script".to_string(),
        action_image: None,
        action_spec: None,
        input: None,
        status: status.to_string(),
        required_ability: "script".to_string(),
        required_tags: vec![],
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

#[tokio::test]
async fn test_job_mark_cancelled_sets_completed_at() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = JobRepo::create(&pool, "default", "t", "distributed", None, "api", None, None, None).await?;
    JobRepo::mark_cancelled(&pool, job_id).await?;
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "cancelled");
    assert!(job.completed_at.is_some(), "mark_cancelled must stamp completed_at");
    Ok(())
}

#[tokio::test]
async fn test_fail_non_terminal_steps_only_touches_live_rows() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = JobRepo::create(&pool, "default", "t", "distributed", None, "api", None, None, None).await?;
    JobStepRepo::create_steps(
        &pool,
        &[
            plain_step(job_id, "done", "completed"),
            plain_step(job_id, "waiting", "pending"),
            plain_step(job_id, "queued", "ready"),
        ],
    )
    .await?;
    let n = JobStepRepo::fail_non_terminal_steps(&pool, job_id, "init exploded").await?;
    assert_eq!(n, 2);
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let by: std::collections::HashMap<_, _> = steps.iter().map(|s| (s.step_name.as_str(), s)).collect();
    assert_eq!(by["done"].status, "completed", "terminal rows untouched");
    assert_eq!(by["waiting"].status, "failed");
    assert_eq!(by["waiting"].error_message.as_deref(), Some("init exploded"));
    assert_eq!(by["queued"].status, "failed");
    assert!(by["queued"].completed_at.is_some());
    Ok(())
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p stroem-db --test integration_test mark_cancelled_sets fail_non_terminal`
Expected: compile error — `no function or associated item named mark_cancelled` / `fail_non_terminal_steps`.

- [ ] **Step 3: Implement**

`crates/stroem-db/src/repos/job.rs`, after `mark_failed`:

```rust
    /// Mark job as cancelled (stamps `completed_at`, unlike `update_status`).
    pub async fn mark_cancelled(pool: &PgPool, job_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET status = 'cancelled', completed_at = NOW()
            WHERE job_id = $1
            "#,
        )
        .bind(job_id)
        .execute(pool)
        .await
        .context("Failed to mark job as cancelled")?;
        Ok(())
    }
```

`crates/stroem-db/src/repos/job_step.rs`, after `cancel_pending_steps`:

```rust
    /// Fail every non-terminal step of a job with one error message. Used when
    /// post-commit job initialisation (promotion / expansion / dispatch) fails:
    /// the job row already exists, so the failure must be made visible on its
    /// steps instead of vanishing into a 500. Returns rows affected.
    pub async fn fail_non_terminal_steps(pool: &PgPool, job_id: Uuid, error: &str) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE job_step
            SET status = 'failed', error_message = $2, completed_at = NOW()
            WHERE job_id = $1
              AND status IN ('pending', 'ready', 'claimed', 'running', 'suspended')
            "#,
        )
        .bind(job_id)
        .bind(error)
        .execute(pool)
        .await
        .context("Failed to fail non-terminal steps")?;
        Ok(result.rows_affected())
    }
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p stroem-db --test integration_test mark_cancelled_sets fail_non_terminal`
Expected: `2 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-db/src/repos/job.rs crates/stroem-db/src/repos/job_step.rs crates/stroem-db/tests/integration_test.rs
git commit -m "feat(db): JobRepo::mark_cancelled and JobStepRepo::fail_non_terminal_steps

Settlement helpers for Plan A (creation-time settlement unification).

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 2: Extract `orchestrator::settle_if_all_terminal` (adds `cancelled` handling)

**Files:**
- Modify: `crates/stroem-server/src/orchestrator.rs:115-214` (the `all_terminal` block)
- Test: `crates/stroem-server/tests/orchestrator_test.rs` (append)

**Interfaces:**
- Produces: `pub async fn settle_if_all_terminal(pool: &PgPool, job_id: Uuid, task: &TaskDef) -> Result<Option<JobStatus>>`. Returns `None` when a non-terminal step remains; otherwise the status it set (or found, for an already-cancelled job): `Failed` (an untolerated failure), `Cancelled` (no untolerated failure, ≥1 cancelled step, or job already cancelled), `Completed` (with aggregated output of terminal steps).
- `on_step_completed` keeps its signature; its body ends with `settle_if_all_terminal(...).await?; Ok(())`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-server/tests/orchestrator_test.rs` (helpers `setup_db`, `create_job`, `step`, `flow_step`, `flow_step_cof`, `make_task`, `step_statuses` exist):

```rust
// ─── Plan A / Task 2: shared settlement ──────────────────────────────────────

/// A job whose only non-completed step was cancelled must settle as
/// `cancelled`, not `completed` (the old creation-time settle did that).
#[tokio::test]
async fn test_settle_cancelled_step_without_failure_marks_job_cancelled() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert("b".to_string(), flow_step(vec![]));
    let task = make_task(flow);
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "a", "completed"), step(job_id, "b", "cancelled")]).await?;
    JobStepRepo::mark_completed(&pool, job_id, "a", Some(json!({"x": 1}))).await?;

    let settled = stroem_server::orchestrator::settle_if_all_terminal(&pool, job_id, &task).await?;
    assert_eq!(settled, Some(stroem_common::models::job::JobStatus::Cancelled));
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "cancelled");
    assert!(job.completed_at.is_some());
    Ok(())
}

/// Tolerated failure (continue_on_failure) completes the job and aggregates
/// output from terminal steps — identical to the orchestrator path.
#[tokio::test]
async fn test_settle_tolerated_failure_completes_with_aggregated_output() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step_cof(vec![]));
    flow.insert("b".to_string(), flow_step(vec![]));
    let task = make_task(flow);
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "a", "ready"), step(job_id, "b", "ready")]).await?;
    JobStepRepo::mark_failed(&pool, job_id, "a", "boom").await?;
    JobStepRepo::mark_completed(&pool, job_id, "b", Some(json!({"out": "b"}))).await?;

    let settled = stroem_server::orchestrator::settle_if_all_terminal(&pool, job_id, &task).await?;
    assert_eq!(settled, Some(stroem_common::models::job::JobStatus::Completed));
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    assert_eq!(job.output.unwrap()["b"]["out"], "b");
    Ok(())
}

#[tokio::test]
async fn test_settle_returns_none_while_a_step_is_live() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    let task = make_task(flow);
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(&pool, &[step(job_id, "a", "ready")]).await?;
    let settled = stroem_server::orchestrator::settle_if_all_terminal(&pool, job_id, &task).await?;
    assert_eq!(settled, None);
    assert_eq!(JobRepo::get(&pool, job_id).await?.unwrap().status, "pending");
    Ok(())
}
```

`JobStatus` needs `PartialEq` + `Debug` for `assert_eq!` — check `crates/stroem-common/src/models/job.rs:9`; if the derive list lacks `PartialEq`, add `PartialEq, Eq` to it.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p stroem-server --test orchestrator_test test_settle_`
Expected: compile error `cannot find function settle_if_all_terminal`.

- [ ] **Step 3: Implement**

In `crates/stroem-server/src/orchestrator.rs`, replace everything from `// 2. Check if all steps are terminal` down to the closing `Ok(())` of `on_step_completed` with:

```rust
    // 2. Settle the job if every step is terminal.
    settle_if_all_terminal(pool, job_id, task).await?;
    Ok(())
}

/// If every step of the job is terminal, decide and persist the job's final
/// status and return it; otherwise return `None` and touch nothing.
///
/// Single source of truth for terminal settlement — called from
/// `on_step_completed` AND from job creation (`create_job_for_task_inner`), so
/// a job that is already terminal at creation (all steps skipped by `when`, or
/// a server-dispatched root step that failed) gets exactly the same rules:
/// `continue_on_failure` tolerance, `cancelled` propagation, and aggregated
/// output of the flow's terminal steps.
#[tracing::instrument(skip(pool, task))]
pub async fn settle_if_all_terminal(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
) -> Result<Option<JobStatus>> {
    let all_terminal = JobStepRepo::all_steps_terminal(pool, job_id)
        .await
        .context("Failed to check if all steps are terminal")?;
    if !all_terminal {
        return Ok(None);
    }

    // Never overwrite an explicit cancellation.
    if let Some(j) = JobRepo::get(pool, job_id).await? {
        if j.status == JobStatus::Cancelled.as_ref() {
            tracing::info!("Job {} is already cancelled, skipping status update", job_id);
            return Ok(Some(JobStatus::Cancelled));
        }
    }

    let steps = JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .context("Failed to get steps for settlement")?;

    // Loop instance steps ("process[0]") are not in task.flow — look up by
    // their placeholder name. Instance failures are already folded into the
    // placeholder by `check_loop_completion`.
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
        tracing::info!("Job {} failed (one or more steps failed)", job_id);
        JobRepo::mark_failed(pool, job_id)
            .await
            .context("Failed to mark job as failed")?;
        return Ok(Some(JobStatus::Failed));
    }

    let any_cancelled = steps
        .iter()
        .any(|s| s.status == StepStatus::Cancelled.as_ref());
    if any_cancelled {
        tracing::info!("Job {} cancelled (a step was cancelled, no untolerated failure)", job_id);
        JobRepo::mark_cancelled(pool, job_id)
            .await
            .context("Failed to mark job as cancelled")?;
        return Ok(Some(JobStatus::Cancelled));
    }

    let failed_count = steps.iter().filter(|s| s.status == StepStatus::Failed.as_ref()).count();
    if failed_count > 0 {
        tracing::info!("Job {} completed with {} tolerable failure(s)", job_id, failed_count);
    } else {
        tracing::info!("Job {} completed successfully", job_id);
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
    for s in &steps {
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
    JobRepo::mark_completed(pool, job_id, output)
        .await
        .context("Failed to mark job as completed")?;
    Ok(Some(JobStatus::Completed))
}
```

Add `use stroem_common::models::job::StepStatus;` to the imports (the file already imports `JobStatus`). Remove the now-unused `get_failed_step_names` call; keep the repo function (other callers may use it).

- [ ] **Step 4: Run to verify**

Run: `cargo test -p stroem-server --test orchestrator_test`
Expected: all existing tests still pass (31 + 3 new). Pay attention to `test_all_tolerable_failures_job_completes` and `test_mix_completed_and_failed_job_fails` — they exercise the moved logic.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/orchestrator.rs crates/stroem-server/tests/orchestrator_test.rs crates/stroem-common/src/models/job.rs
git commit -m "refactor(orchestrator): extract settle_if_all_terminal; settle cancelled steps as cancelled

Single terminal-settlement routine shared by the orchestrator and (next task)
job creation. Adds the missing 'cancelled' outcome: a job whose only
non-completed step was cancelled no longer settles as completed.

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 3: Creator uses shared settlement, returns `CreatedJob`, survives post-commit failures

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs:28-98` (wrappers), `:106-121` (inner signature), `:345-431` (post-commit phase)
- Test: `crates/stroem-server/tests/integration_test.rs` (append)

**Interfaces:**
- Produces: `pub struct CreatedJob { pub job_id: Uuid, pub terminal_at_creation: bool }`.
- Produces: `pub async fn create_job_for_task_detailed(<same params as create_job_for_task>) -> Result<CreatedJob>`.
- `create_job_for_task` and `create_child_job_for_task` keep returning `Result<Uuid>` (they call `_detailed` / inner and drop the flag) — no caller churn.
- Inner returns `Result<CreatedJob>`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/stroem-server/tests/integration_test.rs`. It has `task_action_test_workspace()`, `setup_with_workspace`, `api_request`, `body_json`, and the `ActionDef`/`FlowStep`/`TaskDef` struct-update pattern used by `test_task_step_dispatch_failure_cascades_and_fails_job`.

```rust
// ─── Plan A / Task 3: creation-time settlement uses the shared routine ───────

/// A root `type: task` step that fails at dispatch but is `continue_on_failure`
/// must leave the job `completed` (the orchestrator rule), not `failed` (the
/// old creation-time rule).
#[tokio::test]
async fn test_creation_settle_honours_continue_on_failure() -> Result<()> {
    let mut workspace = task_action_test_workspace();
    let greet_action = workspace.actions["greet"].clone();
    workspace.actions.insert(
        "run-missing".to_string(),
        ActionDef { action_type: "task".to_string(), task: Some("nonexistent-task".to_string()), ..greet_action },
    );
    let base_task = workspace.tasks["cleanup"].clone();
    let base_step = base_task.flow.values().next().unwrap().clone();
    let mut flow = HashMap::new();
    flow.insert(
        "dispatch".to_string(),
        FlowStep { action: "run-missing".to_string(), depends_on: vec![], input: HashMap::new(), continue_on_failure: true, ..base_step },
    );
    workspace.tasks.insert("tolerant-root".to_string(), TaskDef { flow, ..base_task });

    let (router, pool, _tmp, _container) = setup_with_workspace(workspace).await?;
    let response = router
        .clone()
        .oneshot(api_request("POST", "/api/workspaces/default/tasks/tolerant-root/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let job_id: Uuid = body_json(response).await["job_id"].as_str().unwrap().parse()?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "failed", "{steps:?}");
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed", "tolerated failure must complete the job: {job:?}");
    assert!(job.completed_at.is_some());
    Ok(())
}

/// `create_job_for_task_detailed` reports whether the job settled at creation.
#[tokio::test]
async fn test_create_job_detailed_reports_terminal_at_creation() -> Result<()> {
    let mut workspace = task_action_test_workspace();
    let base_task = workspace.tasks["cleanup"].clone();
    let base_step = base_task.flow.values().next().unwrap().clone();
    // Root step with `when: "false"` → skipped at creation → job terminal at creation.
    let mut flow = HashMap::new();
    flow.insert(
        "never".to_string(),
        FlowStep { action: "greet".to_string(), depends_on: vec![], input: HashMap::new(), when: Some("false".to_string()), ..base_step.clone() },
    );
    workspace.tasks.insert("all-skipped".to_string(), TaskDef { flow, ..base_task.clone() });
    // Plain root step → not terminal at creation.
    let mut flow2 = HashMap::new();
    flow2.insert("run".to_string(), FlowStep { action: "greet".to_string(), depends_on: vec![], input: HashMap::new(), ..base_step });
    workspace.tasks.insert("live".to_string(), TaskDef { flow: flow2, ..base_task });

    let (pool, _container) = {
        let container = Postgres::default().start().await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let pool = create_pool(&format!("postgres://postgres:postgres@localhost:{}/postgres", port)).await?;
        run_migrations(&pool).await?;
        (pool, container)
    };
    let mgr = WorkspaceManager::from_config("default", workspace.clone());

    let created = stroem_server::job_creator::create_job_for_task_detailed(
        &mgr, &pool, &workspace, "default", "all-skipped", json!({}), "api", None, None, None, None, JobDefaults::default(),
    )
    .await?;
    assert!(created.terminal_at_creation);
    assert_eq!(JobRepo::get(&pool, created.job_id).await?.unwrap().status, "completed");

    let created = stroem_server::job_creator::create_job_for_task_detailed(
        &mgr, &pool, &workspace, "default", "live", json!({}), "api", None, None, None, None, JobDefaults::default(),
    )
    .await?;
    assert!(!created.terminal_at_creation);
    assert_eq!(JobRepo::get(&pool, created.job_id).await?.unwrap().status, "pending");
    Ok(())
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p stroem-server --test integration_test test_creation_settle_honours test_create_job_detailed_reports`
Expected: first test FAILS with `job.status == "failed"`; second fails to compile (`create_job_for_task_detailed` missing).

- [ ] **Step 3: Implement**

In `crates/stroem-server/src/job_creator.rs`:

(a) Add near the top (after imports):

```rust
/// Result of job creation. `terminal_at_creation` is true when every step was
/// already terminal once creation-time promotion/expansion/dispatch finished
/// (e.g. all root steps skipped by `when`, or a server-dispatched root step
/// failed) — the caller must then run `job_recovery::finalize_created_job`
/// so hooks/metrics/log-archive fire exactly as for an orchestrator-settled job.
#[derive(Debug, Clone, Copy)]
pub struct CreatedJob {
    pub job_id: Uuid,
    pub terminal_at_creation: bool,
}
```

(b) Change `create_job_for_task` to delegate and drop the flag; add the detailed variant:

```rust
pub async fn create_job_for_task(/* unchanged params */) -> Result<Uuid> {
    create_job_for_task_detailed(
        workspaces, pool, workspace_config, workspace_name, task_name, input,
        source_type, source_id, revision, source_job_id, agents_config, defaults,
    )
    .await
    .map(|c| c.job_id)
}

/// Like [`create_job_for_task`] but also reports `terminal_at_creation`.
/// HTTP/MCP/scheduler entry points use this and call
/// `job_recovery::finalize_created_job` afterwards.
#[allow(clippy::too_many_arguments)]
pub async fn create_job_for_task_detailed(
    workspaces: &WorkspaceManager,
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    task_name: &str,
    input: serde_json::Value,
    source_type: &str,
    source_id: Option<&str>,
    revision: Option<&str>,
    source_job_id: Option<Uuid>,
    agents_config: Option<&AgentsConfig>,
    defaults: JobDefaults,
) -> Result<CreatedJob> {
    create_job_for_task_inner(
        workspaces, pool, workspace_config, workspace_name, task_name, input,
        source_type, source_id, None, None, revision, source_job_id, agents_config, defaults,
    )
    .await
}
```

`create_child_job_for_task`: append `.map(|c| c.job_id)` to its inner call.

(c) Inner: change the return type to `Pin<Box<dyn Future<Output = Result<CreatedJob>> + Send + 'a>>`. Replace everything after `tx.commit()...;` (the metrics counter stays) i.e. from `// Evaluate root steps with ...` to the final `Ok(job_id)` with:

```rust
        // ── Post-commit initialisation ────────────────────────────────────
        // The job row is committed; anything that fails from here on must be
        // made visible on the job instead of surfacing as a 500 with a
        // committed `pending` job left behind (spec §6.2 / P8).
        let init: Result<()> = async {
            // Promote/skip/expand root steps. Runs unconditionally: cheap when
            // nothing is promotable, and required for Plan B's seeded jobs.
            let job_row = JobRepo::get(pool, job_id).await?.context("Job not found")?;
            let max_iterations = task.flow.len() * 2 + 10;
            for _iteration in 0..max_iterations {
                let steps_snapshot = JobStepRepo::get_steps_for_job(pool, job_id).await?;
                let render_ctx =
                    build_step_render_context(&job_row, &steps_snapshot, workspace_config);
                let changed =
                    JobStepRepo::promote_ready_steps(pool, job_id, &task.flow, Some(&render_ctx))
                        .await?;
                let skipped = JobStepRepo::skip_unreachable_steps(pool, job_id, &task.flow).await?;
                let expanded =
                    expand_for_each_steps(pool, workspace_config, workspace_name, job_id, task)
                        .await?;
                if changed.is_empty() && skipped.is_empty() && expanded.is_empty() {
                    break;
                }
                if _iteration + 1 == max_iterations {
                    tracing::warn!(
                        job_id = %job_id,
                        "Root-condition cascade loop reached iteration limit ({}) — breaking to avoid infinite loop",
                        max_iterations
                    );
                }
            }

            handle_task_steps(workspaces, pool, workspace_config, workspace_name, job_id, task, defaults)
                .await?;

            if let Err(e) =
                handle_approval_steps(pool, workspace_config, workspace_name, job_id, task).await
            {
                tracing::error!(job_id = %job_id, "Failed to handle initial approval steps: {:#}", e);
            }
            Ok(())
        }
        .await;

        if let Err(e) = init {
            let msg = format!("[creation] initialisation failed: {:#}", e);
            tracing::error!(job_id = %job_id, "{}", msg);
            JobStepRepo::fail_non_terminal_steps(pool, job_id, &msg)
                .await
                .context("fail steps after initialisation error")?;
            JobRepo::mark_failed(pool, job_id)
                .await
                .context("mark job failed after initialisation error")?;
            return Ok(CreatedJob { job_id, terminal_at_creation: true });
        }

        // Shared settlement — identical rules to the orchestrator path.
        let settled = crate::orchestrator::settle_if_all_terminal(pool, job_id, task)
            .await
            .context("settle job at creation")?;
        if let Some(status) = settled {
            tracing::info!(job_id = %job_id, ?status, "All steps terminal at creation — job settled");
        }

        Ok(CreatedJob { job_id, terminal_at_creation: settled.is_some() })
    })
}
```

Delete the old `needs_post_creation_loop` variable and the old settle block entirely. `handle_task_steps`'s own internal `?` on DB errors now lands in `init`, which is the intended behaviour.

- [ ] **Step 4: Run to verify**

Run: `cargo test -p stroem-server --test integration_test test_creation_settle_honours test_create_job_detailed_reports test_task_step_dispatch_failure test_task_step_bad_connection test_create_job_for_task_step_type_task_with_when_false`
Expected: all pass. Then `cargo test -p stroem-server` (full crate) — expected all green; `test_task_step_bad_connection_fails_step_not_swallowed` asserts `job.status == "failed"` for an *untolerated* dispatch failure and must still pass.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/job_creator.rs crates/stroem-server/tests/integration_test.rs
git commit -m "fix(job-creator): settle via the shared routine, report terminal_at_creation, survive init errors

Creation-time settlement now honours continue_on_failure and cancelled steps and
aggregates output (same rules as the orchestrator). A post-commit
initialisation error fails the live steps with the error and marks the job
failed instead of returning 500 over a committed pending job.

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 4: `finalize_created_job` + `reconcile_settled_children`; wire all creation entry points

**Files:**
- Modify: `crates/stroem-server/src/job_recovery.rs` (add two fns near `handle_job_terminal`; call `reconcile_settled_children` after the two `handle_task_steps` call sites at ~`:175` and ~`:503`)
- Modify: `crates/stroem-db/src/repos/job.rs` (add `get_settled_children_with_running_parent_step`)
- Modify callers: `crates/stroem-server/src/web/api/tasks.rs:517-535`, `crates/stroem-server/src/mcp/tools.rs:510`, `crates/stroem-server/src/scheduler.rs:413`, `crates/stroem-server/src/web/hooks.rs:101`, `crates/stroem-server/src/event_source.rs:592`, `crates/stroem-server/src/hooks.rs:436`, `crates/stroem-server/src/job_recovery.rs:913` (`try_retry_job`)
- Test: `crates/stroem-server/tests/integration_test.rs` (append)

**Interfaces:**
- Produces: `pub async fn finalize_created_job(state: &AppState, created: CreatedJob)` — if `terminal_at_creation` → `handle_job_terminal(state, job_id)`; always → `reconcile_settled_children(state, job_id)`. Errors are logged, never returned (creation already succeeded).
- Produces: `pub async fn reconcile_settled_children(state: &AppState, parent_job_id: Uuid)` — for every child job of `parent_job_id` that is terminal while its parent step is still `running`, call `handle_job_terminal(state, child.job_id)` (which propagates to the parent step and runs the child's hooks).
- Produces: `JobRepo::get_settled_children_with_running_parent_step(pool, parent_job_id) -> Result<Vec<JobRow>>`.

- [ ] **Step 1: Write the failing tests**

```rust
// ─── Plan A / Task 4: terminal-at-creation side effects ──────────────────────

/// Root job that settles at creation (all steps skipped) must run terminal
/// handling: the workspace `on_success` hook job is created.
#[tokio::test]
async fn test_all_skipped_job_at_creation_fires_workspace_hook() -> Result<()> {
    let mut workspace = task_action_test_workspace();
    let base_task = workspace.tasks["cleanup"].clone();
    let base_step = base_task.flow.values().next().unwrap().clone();
    let mut flow = HashMap::new();
    flow.insert(
        "never".to_string(),
        FlowStep { action: "greet".to_string(), depends_on: vec![], input: HashMap::new(), when: Some("false".to_string()), ..base_step },
    );
    workspace.tasks.insert("all-skipped".to_string(), TaskDef { flow, ..base_task });
    workspace.on_success.push(HookDef { action: "greet".to_string(), input: HashMap::new() });

    let (router, pool, _tmp, _container) = setup_with_workspace(workspace).await?;
    let response = router
        .oneshot(api_request("POST", "/api/workspaces/default/tasks/all-skipped/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let job_id: Uuid = body_json(response).await["job_id"].as_str().unwrap().parse()?;
    assert_eq!(JobRepo::get(&pool, job_id).await?.unwrap().status, "completed");

    let jobs = JobRepo::list(&pool, Some("default"), None, None, None, 100, 0).await?;
    let hook_jobs: Vec<_> = jobs.iter().filter(|j| j.source_type == "hook").collect();
    assert_eq!(hook_jobs.len(), 1, "exactly one on_success hook job: {jobs:?}");
    assert!(hook_jobs[0].source_id.as_deref().unwrap_or("").starts_with(&job_id.to_string()));
    Ok(())
}

/// A `type: task` child that settles synchronously at creation (its only step
/// is skipped by `when`) must propagate to the parent step; the parent job
/// must complete instead of staying `running` (P5).
#[tokio::test]
async fn test_child_settled_at_creation_propagates_to_parent() -> Result<()> {
    let mut workspace = task_action_test_workspace();
    let base_task = workspace.tasks["cleanup"].clone();
    let base_step = base_task.flow.values().next().unwrap().clone();
    let mut child_flow = HashMap::new();
    child_flow.insert(
        "never".to_string(),
        FlowStep { action: "greet".to_string(), depends_on: vec![], input: HashMap::new(), when: Some("false".to_string()), ..base_step.clone() },
    );
    workspace.tasks.insert("instant-child".to_string(), TaskDef { flow: child_flow, ..base_task.clone() });
    let greet_action = workspace.actions["greet"].clone();
    workspace.actions.insert(
        "run-instant".to_string(),
        ActionDef { action_type: "task".to_string(), task: Some("instant-child".to_string()), ..greet_action },
    );
    let mut flow = HashMap::new();
    flow.insert("child".to_string(), FlowStep { action: "run-instant".to_string(), depends_on: vec![], input: HashMap::new(), ..base_step });
    workspace.tasks.insert("parent".to_string(), TaskDef { flow, ..base_task });

    let (router, pool, _tmp, _container) = setup_with_workspace(workspace).await?;
    let response = router
        .oneshot(api_request("POST", "/api/workspaces/default/tasks/parent/execute", json!({"input": {}})))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let job_id: Uuid = body_json(response).await["job_id"].as_str().unwrap().parse()?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "completed", "parent step must reflect the settled child: {steps:?}");
    assert_eq!(JobRepo::get(&pool, job_id).await?.unwrap().status, "completed");
    Ok(())
}
```

`HookDef` import: `use stroem_common::models::workflow::HookDef;` (check the file's existing imports; `test_task_action_in_hook` at ~`:13120` shows the hook fixture shape).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p stroem-server --test integration_test test_all_skipped_job_at_creation_fires test_child_settled_at_creation`
Expected: first FAILS with `hook_jobs.len() == 0`; second FAILS with parent step `running` / job `running`.

- [ ] **Step 3: Implement**

`crates/stroem-db/src/repos/job.rs` (after `get_child_jobs`):

```rust
    /// Child jobs that are terminal while the parent step that spawned them is
    /// still `running` — i.e. children that settled synchronously at creation
    /// and never went through terminal handling / parent propagation.
    pub async fn get_settled_children_with_running_parent_step(
        pool: &PgPool,
        parent_job_id: Uuid,
    ) -> Result<Vec<JobRow>> {
        let rows = sqlx::query_as::<_, JobRow>(&format!(
            "SELECT {} FROM job j \
             WHERE j.parent_job_id = $1 \
               AND j.status IN ('completed', 'failed', 'cancelled', 'skipped') \
               AND EXISTS ( \
                   SELECT 1 FROM job_step s \
                   WHERE s.job_id = $1 AND s.step_name = j.parent_step_name AND s.status = 'running' \
               )",
            JOB_COLUMNS
        ))
        .bind(parent_job_id)
        .fetch_all(pool)
        .await
        .context("get_settled_children_with_running_parent_step")?;
        Ok(rows)
    }
```

`crates/stroem-server/src/job_recovery.rs` (before `handle_job_terminal`):

```rust
/// Run the side effects a freshly created job may already owe.
///
/// - `terminal_at_creation` → `handle_job_terminal` (hooks, metrics, archive,
///   parent propagation) — the creator itself has no `AppState`.
/// - Always → `reconcile_settled_children`: `type: task` root steps dispatched
///   at creation may have produced a child that settled synchronously.
///
/// Best-effort: creation already succeeded, so problems are logged, not returned.
pub async fn finalize_created_job(state: &AppState, created: crate::job_creator::CreatedJob) {
    if created.terminal_at_creation {
        if let Err(e) = handle_job_terminal(state, created.job_id).await {
            tracing::error!(job_id = %created.job_id, "terminal handling after creation failed: {:#}", e);
        }
    }
    reconcile_settled_children(state, created.job_id).await;
}

/// Children of `parent_job_id` that are terminal while their parent step is
/// still `running` never reached `propagate_to_parent` (they settled inside
/// `create_job_for_task_inner`, which has no `AppState`). Run terminal handling
/// for each; it propagates to the parent step and fires the child's hooks. The
/// "parent step still running" predicate makes this idempotent.
pub async fn reconcile_settled_children(state: &AppState, parent_job_id: Uuid) {
    let children = match JobRepo::get_settled_children_with_running_parent_step(&state.pool, parent_job_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(job_id = %parent_job_id, "reconcile_settled_children: {:#}", e);
            return;
        }
    };
    for child in children {
        tracing::info!(
            child = %child.job_id, parent = %parent_job_id,
            "child job settled at creation — running terminal handling"
        );
        if let Err(e) = handle_job_terminal(state, child.job_id).await {
            tracing::error!(child = %child.job_id, "terminal handling for settled child failed: {:#}", e);
        }
    }
}
```

In `orchestrate_after_step`, immediately after the `handle_task_steps(...)` `if let Err` block (~`:175-192`), add `reconcile_settled_children(state, job_id).await;`. In `propagate_to_parent`, after its `handle_task_steps(...)` call (~`:503-512`), add `reconcile_settled_children(state, parent_job_id).await;`.

Callers — replace `create_job_for_task(...)` with `create_job_for_task_detailed(...)` and follow with `finalize_created_job`:

`web/api/tasks.rs:517-535`:
```rust
    let created = create_job_for_task_detailed(
        &state.workspaces, &state.pool, &workspace, &ws, &name, input_value,
        effective_source_type, source_id.as_deref(), revision.as_deref(),
        req.source_job_id, state.config.agents.as_ref(), JobDefaults::from(state.config.as_ref()),
    )
    .await
    .map_err(classify_execute_error)?;
    let job_id = created.job_id;

    crate::job_creator::fire_initial_suspended_hooks(&state, &workspace, &ws, &name, job_id).await;
    crate::job_recovery::finalize_created_job(&state, created).await;
```
(update the `use crate::job_creator::{...}` import to include `create_job_for_task_detailed`.)

`mcp/tools.rs:510`, `scheduler.rs:413`, `web/hooks.rs:101`, `event_source.rs:592`, `hooks.rs:436`, `job_recovery.rs:913`: same pattern — each already has `state`/`&self.state` in scope (check each site; `scheduler.rs` uses `match create_job_for_task(...)` — bind `Ok(created)` and call `finalize_created_job(&state, created).await` inside the arm, keep `created.job_id` where `job_id` was used). For `hooks.rs:436` (hook jobs) the recursion guard in `fire_hooks` prevents hook-of-hook; still call `finalize_created_job` so a hook job that settles at creation gets its archive/metrics.

- [ ] **Step 4: Run to verify**

Run: `cargo test -p stroem-server` (the two new tests plus everything else; scheduler, webhook, event-source and hook tests exercise the rewired call sites).
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-db/src/repos/job.rs crates/stroem-server/src
git add crates/stroem-server/tests/integration_test.rs
git commit -m "fix(orchestration): run terminal handling for jobs settled at creation; reconcile children settled synchronously

finalize_created_job runs handle_job_terminal once when a job is terminal at
creation (hooks, metrics, log archive were skipped before) and
reconcile_settled_children propagates type:task children that settled inside
creation to their parent step (parent no longer stuck running).

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 5: Hooks — shared top-level classifier including `rerun` and `restart`

**Files:**
- Modify: `crates/stroem-server/src/hooks.rs:95-98`, `:210-213` (and the test helper at `:552` if it duplicates the list)
- Test: `crates/stroem-server/tests/rerun_integration_test.rs` (append)

**Interfaces:**
- Produces: `pub(crate) fn is_top_level_source(source_type: &str) -> bool` matching `api | user | trigger | webhook | mcp | retry | rerun | restart`.

- [ ] **Step 1: Write the failing test**

Append to `crates/stroem-server/tests/rerun_integration_test.rs` (helpers `build_test_app`, `build_rerun_workspace`, `execute_task`, `get_job` exist; the router needs a worker request helper — add one):

```rust
fn worker_req(method: &str, uri: &str, body: JsonValue) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-token")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A failed Re-run must fire the WORKSPACE `on_error` hook (rerun was missing
/// from the top-level source-type list).
#[tokio::test(flavor = "multi_thread")]
async fn rerun_failure_fires_workspace_on_error_hook() -> Result<()> {
    let mut workspace = build_rerun_workspace();
    // Any existing action works as the hook action — the hook job just needs to exist.
    let hook_action = workspace.actions.keys().next().cloned().expect("workspace has an action");
    workspace
        .on_error
        .push(stroem_common::models::workflow::HookDef { action: hook_action, input: HashMap::new() });
    let app = build_test_app("default", workspace).await?;
    let task = app_task_name(); // see note below

    // Source job.
    let (st, body) = execute_task(&app, "default", &task, json!({"input": {"note": "first"}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id = body["job_id"].as_str().unwrap().to_string();

    // Re-run it.
    let (st, body) = execute_task(&app, "default", &task, json!({"input": {"note": "second"}, "source_job_id": source_id})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let rerun_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Worker claims and fails the rerun's step.
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&app.pool, worker_id, "w", &["script".to_string()], &[], false, None).await?;
    let resp = app.router.clone().oneshot(worker_req("POST", "/worker/jobs/claim",
        json!({"worker_id": worker_id.to_string(), "capabilities": ["script"]}))).await?;
    let claim: JsonValue = serde_json::from_slice(&resp.into_body().collect().await?.to_bytes())?;
    let step = claim["step_name"].as_str().expect("claimed the rerun step").to_string();
    assert_eq!(claim["job_id"].as_str().unwrap(), rerun_id.to_string());
    let resp = app.router.clone().oneshot(worker_req("POST",
        &format!("/worker/jobs/{rerun_id}/steps/{step}/complete"),
        json!({"exit_code": 1, "error": "boom"}))).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let rerun = get_job(&app, &rerun_id.to_string()).await?;
    assert_eq!(rerun["status"], "failed");
    let jobs = JobRepo::list(&app.pool, Some("default"), None, None, None, 100, 0).await?;
    let hook_jobs: Vec<_> = jobs.iter().filter(|j| j.source_type == "hook").collect();
    assert_eq!(hook_jobs.len(), 1, "workspace on_error must fire for a rerun: {jobs:?}");
    Ok(())
}
```

Note: `build_rerun_workspace()` defines exactly one task — read its name at `rerun_integration_test.rs:47-200` and either hard-code it in place of `app_task_name()` or add `fn app_task_name() -> String` returning that literal. The source job in this test never runs (no worker claimed it) — that is fine, the rerun only needs the source's `raw_input`. Imports needed: `use stroem_db::{JobRepo, WorkerRepo};`, `use http_body_util::BodyExt;` (already used by `execute_task`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-server --test rerun_integration_test rerun_failure_fires_workspace_on_error_hook`
Expected: FAIL `hook_jobs.len() == 0`.

- [ ] **Step 3: Implement**

In `crates/stroem-server/src/hooks.rs`, add near the top-level helpers:

```rust
/// Source types whose jobs are "top-level" runs: workspace-level hooks
/// (`on_success` / `on_error` / `on_cancel` / `on_suspended`) fall back to
/// them when the task defines none. Child (`task`), hook, agent-tool and
/// event-source consumer jobs are excluded. `rerun` and `restart` are
/// user-initiated top-level runs just like `user`/`api`.
pub(crate) fn is_top_level_source(source_type: &str) -> bool {
    matches!(
        source_type,
        "api" | "user" | "trigger" | "webhook" | "mcp" | "retry" | "rerun" | "restart"
    )
}
```

Replace both `let is_top_level = matches!(job.source_type.as_str(), "api" | ... | "retry");` blocks with `let is_top_level = is_top_level_source(&job.source_type);`. If the test helper at ~`:552` repeats the list, make it call `is_top_level_source` too.

- [ ] **Step 4: Run to verify**

Run: `cargo test -p stroem-server --test rerun_integration_test && cargo test -p stroem-server hooks`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/hooks.rs crates/stroem-server/tests/rerun_integration_test.rs
git commit -m "fix(hooks): rerun and restart jobs are top-level for workspace hooks; single classifier

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

---

### Task 6: Docs, TODO, CLAUDE.md, full verification

**Files:**
- Modify: `CLAUDE.md` (§ Task Actions, § Hooks, § Job Lineage), `docs/internal/TODO.md` (P3, P4, P5, P6, P8 → `[x]`), `docs/src/content/docs/guides/hooks.md` (or wherever workspace hooks are documented — `grep -rl "on_error" docs/src/content/docs/guides`)

- [ ] **Step 1: CLAUDE.md**

Under `### Task Actions (type: task)` add:
```
- **Settlement is shared**: `orchestrator::settle_if_all_terminal(pool, job_id, task)` is the ONLY place a job's terminal status is decided (orchestrator AND job creation). Rules: any untolerated `failed` → `failed`; else any `cancelled` → `cancelled`; else `completed` with aggregated terminal-step output. `create_job_for_task_detailed` returns `CreatedJob { job_id, terminal_at_creation }`; every HTTP/MCP/scheduler/webhook/hook/event-source/retry entry point calls `job_recovery::finalize_created_job` afterwards (runs `handle_job_terminal` once when terminal at creation + `reconcile_settled_children`). A child job that settles inside creation is picked up by `reconcile_settled_children` (also called after every `handle_task_steps`).
- Post-commit initialisation errors fail the live steps with `[creation] initialisation failed: …` and mark the job failed (never a 500 over a committed pending job).
```
Under `### Hooks`: `- Top-level source types (workspace-hook fallback): hooks::is_top_level_source — api, user, trigger, webhook, mcp, retry, rerun, restart.`

- [ ] **Step 2: TODO.md** — flip the five entries added on 2026-09-07 (settle/terminal handling, child settled at creation, rerun hooks, post-commit init errors) to `[x]` with a one-line "Fixed:" note referencing this plan. Leave P7 (retry input replay) open.

- [ ] **Step 3: User docs** — in the hooks guide, state that workspace-level hooks now also fire for Re-run jobs.

- [ ] **Step 4: Full verification**

```bash
cargo fmt --check --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
cd ui && bun run lint && bunx tsc --noEmit
```
Expected: all clean/green.

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md docs
git commit -m "docs: shared settlement, finalize_created_job, top-level hook sources

Claude-Session: https://claude.ai/code/session_018gEfSxowtJHuLkbbNkoVg5"
```

Plan A is complete and independently releasable (patch). Release notes must call out: workspace hooks now fire for Re-run jobs; a job whose only non-completed step was cancelled now settles `cancelled` (was `completed`); jobs terminal at creation now fire hooks and archive logs.
