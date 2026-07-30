# Cross-Workspace References Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a flow step reference another workspace's action as `owner_ws.action`, executing it with the owner workspace's tarball, secrets, and connections — no library registration required.

**Architecture:** A dotted action name `owner_ws.action` is resolved at job-creation time against the owner workspace's loaded config; the step records `action_workspace` + a pinned `action_revision`. At claim time the server renders the owner action against the owner workspace (secrets/connections/files) and tells the worker to fetch the owner tarball. The worker is unchanged — it already fetches per-step by `(workspace, revision)`.

**Tech Stack:** Rust, axum, sqlx (runtime queries), tokio, Tera templating, Postgres (testcontainers).

## Global Constraints

- **Backward compatibility:** unqualified action names resolve locally, exactly as today. New DB columns are nullable; NULL ⇒ "the job's own workspace" (no backfill).
- **Library precedence:** a dotted name resolves as a **library** item first (libraries are already flattened into the local config), then as a `workspace.item` cross-workspace reference. Never break existing library refs.
- **Access:** open — any workspace may reference any other (no ACL gate this iteration).
- **DB:** runtime `sqlx::query`/`query_as`, NOT compile-time macros. Migrations in `crates/stroem-db/migrations/`, next number is `043`.
- **Error handling:** `anyhow::Result` + `.context(...)`; user/config errors must surface as HTTP 400, not 500.
- **Scope (this plan):** cross-workspace **actions** + owner-context resolution of the connections/secrets/files those actions use. **Deferred to follow-ups** (out of scope here, note in docs): qualified connection references used outside their owner (`jobs.clickhouse-prod` from an arbitrary field), qualified connection-*type* references (`type: jobs.clickhouse`), and cross-workspace `type: task` references. The incident is fixed by cross-workspace actions alone, because the owner action resolves its own `clickhouse-prod` connection in the owner context.
- **Every feature/behaviour change updates docs** (`docs/src/content/docs/`, `CLAUDE.md`) and the plan/spec status.

---

## Task 1: Connection-resolution failures return 400, not 500

Independent, ship-first fix (spec §9). A missing/misnamed connection is a user error; today it becomes a 500.

**Files:**
- Modify: `crates/stroem-server/src/web/api/tasks.rs:501-513` (error-mapping in `execute_task`)
- Test: `crates/stroem-server/tests/integration_test.rs` (new test)

**Interfaces:**
- Consumes: `create_job_for_task(...) -> anyhow::Result<Uuid>` (unchanged), `AppError::BadRequest(String)`.
- Produces: nothing new; behavioural change only.

- [ ] **Step 1: Write the failing test**

Add to `crates/stroem-server/tests/integration_test.rs` (mirror `test_execute_task_creates_job_and_steps` setup; the test workspace must have a task whose input declares a connection-typed field with a default naming a connection that does NOT exist). Assert the HTTP status is 400:

```rust
#[tokio::test]
async fn test_execute_task_missing_connection_returns_400() -> anyhow::Result<()> {
    // setup() builds a workspace; add a task "needs-conn" with input:
    //   { conn: { field_type: "myconntype", default: "does-not-exist" } }
    // and a connection_type "myconntype" but NO connection named "does-not-exist".
    let (router, _pool, _tmp, _c) = setup_with_task_needing_missing_connection().await?;
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspaces/default/tasks/needs-conn/execute")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `DOCKER_HOST=unix:///Users/ala/.orbstack/run/docker.sock cargo test -p stroem-server --test integration_test test_execute_task_missing_connection_returns_400`
Expected: FAIL — status is `500`, not `400`.

- [ ] **Step 3: Widen the error-substring mapping**

In `tasks.rs`, the existing block maps messages containing `"not found" | "required" | "invalid" | "validation"` to `BadRequest`. Connection errors say `"references connection '...' which does not exist"` and `"Failed to resolve connection inputs"`. Add those substrings:

```rust
let msg = e.to_string();
let is_user_error = msg.contains("not found")
    || msg.contains("does not exist")          // connection/action missing
    || msg.contains("resolve connection")      // resolve_connection_inputs context
    || msg.contains("required")
    || msg.contains("invalid")
    || msg.contains("validation");
if is_user_error {
    return Err(AppError::BadRequest(msg));
}
return Err(AppError::Internal(e));
```

- [ ] **Step 4: Run test to verify it passes**

Run: same as Step 2. Expected: PASS.

- [ ] **Step 5: Run the surrounding suite**

Run: `cargo test -p stroem-server --test integration_test`
Expected: PASS (no regressions).

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server/src/web/api/tasks.rs crates/stroem-server/tests/integration_test.rs
git commit -m "fix(api): return 400 (not 500) for missing/unresolvable connection inputs"
```

---

## Task 2: `parse_qualified_ref` helper

A pure parser used by both creation and claim.

**Files:**
- Modify: `crates/stroem-common/src/template.rs` (add function + unit tests at bottom of the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub fn parse_qualified_ref(name: &str) -> (Option<&str>, &str)` — splits on the FIRST `.`: `"jobs.recalc-agg-sessions"` → `(Some("jobs"), "recalc-agg-sessions")`; `"score-all"` → `(None, "score-all")`. A leading/trailing dot or empty workspace part yields `(None, name)` (treat as local, let downstream lookup fail with a clear message).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_parse_qualified_ref() {
    assert_eq!(parse_qualified_ref("jobs.recalc-agg-sessions"), (Some("jobs"), "recalc-agg-sessions"));
    assert_eq!(parse_qualified_ref("score-all"), (None, "score-all"));
    // Only the first dot splits (action names may contain dots via libraries).
    assert_eq!(parse_qualified_ref("a.b.c"), (Some("a"), "b.c"));
    // Degenerate forms are treated as local.
    assert_eq!(parse_qualified_ref(".foo"), (None, ".foo"));
    assert_eq!(parse_qualified_ref("foo."), (None, "foo."));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p stroem-common template::tests::test_parse_qualified_ref`
Expected: FAIL — `parse_qualified_ref` not defined.

- [ ] **Step 3: Implement**

```rust
/// Split a possibly-qualified reference `workspace.item` on the FIRST `.`.
/// Returns `(Some(workspace), item)` for a qualified name, or `(None, name)`
/// for a local name or a degenerate form (empty workspace/item).
pub fn parse_qualified_ref(name: &str) -> (Option<&str>, &str) {
    match name.split_once('.') {
        Some((ws, item)) if !ws.is_empty() && !item.is_empty() => (Some(ws), item),
        _ => (None, name),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: same as Step 2. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/template.rs
git commit -m "feat(common): add parse_qualified_ref for workspace.item names"
```

---

## Task 3: Migration + `job_step` owner columns

**Files:**
- Create: `crates/stroem-db/migrations/043_job_step_action_workspace.sql`
- Modify: `crates/stroem-db/src/repos/job_step.rs` — `STEP_COLUMNS` (`:10`), `JobStepRow` (`:13`), `NewJobStep` (`:56`), `StepInsertRow` (`:88`), `create_steps_tx` INSERT list (`:177`), `cols_per_row` (`:183`, 23→25), `.bind` chain (`:232-255`)
- Test: add a repo round-trip test in `job_step.rs` `#[cfg(test)]` or in `crates/stroem-db/tests/` following the existing repo test pattern.

**Interfaces:**
- Produces: `NewJobStep.action_workspace: Option<String>`, `NewJobStep.action_revision: Option<String>`; same two `Option<String>` fields on `JobStepRow`. NULL semantics: "use the job's workspace/revision".

- [ ] **Step 1: Write the migration**

`crates/stroem-db/migrations/043_job_step_action_workspace.sql`:

```sql
-- Cross-workspace action references: a flow step may reference an action that
-- lives in a DIFFERENT workspace (`owner_ws.action`). The step then executes
-- with the owner workspace's tarball + secrets + connections. These two columns
-- record the owner and a pinned revision. NULL ⇒ the step's action belongs to
-- the job's own workspace (existing behaviour; no backfill required).
ALTER TABLE job_step ADD COLUMN action_workspace TEXT;
ALTER TABLE job_step ADD COLUMN action_revision TEXT;
```

- [ ] **Step 2: Add the fields to the structs**

In `job_step.rs`, add to `JobStepRow` and `NewJobStep` and `StepInsertRow`:

```rust
pub action_workspace: Option<String>,
pub action_revision: Option<String>,
```

Extend `STEP_COLUMNS` (`:10`) to include `action_workspace, action_revision` (append at the end so ordinal positions of existing columns are unchanged).

- [ ] **Step 3: Extend the INSERT**

In `create_steps_tx` (`:177`): append `, action_workspace, action_revision` to the column list; bump `cols_per_row` 23 → 25 (`:183`); append two `.bind(row.action_workspace)` / `.bind(row.action_revision)` at the END of the `.bind` chain (`:255`), matching the new column order. Update the `StepInsertRow` construction to carry the two fields from `NewJobStep`.

- [ ] **Step 4: Write the round-trip test**

Following the repo's existing testcontainer pattern, insert a `NewJobStep` with `action_workspace: Some("jobs")`, `action_revision: Some("abc123")` and read it back via `get_steps_for_job`; assert both fields survive. Also insert one with `None`/`None` and assert NULL round-trips.

- [ ] **Step 5: Run**

Run: `DOCKER_HOST=... cargo test -p stroem-db`
Expected: PASS (migration applies; round-trip asserts hold).

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-db/migrations/043_job_step_action_workspace.sql crates/stroem-db/src/repos/job_step.rs
git commit -m "feat(db): add job_step.action_workspace/action_revision (migration 043)"
```

---

## Task 4: Resolve cross-workspace actions at job creation

Thread the `WorkspaceManager` into job creation; resolve a qualified `owner_ws.action` against the owner config; stamp the owner workspace + pinned revision + the owner action's spec onto the step; copy the fields onto for-each instances.

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs` — `create_job_for_task` (`:30`), `create_child_job_for_task` (`:66`), `create_job_for_task_inner` (`:99`), the per-step action lookup + `NewJobStep` build (`:175`, `:203`), `expand_for_each_steps` (`:522`, instance copy at `:661`).
- Modify call sites: `crates/stroem-server/src/web/api/tasks.rs:487`, `crates/stroem-server/src/web/hooks.rs:101`, `crates/stroem-server/src/web/worker_api/event_source.rs:95`, `crates/stroem-server/src/scheduler.rs` (wherever `create_job_for_task` is called) — pass `&state.workspaces`.
- Test: `crates/stroem-server/tests/integration_test.rs` (two-workspace setup).

**Interfaces:**
- Consumes: `parse_qualified_ref` (Task 2); `WorkspaceManager::get_config(&str) -> Option<Arc<WorkspaceConfig>>` (`workspace/mod.rs:310`); `WorkspaceManager::get_revision(&str) -> Option<String>` (`:331`).
- Produces: `create_job_for_task` and inner/child variants gain a leading `workspaces: &WorkspaceManager` parameter. Steps whose flow-step action is `owner_ws.action` are persisted with `action_workspace = Some(owner_ws)`, `action_revision = Some(<owner pinned rev>)`, `action_name = <bare action>`, and `action_spec`/`required_ability`/`required_tags`/`runner`/retry derived from the **owner** action.

- [ ] **Step 1: Write the failing integration test**

Add a two-workspace setup helper (extend beyond `WorkspaceManager::from_config`; use `from_entries` per `workspace/mod.rs:274`, or build a manager with two folder workspaces). Workspace `A` has task `caller` with a single flow step `run` whose `action: "B.remote"`. Workspace `B` defines action `remote` (a trivial `type: script`, `runner: local`, `script: "echo hi"`). Execute `A/caller`; assert the created `run` step row has `action_workspace == Some("B")`, `action_revision.is_some()`, and `action_type`/`action_spec` came from `B.remote`.

```rust
#[tokio::test]
async fn test_cross_workspace_action_stamps_owner_on_step() -> anyhow::Result<()> {
    let (router, pool, _tmp, _c) = setup_two_workspaces().await?; // A + B
    // POST /api/workspaces/A/tasks/caller/execute {}
    // fetch the job's steps from the DB
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let run = steps.iter().find(|s| s.step_name == "run").unwrap();
    assert_eq!(run.action_workspace.as_deref(), Some("B"));
    assert!(run.action_revision.is_some());
    assert_eq!(run.action_type, "script");
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `DOCKER_HOST=... cargo test -p stroem-server --test integration_test test_cross_workspace_action_stamps_owner_on_step`
Expected: FAIL — today this bails "Action 'B.remote' not found in workspace 'A'" (500), or the columns are NULL.

- [ ] **Step 3: Thread `WorkspaceManager` + resolve the owner action**

Add `workspaces: &WorkspaceManager` as the first parameter of `create_job_for_task`, `create_child_job_for_task`, and `create_job_for_task_inner` (thread it through the boxed-future body and both recursive calls). At the per-step action lookup (`job_creator.rs:175`), replace the direct lookup with owner-aware resolution:

Own the resolved `ActionDef` in a local (owner branch clones out of the `Arc<WorkspaceConfig>`; local branch clones out of `workspace_config`) so a single `&owned_action` reference serves the rest of the loop body:

```rust
// flow_step.action may be "owner_ws.action" (cross-workspace) or a local name.
let (owner_ws, bare_action) = stroem_common::template::parse_qualified_ref(&flow_step.action);
// Cross-workspace only when it isn't already a local/library-flattened key
// AND the named workspace exists (library precedence + backward compat).
let is_cross = owner_ws.is_some()
    && !workspace_config.actions.contains_key(&flow_step.action)
    && owner_ws.map(|ws| workspaces.has_workspace(ws)).unwrap_or(false);

let (owned_action, action_workspace, action_revision, action_name) = if is_cross {
    let ws = owner_ws.unwrap();
    let owner_cfg = workspaces.get_config(ws).await.ok_or_else(|| {
        anyhow::anyhow!("action '{}': workspace '{}' is not available", flow_step.action, ws)
    })?;
    let a = owner_cfg.actions.get(bare_action).cloned().ok_or_else(|| {
        anyhow::anyhow!("action '{}': workspace '{}' has no action '{}'", flow_step.action, ws, bare_action)
    })?;
    (a, Some(ws.to_string()), workspaces.get_revision(ws), bare_action.to_string())
} else {
    let a = workspace_config.actions.get(&flow_step.action).cloned().ok_or_else(|| {
        anyhow::anyhow!("Action '{}' not found in workspace '{}'", flow_step.action, workspace_name)
    })?;
    (a, None, None, flow_step.action.clone())
};
let action = &owned_action;
```

Keep `action_spec = serde_json::to_value(action)`; set `NewJobStep.action_name = action_name`, `NewJobStep.action_workspace = action_workspace`, `NewJobStep.action_revision = action_revision`. `required_ability`/`required_tags`/`runner`/retry derive from `action` (the owner action) exactly as before.

- [ ] **Step 4: Copy owner fields onto for-each instances**

In `expand_for_each_steps` (`job_creator.rs:661` instance clone), copy `action_workspace` and `action_revision` from the placeholder `JobStepRow` onto each instance `NewJobStep` (they're already copying `action_name`/`action_type`/`action_spec`).

- [ ] **Step 5: Update all call sites**

Add `&state.workspaces` as the first argument at every `create_job_for_task`/`create_child_job_for_task` call: `tasks.rs:487`, `hooks.rs:101`, `event_source.rs:95`, and the scheduler call site. Fix the internal recursive calls in `create_job_for_task_inner`.

- [ ] **Step 6: Run test to verify it passes**

Run: same as Step 2. Expected: PASS.

- [ ] **Step 7: Run the crate's unit + integration tests**

Run: `cargo test -p stroem-server --lib && DOCKER_HOST=... cargo test -p stroem-server --test integration_test`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/stroem-server/src/job_creator.rs crates/stroem-server/src/web/api/tasks.rs crates/stroem-server/src/web/hooks.rs crates/stroem-server/src/web/worker_api/event_source.rs crates/stroem-server/src/scheduler.rs crates/stroem-server/tests/integration_test.rs
git commit -m "feat(server): resolve cross-workspace actions at job creation, stamp owner ws+revision"
```

---

## Task 5: Claim in owner context + tell the worker to fetch the owner tarball

At claim time render the owner action against the owner workspace and return the owner `workspace`/`revision` to the worker.

**Files:**
- Modify: `crates/stroem-server/src/web/worker_api/rendering.rs` — `RenderContext` (`:9`), `prepare_step_action_input` (`:117`)
- Modify: `crates/stroem-server/src/web/worker_api/jobs.rs` — `claim_job` (`:366`), the render section (`:459`, `:562-613`) and `ClaimResponse` construction (`:769-793`)
- Test: `rendering.rs` unit test; `crates/stroem-server/tests/integration_test.rs` claim assertion.

**Interfaces:**
- Consumes: `JobStepRow.action_workspace`/`action_revision` (Task 3); `parse_qualified_ref`; `AppState::get_workspace(&str)`.
- Produces: `RenderContext` gains `pub action_workspace: Option<&'a WorkspaceConfig>` (owner config; `None` ⇒ same as `workspace`). `ClaimResponse.workspace`/`.revision` reflect the step's owner when set.

- [ ] **Step 1: Write the failing unit test (rendering)**

In `rendering.rs` tests, build a caller `WorkspaceConfig` whose task `t` has a flow step `s` with `action: "B.remote"` and input `{ conn: "prod" }`; build an owner `WorkspaceConfig` `B` defining action `remote` with input `{ conn: { field_type: "pg" } }`, connection_type `pg`, and connection `prod` (host/etc). Call `prepare_step_action_input` with `action_workspace: Some(&owner)`; assert `conn` resolves to the owner's connection object (host present), i.e. resolution used the OWNER, not the caller (which has no `prod`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p stroem-server rendering::tests::` (new test name)
Expected: FAIL — `RenderContext` has no `action_workspace`; resolution uses caller and can't find `prod`.

- [ ] **Step 3: Make `prepare_step_action_input` owner-aware**

Add `pub action_workspace: Option<&'a WorkspaceConfig>` to `RenderContext` (default `None` at all existing construction sites — update the `#[cfg(test)]` `RenderContext { ... }` literals to add `action_workspace: None,`). In `prepare_step_action_input`, look the flow step up in `ctx.workspace` (caller — unchanged), then resolve the action + connections against the owner:

```rust
let action_ws = ctx.action_workspace.unwrap_or(ctx.workspace);
let (_, bare_action) = stroem_common::template::parse_qualified_ref(&flow_step.action);
let action = match action_ws.actions.get(bare_action) {
    Some(a) => a,
    None => return Ok(rendered_input),
};
if action.input.is_empty() { return Ok(rendered_input); }
let mut input_val = rendered_input.unwrap_or_else(|| serde_json::json!({}));
merge_missing_action_fields(&mut input_val, ctx.job_input, action.input.keys());
let prepared = prepare_action_input(&input_val, &action.input, action_ws)
    .context("Failed to prepare action input")?;
Ok(Some(prepared))
```

`render_step_input` stays as-is (flow-step input renders in the CALLER context: caller step outputs, job input, caller secrets).

- [ ] **Step 4: Run unit test to verify it passes**

Run: same as Step 2. Expected: PASS. Also run `cargo test -p stroem-server rendering::tests` to confirm the `action_workspace: None` additions didn't break existing tests.

- [ ] **Step 5: Wire the owner context into `claim_job`**

In `jobs.rs::claim_job`, after fetching the claimed `JobStepRow` and the `job` row:

```rust
// Determine the workspace whose config/tarball this step's action belongs to.
let owner_ws_name = step.action_workspace.clone().unwrap_or_else(|| job.workspace.clone());
let caller_cfg = state.get_workspace(&job.workspace)...;      // existing lookup
let owner_cfg = if owner_ws_name == job.workspace {
    caller_cfg.clone()
} else {
    state.get_workspace(&owner_ws_name)...                    // load owner config
};
```

Build `RenderContext { workspace: &caller_cfg, action_workspace: Some(&owner_cfg), .. }`. Pass **owner** secrets (`owner_cfg.secrets`) to `render_action_spec` and `render_image` (the action body belongs to the owner). Set the response fields from the step's owner:

```rust
workspace: Some(owner_ws_name),                               // was Some(job.workspace)
revision: step.action_revision.clone().or_else(|| job.revision.clone()),
```

(For local steps `action_workspace` is NULL, so `owner_ws_name == job.workspace` and `action_revision` is NULL → identical to today's behaviour.)

- [ ] **Step 6: Write the failing integration test (claim)**

Extend the two-workspace integration test: register a worker with capability `script`, execute `A/caller` (step action `B.remote`), claim the step via the worker API, and assert `ClaimResponse.workspace == "B"` and `revision == <B's pinned revision>`.

- [ ] **Step 7: Run integration test**

Run: `DOCKER_HOST=... cargo test -p stroem-server --test integration_test` (both the stamp test and the claim test)
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/stroem-server/src/web/worker_api/rendering.rs crates/stroem-server/src/web/worker_api/jobs.rs crates/stroem-server/tests/integration_test.rs
git commit -m "feat(server): claim cross-workspace steps in owner context + fetch owner tarball"
```

---

## Task 6: Validation accepts cross-workspace action references

Ensure server-side validation doesn't flag `owner_ws.action` as an unknown action when the owner workspace/action exists.

**Files:**
- Modify: `crates/stroem-common/src/validation.rs` (action-reference validation) — and/or the server-side post-load validation that has access to all workspace configs.
- Test: `validation.rs` unit test.

**Interfaces:**
- Consumes: `parse_qualified_ref`. Validation needs access to the set of workspaces/actions; if `validation.rs` validates a single `WorkspaceConfig` in isolation, add the cross-workspace check at the server layer that already iterates all configs (mirror how library-prefixed names are accepted). CLI `stroem validate` (no server context) continues to skip dotted names with a warning.

- [ ] **Step 1: Write the failing test**

A `WorkspaceConfig` with a flow-step action `B.remote` should validate cleanly when a resolver reports workspace `B` has action `remote`, and should error with a precise message when it doesn't. Model the test on the existing action-reference validation tests in `validation.rs`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p stroem-common validation::` (new test)
Expected: FAIL — dotted action currently reported as unknown.

- [ ] **Step 3: Implement**

Where an action reference is checked, treat a dotted name as valid if (a) it is a known local/library key, or (b) `parse_qualified_ref` yields `(Some(ws), item)` and the provided resolver confirms `ws` exists with action `item`. Emit `"action '{qualified}': workspace '{ws}' has no action '{item}'"` otherwise.

- [ ] **Step 4: Run to verify it passes**

Run: same as Step 2. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-common/src/validation.rs
git commit -m "feat(validation): accept workspace.action cross-workspace references"
```

---

## Task 7: End-to-end test (real worker, real tarball)

Prove the owner workspace's **files** reach the worker for a cross-workspace step.

**Files:**
- Modify: `tests/e2e.sh` (add a cross-workspace assertion)
- Modify: `docker/server-config.yaml` (two workspaces already exist: `default`, `test`)
- Add fixture files under the e2e `test` workspace: an action `remote-cat` that `cat`s a file present only in `test`; and in `default` a task `xref` with a step `action: test.remote-cat`.

**Interfaces:**
- Consumes: the whole stack (server + worker + runner) via docker-compose, as `e2e.sh` already drives.

- [ ] **Step 1: Add fixtures**

In the `test` workspace folder: `data/marker.txt` containing `CROSS_WS_OK`, and an action `remote-cat` (`type: script`, `runner: local`, `script: "cat data/marker.txt"`). In the `default` workspace: task `xref` with one step `run` → `action: test.remote-cat`.

- [ ] **Step 2: Add the assertion to `e2e.sh`**

Trigger `default/xref`, wait for completion, fetch its logs, and assert they contain `CROSS_WS_OK` (proving the worker executed with the `test` workspace's file, not `default`'s).

- [ ] **Step 3: Run e2e**

Run: `./tests/e2e.sh` (needs Docker)
Expected: the `xref` assertion passes; job completes.

- [ ] **Step 4: Commit**

```bash
git add tests/e2e.sh docker/server-config.yaml tests/**/marker.txt
git commit -m "test(e2e): cross-workspace action executes with owner workspace files"
```

---

## Task 8: Documentation

**Files:**
- Create: `docs/src/content/docs/guides/cross-workspace-references.md`
- Modify: `CLAUDE.md` (add a "Cross-Workspace References" subsection near "Libraries"), `docs/internal/stroem-v2-plan.md` (status), `docs/internal/TODO.md` (mark done + list the deferred follow-ups).

**Interfaces:** none (docs only).

- [ ] **Step 1: Write the guide**

Cover: the `owner_ws.action` syntax; that it needs no library; owner-context execution (files/secrets/connections resolve in the owner); precedence (library first, then workspace); the **open-access** exposure; revision pinning; and the deferred items (qualified connection/type refs, cross-workspace `type: task`). Include the `daily.yaml` before/after from the spec §13.

- [ ] **Step 2: Update CLAUDE.md + internal docs**

Add the subsection describing the mechanism and the `job_step.action_workspace`/`action_revision` columns + the claim-in-owner-context behaviour. Mark the feature in `TODO.md`.

- [ ] **Step 3: Commit**

```bash
git add docs/ CLAUDE.md
git commit -m "docs: cross-workspace references guide + internal notes"
```

---

## Self-Review Notes (for the executor)

- After Task 5, the incident is fixed: migrate `ai_traffic_model/daily.yaml` + `backfill.yaml` to drop the local `clickhouse` input and pass `clickhouse: "clickhouse-prod"` to the `jobs.recalc-agg-sessions` step (resolved in the `jobs` owner context). Do this as a follow-up commit in the workspace repo, not in this codebase.
- Full CI suite before any push: `cargo fmt --check --all` · `cargo clippy --workspace -- -D warnings` · `cargo test --workspace` · `cd ui && bun run lint && bunx tsc --noEmit`.
- Run everything in a dedicated git worktree/feature branch (large feature).
