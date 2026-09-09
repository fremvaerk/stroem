# `continue_when_skipped` + Skip Reasons Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `continue_when_skipped` flow-step flag that lets a step run after all of its dependencies were skipped by choice, backed by a persisted `skip_reason` so a failure-induced skip still stops it; make `continue_on_failure` failure-only.

**Architecture:** The reason is decided inside the pure cascade (`cascade.rs`), carried on `Change::Skip`, applied in memory on the snapshot (so it propagates across passes) and written to a new nullable `job_step.skip_reason` column by the existing batched skip primitive. The all-deps-skipped rule becomes `bypass = continue_when_skipped && (!tainted || continue_on_failure)` where `tainted` means any dependency was skipped as `unreachable` or has no reason. The CLI local runner mirrors the flag; API, MCP and UI surface the reason.

**Tech Stack:** Rust (axum, sqlx runtime queries, serde_yaml), Postgres migration, React 19 + TypeScript + Vitest, Starlight docs, bash e2e.

**Spec:** `docs/superpowers/specs/2026-09-09-continue-when-skipped-design.md` — read it first; section numbers below refer to it.

## Global Constraints

- Work in this worktree only (`/Users/ala/workspace/fremvaerk/stroem/.claude/worktrees/continue-when-skipped`, branch `worktree-continue-when-skipped`). Never `cd` to the main checkout. The cargo target dir is shared with the main checkout (`~/.tmp/cargo`); do not set a different one (disk is at 99%).
- Commit messages: conventional prefix (`feat(cascade): …`, `test(db): …`, `docs: …`). **No `Co-Authored-By` trailers of any kind** (project rule in the user's global CLAUDE.md overrides the harness default).
- Reason strings are exactly `condition`, `empty`, `cascade`, `unreachable` (spec §2.2). `NULL` is treated as `unreachable` by the cascade.
- Flag name is exactly `continue_when_skipped`, serde default `false`.
- Every `FlowStep { .. }` struct literal in the workspace must gain `continue_when_skipped: false` (Task 1); every `Seed { .. }` literal must gain `skip_reason: None` (Task 2); every `Change::Skip { .. }` literal must gain a `reason` (Task 3). Use `cargo build --workspace --tests` to enumerate them — the compiler is the checklist.
- Tests that touch Postgres use testcontainers and need Docker running. Run them with `cargo test -p <crate> --test <file>`; run the pure unit tests with `--lib`.
- Docs are mandatory (project rule): Task 10 is not optional.
- Do not bump the crate version; the release skill does that at release time (0.16.2).

---

### Task 1: The `continue_when_skipped` flag on `FlowStep`

**Files:**
- Modify: `crates/stroem-common/src/models/workflow.rs:384-420` (struct), `:446-486` (reference-step branch), `:490-500` (inline key list), `:568-625` (manual parser)
- Modify: `crates/stroem-common/src/validation.rs:259-266` (warning next to the `sequential` one)
- Modify: `crates/stroem-cli/src/local/inspect.rs:60-73` (extras list)
- Modify (struct literals, add `continue_when_skipped: false,` after `continue_on_failure`): `crates/stroem-common/src/dag.rs:109`, `crates/stroem-server/src/workspace/library.rs:487`, `crates/stroem-cli/src/local/tasks.rs:74`, `crates/stroem-cli/src/local/inspect.rs:127`, `crates/stroem-cli/src/local/run.rs:643`, `crates/stroem-server/src/cascade.rs` (test `fs`), `crates/stroem-server/src/restart.rs:147`, `crates/stroem-server/src/settlement/terminal.rs:274`, `crates/stroem-server/src/settlement/hooks.rs:614,738,836`, `crates/stroem-server/tests/orchestrator_test.rs:129`, `crates/stroem-server/tests/cascade_apply_test.rs`, and any other the compiler reports.
- Test: in-module tests of `workflow.rs`, `validation.rs`, `inspect.rs`

**Interfaces:**
- Produces: `FlowStep.continue_when_skipped: bool` (public field, read by Tasks 3, 7, 9 via the API's task detail JSON which serialises `FlowStep` as-is).

- [ ] **Step 1: Write the failing parser tests**

Add to `crates/stroem-common/src/models/workflow.rs` `mod tests`, right after `test_continue_on_failure_true`:

```rust
    #[test]
    fn test_continue_when_skipped_defaults_false() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: action1
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let step = config.tasks["test"].flow.get("step1").unwrap();
        assert!(!step.continue_when_skipped);
    }

    #[test]
    fn test_continue_when_skipped_true_on_reference_step() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: action1
        depends_on: [step0]
        continue_when_skipped: true
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let step = config.tasks["test"].flow.get("step1").unwrap();
        assert!(step.continue_when_skipped);
        assert!(!step.continue_on_failure, "flags are independent");
    }

    #[test]
    fn test_continue_when_skipped_true_on_inline_step() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        type: script
        script: echo hi
        depends_on: [step0]
        continue_when_skipped: true
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let step = config.tasks["test"].flow.get("step1").unwrap();
        assert!(step.continue_when_skipped);
        // The key must have been routed to the step, not the hoisted action.
        assert!(config.actions.values().all(|a| a.action_type == "script"));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p stroem-common --lib continue_when_skipped`
Expected: compile error `no field continue_when_skipped on type FlowStep`.

- [ ] **Step 3: Add the field and parse it in all three branches**

In the struct (`workflow.rs:397`, right after `pub continue_on_failure: bool,`):

```rust
    /// When true, the step is not cascade-skipped when all of its dependencies
    /// were skipped by choice (`when` false, empty `for_each`). A dependency
    /// skipped because an upstream step failed still skips this step unless
    /// `continue_on_failure` is also set. See spec 2026-09-09 §2.3.
    #[serde(default)]
    pub continue_when_skipped: bool,
```

In `RefStep` (reference-step branch, after `continue_on_failure: bool,`):

```rust
                #[serde(default)]
                continue_when_skipped: bool,
```

and in its `Ok(FlowStep { .. })`: `continue_when_skipped: ref_step.continue_when_skipped,`.

In `step_field_keys` add `"continue_when_skipped",` after `"continue_on_failure",`.

In the manual parser, after the `continue_on_failure` block:

```rust
            let continue_when_skipped: bool = step_map
                .get(serde_yaml::Value::String("continue_when_skipped".into()))
                .map(|v| serde_yaml::from_value(v.clone()).unwrap_or(false))
                .unwrap_or(false);
```

and in its `Ok(FlowStep { .. })`: `continue_when_skipped,`.

- [ ] **Step 4: Fix every struct literal the compiler reports**

Run: `cargo build --workspace --tests 2>&1 | grep -B2 "missing field .continue_when_skipped"`
Add `continue_when_skipped: false,` immediately after `continue_on_failure: …,` in each reported literal (list in **Files** above). Re-run until the build is clean.

- [ ] **Step 5: Run the parser tests**

Run: `cargo test -p stroem-common --lib continue_when_skipped`
Expected: 3 passed.

- [ ] **Step 6: Write the failing validation test**

Add to `crates/stroem-common/src/validation.rs` tests, after `test_sequential_without_for_each_warns`:

```rust
    #[test]
    fn test_continue_when_skipped_without_depends_on_warns() {
        let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      step:
        action: process
        continue_when_skipped: true
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("continue_when_skipped") && w.contains("no depends_on")),
            "Expected warning about continue_when_skipped without depends_on, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_continue_when_skipped_with_depends_on_does_not_warn() {
        let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      first:
        action: process
      step:
        action: process
        depends_on: [first]
        continue_when_skipped: true
"#;
        let config: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().all(|w| !w.contains("continue_when_skipped")),
            "unexpected warning: {:?}",
            warnings
        );
    }
```

- [ ] **Step 7: Run it to verify it fails**

Run: `cargo test -p stroem-common --lib test_continue_when_skipped_without_depends_on_warns`
Expected: FAIL, "Expected warning about continue_when_skipped".

- [ ] **Step 8: Add the warning**

In `validation.rs`, right after the `sequential` warning block (line ~266):

```rust
            // Warn if continue_when_skipped without depends_on
            if step.continue_when_skipped && step.depends_on.is_empty() {
                warnings.push(format!(
                    "Task '{}' step '{}' has continue_when_skipped: true but no depends_on — the flag has no effect",
                    task_name,
                    step_name
                ));
            }
```

- [ ] **Step 9: Run the validation tests**

Run: `cargo test -p stroem-common --lib continue_when_skipped`
Expected: 5 passed.

- [ ] **Step 10: Show the flag in `stroem inspect`**

In `crates/stroem-cli/src/local/inspect.rs` after the `continue_on_failure` extras push (line ~73):

```rust
            if step.continue_when_skipped {
                extras.push("continue_when_skipped".to_string());
            }
```

Add a test next to `inspect_step_with_continue_on_failure` (copy that test's body verbatim, change the flag and the assertion string):

```rust
    #[test]
    fn inspect_step_with_continue_when_skipped() {
        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        let mut step = make_step("act", vec![]);
        step.continue_when_skipped = true;
        flow.insert("step1".to_string(), step);

        let task = TaskDef {
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
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }
```

(`inspect_step_with_continue_on_failure` only asserts that rendering succeeds; this test does the same. The extras string itself is covered by the `extras.push` line being the single source for the printed flag.)

Run: `cargo test -p stroem-cli --lib inspect_step_with_continue_when_skipped`
Expected: PASS.

- [ ] **Step 11: Full unit suite for the touched crates and commit**

Run: `cargo test -p stroem-common -p stroem-cli --lib && cargo fmt --all`
Expected: all pass.

```bash
git add -A crates
git commit -m "feat(model): continue_when_skipped flow-step flag, validation warning, inspect output"
```

---

### Task 2: `job_step.skip_reason` column and the three writers

**Files:**
- Create: `crates/stroem-db/migrations/046_job_step_skip_reason.sql`
- Modify: `crates/stroem-db/src/repos/job_step.rs:8` (`STEP_COLUMNS`), `:60` (row field), `:103` (Default), `:214-220` (`Seed`), `:372-386` (`skip_steps_tx`), `:481-513` (`seed_steps_tx`), `:908-928` (`mark_skipped`)
- Modify: `crates/stroem-server/src/restart.rs:96-108` (two `Seed` literals), `crates/stroem-server/tests/restart_integration_test.rs:1297`, `crates/stroem-db/tests/integration_test.rs:5460-5478` (`Seed` literals)
- Test: `crates/stroem-db/tests/job_step_status_tests.rs`, `crates/stroem-db/tests/integration_test.rs`

**Interfaces:**
- Produces: `JobStepRow.skip_reason: Option<String>`; `JobStepRepo::skip_steps_tx(executor, job_id, names: &[String], reason: &str) -> Result<u64>`; `JobStepRepo::mark_skipped(pool, job_id, step_name, reason: &str)`; `Seed.skip_reason: Option<String>` written by `seed_steps_tx`.

- [ ] **Step 1: Write the migration**

`crates/stroem-db/migrations/046_job_step_skip_reason.sql`:

```sql
-- continue_when_skipped (spec 2026-09-09 §5): why a step is `skipped`.
-- One of 'condition' | 'empty' | 'cascade' | 'unreachable'. NULL on rows written
-- before this migration or by an older replica; the cascade treats NULL as
-- 'unreachable' (the pre-change behaviour). No CHECK: the Rust enum is the
-- authority so a future reason needs no migration.
ALTER TABLE job_step ADD COLUMN skip_reason TEXT;
```

- [ ] **Step 2: Write the failing DB tests**

In `crates/stroem-db/tests/job_step_status_tests.rs` change the two `mark_skipped` calls to pass a reason and extend the first test:

```rust
    JobStepRepo::mark_skipped(&pool, job_id, "step1", "unreachable").await?;
    // ...existing assertions...
    assert_eq!(step.skip_reason.as_deref(), Some("unreachable"));
```

(second test: `JobStepRepo::mark_skipped(&pool, job_id, "step1", "unreachable").await?;` and add `assert!(steps[0].skip_reason.is_none(), "guard must not write a reason either");`).

Add a new test in the same file:

```rust
/// `skip_steps_tx` writes the reason on every row it skips and only on
/// pending rows.
#[tokio::test]
async fn test_skip_steps_tx_writes_reason_on_pending_rows_only() -> Result<()> {
    let (pool, _container) = setup_db().await?;
    let job_id = make_job(&pool, "skip-reason").await?;
    JobStepRepo::create_steps(
        &pool,
        &[
            make_step(job_id, "a", "pending"),
            make_step(job_id, "b", "pending"),
            make_step(job_id, "c", "ready"),
        ],
    )
    .await?;

    let mut tx = pool.begin().await?;
    let n = JobStepRepo::skip_steps_tx(
        &mut *tx,
        job_id,
        &["a".to_string(), "b".to_string(), "c".to_string()],
        "cascade",
    )
    .await?;
    tx.commit().await?;
    assert_eq!(n, 2, "only the two pending rows are skipped");

    let by: std::collections::HashMap<_, _> = JobStepRepo::get_steps_for_job(&pool, job_id)
        .await?
        .into_iter()
        .map(|s| (s.step_name.clone(), s))
        .collect();
    assert_eq!(by["a"].skip_reason.as_deref(), Some("cascade"));
    assert_eq!(by["b"].skip_reason.as_deref(), Some("cascade"));
    assert_eq!(by["c"].status, "ready");
    assert!(by["c"].skip_reason.is_none());
    Ok(())
}
```

In `crates/stroem-db/tests/integration_test.rs` test `test_seed_steps_tx_overwrites_status_output_and_flags_row`: add a third created step `plain_step(job_id, "c", "pending")`, a third seed

```rust
            Seed {
                step_name: "c".into(),
                status: "skipped".into(),
                output: None,
                error_message: None,
                skip_reason: Some("condition".into()),
            },
```

and assertions `assert_eq!(by["c"].status, "skipped"); assert_eq!(by["c"].skip_reason.as_deref(), Some("condition"));`. Add `skip_reason: None,` to the two existing seeds in that test and to any other `Seed { .. }` literal in the file.

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p stroem-db --test job_step_status_tests --test integration_test skip 2>&1 | tail -20`
Expected: compile errors (`mark_skipped` takes 3 arguments, no field `skip_reason`).

- [ ] **Step 4: Implement the column, row field and writers**

`job_step.rs`:

1. `STEP_COLUMNS`: append `, skip_reason` at the end of the string.
2. `JobStepRow`: after `pub carried_over: bool,` add
   ```rust
       /// Why the step is `skipped` (spec 2026-09-09 §2.2): `condition` | `empty` |
       /// `cascade` | `unreachable`. `None` before migration 046 or on rows an older
       /// replica wrote; the cascade treats `None` as `unreachable`.
       pub skip_reason: Option<String>,
   ```
   and `skip_reason: None,` in the `Default` impl after `carried_over: false,`.
3. `Seed`: after `pub error_message: Option<String>,` add `pub skip_reason: Option<String>,`.
4. `skip_steps_tx`:
   ```rust
       pub async fn skip_steps_tx<'e, E>(
           executor: E,
           job_id: Uuid,
           names: &[String],
           reason: &str,
       ) -> Result<u64>
       where
           E: sqlx::Executor<'e, Database = sqlx::Postgres>,
       {
           let r = sqlx::query(
               "UPDATE job_step SET status = 'skipped', skip_reason = $3, completed_at = NOW() \
                WHERE job_id = $1 AND step_name = ANY($2) AND status = 'pending'",
           )
           .bind(job_id)
           .bind(names)
           .bind(reason)
           .execute(executor)
           .await
           .context("skip_steps_tx")?;
           Ok(r.rows_affected())
       }
   ```
5. `mark_skipped(pool, job_id, step_name, reason: &str)`: SQL becomes `SET status = 'skipped', skip_reason = $3, completed_at = NOW()` with `.bind(reason)` after `.bind(step_name)`.
6. `seed_steps_tx`: SQL `SET status = $3, output = $4, error_message = $5, skip_reason = $6, completed_at = NOW(), carried_over = TRUE, …` and `.bind(&seed.skip_reason)` after `.bind(&seed.error_message)`.

`restart.rs` (`compute_restart_set`): terminal seed gets `skip_reason: src.skip_reason.clone(),`; the cancelled seed gets `skip_reason: None,`. `restart_integration_test.rs:1297` seed literal gets `skip_reason: None,`.

The only caller of `skip_steps_tx` is `cascade::apply` — pass `"unreachable"` there for now as a placeholder value that Task 3 replaces (`JobStepRepo::skip_steps_tx(&mut **tx, job_id, &names, "unreachable")`). This keeps the build green between tasks; Task 3 removes it.

- [ ] **Step 5: Build and run the DB tests**

Run: `cargo build --workspace --tests && cargo test -p stroem-db --test job_step_status_tests --test integration_test skip`
Expected: all pass (`test_mark_skipped_*`, `test_skip_steps_tx_writes_reason_on_pending_rows_only`, `test_seed_steps_tx_*`).

- [ ] **Step 6: Commit**

```bash
git add -A crates
git commit -m "feat(db): job_step.skip_reason column (046); skip/mark/seed writers take a reason"
```

---

### Task 3: Skip reasons and the `continue_when_skipped` rule in the cascade

**Files:**
- Modify: `crates/stroem-server/src/cascade.rs` — `Change` enum (`:15-35`), `Snapshot` (`:120-160`), predicates (`:238-260`), `phase_rollup` R5 (`:290-300`), `phase_promote` (`:357-397`), `phase_skip_unreachable` (`:399-418`), `phase_placeholders` (`:440-495`), `apply` (`:682-691`), test helpers (`:895-1000`)
- Modify: `crates/stroem-server/tests/cascade_apply_test.rs:150`, `crates/stroem-server/tests/orchestrator_test.rs:1068` (`Change::Skip` literals)
- Test: `cascade.rs` in-module tests

**Interfaces:**
- Produces: `pub enum SkipReason { Condition, Empty, Cascade, Unreachable }` with `pub fn as_str(self) -> &'static str`; `Change::Skip { step: String, reason: SkipReason }`. Consumed by Tasks 4, 8.

- [ ] **Step 1: Add the test helpers and the failing rule tests**

In `cascade.rs` `mod tests`, next to `fs_cof`:

```rust
    fn fs_cws(deps: &[&str]) -> FlowStep {
        FlowStep {
            continue_when_skipped: true,
            ..fs(deps)
        }
    }
    fn fs_cws_cof(deps: &[&str]) -> FlowStep {
        FlowStep {
            continue_when_skipped: true,
            continue_on_failure: true,
            ..fs(deps)
        }
    }
    fn row_skipped(name: &str, reason: &str) -> JobStepRow {
        JobStepRow {
            skip_reason: Some(reason.to_string()),
            ..row(name, "skipped")
        }
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
```

Update `names()` so its `Skip` arm reads `Change::Skip { step, .. } => format!("skip:{step}"),`.

Add a new test section `// ── continue_when_skipped + skip reasons (spec 2026-09-09) ──`:

```rust
    #[test]
    fn cws_all_deps_skipped_by_condition_promotes() {
        let t = task(vec![("a", fs(&[])), ("b", fs_cws(&["a"]))]);
        let rows = vec![row_skipped("a", "condition"), row("b", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
    }

    #[test]
    fn cws_all_deps_skipped_by_empty_loop_promotes() {
        let t = task(vec![("a", fs(&[])), ("b", fs_cws(&["a"]))]);
        let rows = vec![row_skipped("a", "empty"), row("b", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
    }

    #[test]
    fn cws_with_falsy_own_when_skips_as_condition() {
        let t = task(vec![("a", fs(&[])), ("b", fs_cws(&["a"]))]);
        let rows = vec![row_skipped("a", "condition"), row_when("b", "pending", "false")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(skips(&plan), [s("b", "condition")]);
    }

    #[test]
    fn cof_alone_no_longer_bypasses_all_deps_skipped() {
        // Spec §2.4: continue_on_failure is failure-only.
        let t = task(vec![("a", fs(&[])), ("b", fs_cof(&["a"]))]);
        let rows = vec![row_skipped("a", "condition"), row("b", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(skips(&plan), [s("b", "cascade")]);
    }

    #[test]
    fn cws_with_unreachable_dep_is_skipped_unreachable() {
        let t = task(vec![("a", fs(&[])), ("b", fs_cws(&["a"]))]);
        let rows = vec![row_skipped("a", "unreachable"), row("b", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable")]);
    }

    #[test]
    fn cws_with_mixed_condition_and_unreachable_deps_is_skipped_unreachable() {
        // "any tainted dependency" (spec §2.2): one benign branch must not launder a failure.
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs_cws(&["a", "b"]))]);
        let rows = vec![
            row_skipped("a", "condition"),
            row_skipped("b", "unreachable"),
            row("c", "pending"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(skips(&plan), [s("c", "unreachable")]);
    }

    #[test]
    fn cws_and_cof_with_unreachable_dep_promotes() {
        let t = task(vec![("a", fs(&[])), ("b", fs_cws_cof(&["a"]))]);
        let rows = vec![row_skipped("a", "unreachable"), row("b", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["promote:b"]);
    }

    #[test]
    fn null_reason_counts_as_unreachable() {
        // Pre-migration / older-replica rows (spec §2.2).
        let t = task(vec![("a", fs(&[])), ("b", fs_cws(&["a"]))]);
        let rows = vec![row("a", "skipped"), row("b", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable")]);
    }

    #[test]
    fn unreachable_propagates_through_a_chain_in_one_run() {
        // a failed → b (no cof) → c (cws): b unreachable (R3), c unreachable (R1, tainted).
        let t = task(vec![("a", fs(&[])), ("b", fs(&["a"])), ("c", fs_cws(&["b"]))]);
        let rows = vec![row("a", "failed"), row("b", "pending"), row("c", "pending")];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(skips(&plan), [s("b", "unreachable"), s("c", "unreachable")]);
    }

    #[test]
    fn condition_skip_becomes_cascade_downstream_and_cws_runs() {
        // x completed → a (when false) → b → c (cws): a condition, b cascade, c promoted.
        let t = task(vec![
            ("x", fs(&[])),
            ("a", fs(&["x"])),
            ("b", fs(&["a"])),
            ("c", fs_cws(&["b"])),
        ]);
        let rows = vec![
            row("x", "completed"),
            row_when("a", "pending", "false"),
            row("b", "pending"),
            row("c", "pending"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(skips(&plan), [s("a", "condition"), s("b", "cascade")]);
        assert!(names(&plan).contains(&"promote:c".to_string()), "{:?}", names(&plan));
    }

    #[test]
    fn three_pass_chain_reasons_match_statuses() {
        // Spec §4.1 pass-boundary invariant: each link is decided one pass later
        // than its predecessor and the reason travels with the status.
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
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(
            skips(&plan),
            [s("a", "condition"), s("b", "cascade"), s("c", "cascade")]
        );
        let mut snap = Snapshot::new(rows.clone());
        for c in &plan.changes {
            snap.apply(c);
        }
        assert_eq!(snap.skip_reason("a"), Some("condition"));
        assert_eq!(snap.skip_reason("b"), Some("cascade"));
        assert_eq!(snap.skip_reason("c"), Some("cascade"));
    }

    #[test]
    fn mixed_completed_and_unreachable_skipped_deps_still_promote() {
        // The reason only matters when EVERY dep is skipped (spec §2.3).
        let t = task(vec![("a", fs(&[])), ("b", fs(&[])), ("c", fs(&["a", "b"]))]);
        let rows = vec![
            row("a", "completed"),
            row_skipped("b", "unreachable"),
            row("c", "pending"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        assert_eq!(names(&plan), ["promote:c"]);
    }

    #[test]
    fn reason_on_r3_unreachable_and_r5_sequential_stop() {
        let t = task(vec![
            ("a", fs(&[])),
            ("b", fs(&["a"])),
            ("x", fs_seq(&[])),
        ]);
        let rows = vec![
            row("a", "failed"),
            row("b", "pending"),
            placeholder("x", "running", "[1,2,3]"),
            instance("x", 0, "failed", None),
            instance("x", 1, "pending", None),
            instance("x", 2, "pending", None),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        let sk = skips(&plan);
        assert!(sk.contains(&s("b", "unreachable")), "{sk:?}");
        assert!(sk.contains(&s("x[1]", "unreachable")), "{sk:?}");
        assert!(sk.contains(&s("x[2]", "unreachable")), "{sk:?}");
    }

    #[test]
    fn reason_on_r4_placeholder_condition_empty_unreachable_and_cascade() {
        let t = task(vec![
            ("root", fs(&[])),
            ("dead", fs(&[])),
            ("gone", fs(&[])),
            ("p_when", fs(&["root"])),
            ("p_empty", fs(&["root"])),
            ("p_unreach", fs(&["dead"])),
            ("p_cascade", fs(&["gone"])),
            ("p_cws", fs_cws(&["gone"])),
        ]);
        let rows = vec![
            row("root", "completed"),
            row("dead", "failed"),
            row_skipped("gone", "condition"),
            JobStepRow {
                when_condition: Some("false".to_string()),
                ..placeholder("p_when", "pending", "[1]")
            },
            placeholder("p_empty", "pending", "[]"),
            placeholder("p_unreach", "pending", "[1]"),
            placeholder("p_cascade", "pending", "[1]"),
            placeholder("p_cws", "pending", "[1]"),
        ];
        let plan = run(&t, &job(None), &rows, Some(&ws())).unwrap();
        let sk = skips(&plan);
        assert!(sk.contains(&s("p_when", "condition")), "{sk:?}");
        assert!(sk.contains(&s("p_empty", "empty")), "{sk:?}");
        assert!(sk.contains(&s("p_unreach", "unreachable")), "{sk:?}");
        assert!(sk.contains(&s("p_cascade", "cascade")), "{sk:?}");
        assert!(
            names(&plan).contains(&"expand:p_cws:1".to_string()),
            "cws placeholder expands after a condition skip: {:?}",
            names(&plan)
        );
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p stroem-server --lib cascade::tests 2>&1 | tail -5`
Expected: compile error (`SkipReason` not found / `Change::Skip` has no field `reason`).

- [ ] **Step 3: Implement the reason enum, the change payload and the snapshot**

Below the `Change` enum:

```rust
/// Why a step was skipped (spec 2026-09-09 §2.2). Persisted verbatim as
/// `job_step.skip_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The step's own `when` rendered falsy.
    Condition,
    /// The step's `for_each` produced zero items.
    Empty,
    /// All dependencies skipped, none of them unreachable.
    Cascade,
    /// A dependency failed or was cancelled without `continue_on_failure`, or a
    /// dependency was itself unreachable (propagation).
    Unreachable,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Condition => "condition",
            SkipReason::Empty => "empty",
            SkipReason::Cascade => "cascade",
            SkipReason::Unreachable => "unreachable",
        }
    }
}
```

`Change::Skip` becomes `Skip { step: String, reason: SkipReason },`.

In `Snapshot`, add next to `status`:

```rust
    fn skip_reason(&self, name: &str) -> Option<&str> {
        self.index
            .get(name)
            .and_then(|&i| self.rows[i].skip_reason.as_deref())
    }
```

and in `Snapshot::apply`:

```rust
            Change::Skip { step, reason } => {
                if let Some(r) = self.get_mut(step) {
                    r.status = SKIPPED.to_string();
                    r.skip_reason = Some(reason.as_str().to_string());
                }
            }
```

- [ ] **Step 4: Implement the predicates and rewire every site**

After `all_deps_skipped`:

```rust
/// Only meaningful when `all_deps_skipped` holds: does any skipped dependency
/// carry a failure? `None` (pre-046 rows, older replicas) counts as unreachable
/// (spec §2.2).
fn tainted(snap: &Snapshot, fs: &FlowStep) -> bool {
    fs.depends_on
        .iter()
        .any(|d| matches!(snap.skip_reason(d), None | Some("unreachable")))
}

/// Spec §2.3. Call only when `all_deps_skipped(snap, fs)`. `Some(skip)` when the
/// step is cascade-skipped; `None` when it may fall through to the normal path.
fn all_skipped_decision(snap: &Snapshot, fs: &FlowStep, step: &str) -> Option<Change> {
    let tainted = tainted(snap, fs);
    let bypass = fs.continue_when_skipped && (!tainted || fs.continue_on_failure);
    if bypass {
        return None;
    }
    Some(Change::Skip {
        step: step.to_string(),
        reason: if tainted {
            SkipReason::Unreachable
        } else {
            SkipReason::Cascade
        },
    })
}
```

`phase_promote` (R1/R2):

```rust
        if all_deps_skipped(snap, fs) {
            if let Some(skip) = all_skipped_decision(snap, fs, &r.step_name) {
                out.push(skip);
                continue;
            }
        }
        if !deps_satisfied(snap, fs) {
            continue;
        }
        match (&r.when_condition, ctx) {
            (None, _) => out.push(Change::Promote { step: r.step_name.clone() }),
            (Some(_), None) => {}
            (Some(w), Some(ctx)) => match stroem_common::template::evaluate_condition(w, ctx) {
                Ok(true) => out.push(Change::Promote { step: r.step_name.clone() }),
                Ok(false) => out.push(Change::Skip {
                    step: r.step_name.clone(),
                    reason: SkipReason::Condition,
                }),
                Err(e) => out.push(Change::Fail { /* unchanged */ }),
            },
        }
```

`phase_skip_unreachable` (R3): `Change::Skip { step: r.step_name.clone(), reason: SkipReason::Unreachable }`.

`phase_rollup` R5: `Change::Skip { step: i.step_name.clone(), reason: SkipReason::Unreachable }`.

`phase_placeholders` (R4), in order:
- deps-not-satisfied-because-failed branch: `reason: SkipReason::Unreachable`
- replace `if all_deps_skipped(snap, fs) && !fs.continue_on_failure { … }` with the same `all_skipped_decision` block as in `phase_promote`
- `when` false: `reason: SkipReason::Condition`
- `items.is_empty()`: `reason: SkipReason::Empty`

`apply`, the `Change::Skip` arm:

```rust
            Change::Skip { .. } => {
                // A run of consecutive skips is batched per reason, in plan order
                // (spec §4.3); each bucket carries its own row-count guard.
                let mut buckets: Vec<(SkipReason, Vec<String>)> = Vec::new();
                while let Some(Change::Skip { step, reason }) = changes.get(i) {
                    match buckets.iter_mut().find(|(r, _)| r == reason) {
                        Some((_, names)) => names.push(step.clone()),
                        None => buckets.push((*reason, vec![step.clone()])),
                    }
                    i += 1;
                }
                for (reason, names) in buckets {
                    let n =
                        JobStepRepo::skip_steps_tx(&mut **tx, job_id, &names, reason.as_str())
                            .await?;
                    expect_rows(n, names.len(), &names.join(","))?;
                    a.skipped += names.len();
                }
            }
```

Fix the two literals outside the crate: `cascade_apply_test.rs:150` → `Change::Skip { step: "b".into(), reason: SkipReason::Cascade }` (import `SkipReason` from `stroem_server::cascade`), `orchestrator_test.rs:1068` → `Change::Skip { step: "c".to_string(), reason: stroem_server::cascade::SkipReason::Condition }`.

- [ ] **Step 5: Run the whole cascade unit module**

Run: `cargo test -p stroem-server --lib cascade::tests`
Expected: all pass, including the pre-existing tests. If a pre-existing test asserted that `continue_on_failure` alone bypassed the all-deps-skipped rule, that expectation is the §2.4 change: update its assertion to `skip` and add the comment `// spec 2026-09-09 §2.4: cof is failure-only`.

- [ ] **Step 6: Build the integration tests and commit**

Run: `cargo build --workspace --tests && cargo fmt --all && cargo clippy -p stroem-server -- -D warnings`
Expected: clean.

```bash
git add -A crates
git commit -m "feat(cascade): skip reasons on Change::Skip; continue_when_skipped rule; cof failure-only"
```

---

### Task 4: `apply` writes reasons per bucket (DB integration)

**Files:**
- Modify: `crates/stroem-server/tests/cascade_apply_test.rs`

**Interfaces:**
- Consumes: `Change::Skip { step, reason }`, `SkipReason`, `apply`, `ApplyError::GuardMiss` (Task 3); `JobStepRow.skip_reason` (Task 2).

- [ ] **Step 1: Write the tests**

Append to `cascade_apply_test.rs` (uses that file's `setup_db`, `create_job`, `step` helpers; add `SkipReason` to the `use stroem_server::cascade::{…}` line):

```rust
/// Spec §4.3: consecutive skips are batched per reason and every row gets the
/// reason the plan named for it.
#[tokio::test]
async fn apply_writes_skip_reason_per_bucket() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "pending"),
            step(job_id, "b", "pending"),
            step(job_id, "c", "pending"),
            step(job_id, "d", "pending"),
        ],
    )
    .await?;
    let plan = Plan {
        changes: vec![
            Change::Skip { step: "a".into(), reason: SkipReason::Cascade },
            Change::Skip { step: "b".into(), reason: SkipReason::Cascade },
            Change::Skip { step: "c".into(), reason: SkipReason::Unreachable },
            Change::Skip { step: "d".into(), reason: SkipReason::Condition },
        ],
    };
    let mut tx = pool.begin().await?;
    let applied = apply(&mut tx, job_id, &plan).await.unwrap();
    tx.commit().await?;
    assert_eq!(applied.skipped, 4);

    let by: HashMap<_, _> = JobStepRepo::get_steps_for_job(&pool, job_id)
        .await?
        .into_iter()
        .map(|s| (s.step_name.clone(), s))
        .collect();
    assert_eq!(by["a"].skip_reason.as_deref(), Some("cascade"));
    assert_eq!(by["b"].skip_reason.as_deref(), Some("cascade"));
    assert_eq!(by["c"].skip_reason.as_deref(), Some("unreachable"));
    assert_eq!(by["d"].skip_reason.as_deref(), Some("condition"));
    Ok(())
}

/// A stale row in the SECOND bucket is still a guard miss, and the first
/// bucket's write is rolled back with it.
#[tokio::test]
async fn apply_skip_guard_miss_in_later_bucket_rolls_back_earlier_bucket() -> Result<()> {
    let (pool, _c) = setup_db().await?;
    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[step(job_id, "a", "pending"), step(job_id, "b", "ready")],
    )
    .await?;
    let plan = Plan {
        changes: vec![
            Change::Skip { step: "a".into(), reason: SkipReason::Cascade },
            Change::Skip { step: "b".into(), reason: SkipReason::Unreachable },
        ],
    };
    let mut tx = pool.begin().await?;
    let err = apply(&mut tx, job_id, &plan).await.unwrap_err();
    assert!(matches!(err, ApplyError::GuardMiss { ref step } if step == "b"), "{err}");
    tx.rollback().await?;

    let by: HashMap<_, _> = JobStepRepo::get_steps_for_job(&pool, job_id)
        .await?
        .into_iter()
        .map(|s| (s.step_name.clone(), s))
        .collect();
    assert_eq!(by["a"].status, "pending");
    assert!(by["a"].skip_reason.is_none());
    Ok(())
}
```

- [ ] **Step 2: Run them**

Run: `cargo test -p stroem-server --test cascade_apply_test apply_ 2>&1 | tail -15`
Expected: both pass (the implementation exists from Task 3; these tests pin the DB contract). If `apply_skip_guard_miss_in_later_bucket_rolls_back_earlier_bucket` fails because the guard miss surfaced for `"a,b"` instead of `"b"`, the buckets were merged: fix `apply` so each bucket is its own statement.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/tests/cascade_apply_test.rs
git commit -m "test(cascade): apply writes skip_reason per bucket; guard miss rolls back all buckets"
```

---

### Task 5: Restart carries the reason; server integration scenarios; writer contract

**Files:**
- Modify: `crates/stroem-server/tests/restart_integration_test.rs:494-545` (existing test gains two assertions)
- Modify: `crates/stroem-server/tests/orchestrator_test.rs` (new helper + three tests)

**Interfaces:**
- Consumes: `settlement::cascade_and_settle(pool, job_id, task, ws)` (existing), helpers `setup_db`, `create_job`, `step`, `step_when`, `flow_step`, `flow_step_when`, `make_task`, `step_statuses`, `step_for_each` (all exist in `orchestrator_test.rs`).

- [ ] **Step 1: Restart assertions**

In `restart_set_entirely_skipped_settles_failed_at_creation`, after `assert!(!by["c"].carried_over);` add:

```rust
    // Spec §5: a carried skipped row keeps its reason; the freshly cascaded
    // `c` is unreachable because its only dependency `b` was unreachable.
    assert!(by["b"].carried_over);
    assert_eq!(by["b"].skip_reason.as_deref(), Some("unreachable"));
    assert_eq!(by["c"].skip_reason.as_deref(), Some("unreachable"));
```

Run: `cargo test -p stroem-server --test restart_integration_test restart_set_entirely_skipped_settles_failed_at_creation`
Expected: PASS.

- [ ] **Step 2: Orchestrator scenarios (write, run, expect pass)**

In `orchestrator_test.rs` add a helper after `flow_step_cof`:

```rust
/// Build a `FlowStep` with `continue_when_skipped = true`.
fn flow_step_cws(depends_on: Vec<&str>) -> FlowStep {
    FlowStep {
        continue_when_skipped: true,
        ..flow_step(depends_on)
    }
}
```

and three tests at the end of the file:

```rust
// ─── continue_when_skipped (spec 2026-09-09) ─────────────────────────────────

/// A → B (`when` false) → C (`continue_when_skipped`): C runs and the job
/// completes once C completes.
#[tokio::test]
async fn test_continue_when_skipped_runs_after_condition_skip() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert("b".to_string(), flow_step_when(vec!["a"], "{{ a.output.go }}"));
    flow.insert("c".to_string(), flow_step_cws(vec!["b"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step_when(job_id, "b", "pending", "{{ a.output.go }}"),
            step(job_id, "c", "pending"),
        ],
    )
    .await?;
    let ws = WorkspaceConfig::new();

    JobStepRepo::mark_completed(&pool, job_id, "a", Some(json!({"go": false}))).await?;
    stroem_server::settlement::cascade_and_settle(&pool, job_id, &task, &ws).await?;

    let statuses = step_statuses(&pool, job_id).await;
    assert_eq!(statuses["b"], "skipped");
    assert_eq!(statuses["c"], "ready", "C runs although its only dependency was skipped");
    let rows: HashMap<_, _> = JobStepRepo::get_steps_for_job(&pool, job_id)
        .await?
        .into_iter()
        .map(|s| (s.step_name.clone(), s))
        .collect();
    assert_eq!(rows["b"].skip_reason.as_deref(), Some("condition"));
    assert!(rows["c"].skip_reason.is_none());

    JobStepRepo::mark_completed(&pool, job_id, "c", Some(json!({"ok": true}))).await?;
    stroem_server::settlement::cascade_and_settle(&pool, job_id, &task, &ws).await?;
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    Ok(())
}

/// A fails → B skipped unreachable → C (`continue_when_skipped`) is ALSO
/// skipped unreachable; the job fails.
#[tokio::test]
async fn test_continue_when_skipped_does_not_run_after_upstream_failure() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step(vec![]));
    flow.insert("b".to_string(), flow_step(vec!["a"]));
    flow.insert("c".to_string(), flow_step_cws(vec!["b"]));
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "a", "ready"),
            step(job_id, "b", "pending"),
            step(job_id, "c", "pending"),
        ],
    )
    .await?;
    let ws = WorkspaceConfig::new();

    JobStepRepo::mark_failed(&pool, job_id, "a", "boom").await?;
    stroem_server::settlement::cascade_and_settle(&pool, job_id, &task, &ws).await?;

    let rows: HashMap<_, _> = JobStepRepo::get_steps_for_job(&pool, job_id)
        .await?
        .into_iter()
        .map(|s| (s.step_name.clone(), s))
        .collect();
    assert_eq!(rows["b"].status, "skipped");
    assert_eq!(rows["b"].skip_reason.as_deref(), Some("unreachable"));
    assert_eq!(rows["c"].status, "skipped");
    assert_eq!(rows["c"].skip_reason.as_deref(), Some("unreachable"));
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");
    Ok(())
}

/// Writer contract (spec §11.2): after a cascade that produces every kind of
/// skip, no skipped row is left without a reason.
#[tokio::test]
async fn test_every_skipped_row_has_a_reason() -> Result<()> {
    let (pool, _container) = setup_db().await?;

    let mut flow = HashMap::new();
    flow.insert("x".to_string(), flow_step(vec![]));
    flow.insert("cond".to_string(), flow_step_when(vec!["x"], "false"));
    flow.insert("casc".to_string(), flow_step(vec!["cond"]));
    flow.insert("f".to_string(), flow_step(vec![]));
    flow.insert("unreach".to_string(), flow_step(vec!["f"]));
    flow.insert(
        "empty".to_string(),
        FlowStep {
            for_each: Some(json!([])),
            ..flow_step(vec!["x"])
        },
    );
    let task = make_task(flow);

    let job_id = create_job(&pool).await;
    JobStepRepo::create_steps(
        &pool,
        &[
            step(job_id, "x", "ready"),
            step_when(job_id, "cond", "pending", "false"),
            step(job_id, "casc", "pending"),
            step(job_id, "f", "ready"),
            step(job_id, "unreach", "pending"),
            step_for_each(job_id, "empty", "pending", "[]"),
        ],
    )
    .await?;
    let ws = WorkspaceConfig::new();

    JobStepRepo::mark_completed(&pool, job_id, "x", None).await?;
    JobStepRepo::mark_failed(&pool, job_id, "f", "boom").await?;
    stroem_server::settlement::cascade_and_settle(&pool, job_id, &task, &ws).await?;

    let rows = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let reasons: HashMap<_, _> = rows
        .iter()
        .filter(|s| s.status == "skipped")
        .map(|s| (s.step_name.clone(), s.skip_reason.clone()))
        .collect();
    assert_eq!(reasons["cond"].as_deref(), Some("condition"));
    assert_eq!(reasons["casc"].as_deref(), Some("cascade"));
    assert_eq!(reasons["unreach"].as_deref(), Some("unreachable"));
    assert_eq!(reasons["empty"].as_deref(), Some("empty"));
    let missing: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM job_step WHERE job_id = $1 AND status = 'skipped' AND skip_reason IS NULL",
    )
    .bind(job_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(missing, 0);
    Ok(())
}
```

If `step_for_each`'s signature differs from `(job_id, name, status, expr)`, read its definition in the same file and match it. If `JobStepRepo::mark_failed`'s error argument type differs, match the existing callers in the file.

Run: `cargo test -p stroem-server --test orchestrator_test continue_when_skipped every_skipped_row`
Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/tests
git commit -m "test(server): continue_when_skipped end-to-end through settlement; restart keeps skip_reason; writer contract"
```

---

### Task 6: API and MCP expose `skip_reason`

**Files:**
- Modify: `crates/stroem-server/src/web/api/jobs.rs:340` (after `"carried_over"`), `crates/stroem-server/src/mcp/tools.rs:580` (after `"error_message"`)
- Test: `crates/stroem-server/tests/mcp_test.rs:783-787`

- [ ] **Step 1: Add the assertion to the MCP test**

In `mcp_test.rs` test 3, after `assert!(!steps.is_empty(), …);`:

```rust
    assert!(
        steps[0].get("skip_reason").is_some(),
        "skip_reason must be present on every step (null while pending): {steps:?}"
    );
```

Run: `cargo test -p stroem-server --test mcp_test get_job_status 2>&1 | tail -5`
Expected: FAIL on the new assertion.

- [ ] **Step 2: Add the field to both DTOs**

`jobs.rs` step DTO: `"skip_reason": step.skip_reason,` after `"carried_over": step.carried_over,`.
`mcp/tools.rs` `get_job_status` step DTO: `"skip_reason": step.skip_reason,` after `"error_message": step.error_message,`.

Run the MCP test again. Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/src/web/api/jobs.rs crates/stroem-server/src/mcp/tools.rs crates/stroem-server/tests/mcp_test.rs
git commit -m "feat(api): expose job_step.skip_reason on job detail and MCP get_job_status"
```

---

### Task 7: CLI local runner honours the flag

**Files:**
- Modify: `crates/stroem-cli/src/local/run.rs:180-191` (pre-run check), `:558-590` (`cascade_skip`)
- Test: `run.rs` in-module tests

- [ ] **Step 1: Write the failing tests**

Next to `test_cascade_skip_partial`:

```rust
    #[test]
    fn test_cascade_skip_respects_continue_when_skipped() {
        let mut flow = HashMap::new();
        flow.insert("a".to_string(), make_step("act", vec![]));
        let mut b = make_step("act", vec!["a"]);
        b.continue_when_skipped = true;
        flow.insert("b".to_string(), b);
        flow.insert("c".to_string(), make_step("act", vec!["b"]));

        let mut completed = HashSet::new();
        let mut skipped = HashSet::new();
        let mut outputs = HashMap::new();
        completed.insert("a".to_string());
        skipped.insert("a".to_string());
        outputs.insert("a".to_string(), None);

        cascade_skip(&flow, &mut completed, &mut skipped, &mut outputs);

        assert!(!skipped.contains("b"), "b opted in to run after a skip");
        assert!(!skipped.contains("c"), "c waits for b, which has not completed");
    }
```

Next to `test_run_when_false_skips`:

```rust
    #[tokio::test]
    async fn test_run_continue_when_skipped_runs_after_false_condition() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo hello
tasks:
  conditional:
    flow:
      check:
        action: greet
        when: "false"
      report:
        action: greet
        depends_on: [check]
        continue_when_skipped: true
      follow:
        action: greet
        depends_on: [check]
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["conditional"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 1, "report ran");
        assert_eq!(summary.skipped, 2, "check (condition) and follow (cascade)");
        assert_eq!(summary.failed, 0);
    }
```

Run: `cargo test -p stroem-cli --lib continue_when_skipped`
Expected: both FAIL (b/c skipped; completed == 0).

- [ ] **Step 2: Implement**

Pre-run check (`run.rs:181`):

```rust
            // Check all-deps-skipped cascade (spec 2026-09-09 §2.5: the CLI
            // never produces `unreachable`, so the flag alone decides)
            if !step.depends_on.is_empty()
                && !step.continue_when_skipped
                && step.depends_on.iter().all(|d| skipped.contains(d))
            {
```

`cascade_skip`, replace `if step.depends_on.is_empty() { continue; }` with:

```rust
            if step.depends_on.is_empty() || step.continue_when_skipped {
                continue;
            }
```

Run: `cargo test -p stroem-cli --lib` — Expected: all pass.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-cli/src/local/run.rs
git commit -m "feat(cli): stroem run honours continue_when_skipped"
```

---

### Task 8: UI — types, badge labels, timeline, step detail, task detail

**Files:**
- Modify: `ui/src/lib/types.ts:43-47` (`FlowStep`), `:86-118` (`JobStep`)
- Create: `ui/src/lib/skip-reason.ts`, `ui/src/lib/__tests__/skip-reason.test.ts`
- Modify: `ui/src/components/step-timeline.tsx:154-158` and `:353-357` (two "condition" badges), `ui/src/components/step-detail.tsx:170-196` (logs tab), `ui/src/pages/task-detail.tsx:446-448`
- Modify (factories: add `skip_reason: null,`): `ui/src/components/__tests__/step-timeline.test.tsx`, `ui/src/components/__tests__/step-detail.test.tsx`, `ui/src/pages/__tests__/job-detail.test.tsx`, `ui/src/components/approval-card.test.tsx`, `ui/src/lib/__tests__/eta.test.ts`
- Test: `ui/src/lib/__tests__/skip-reason.test.ts`, `ui/src/components/__tests__/step-timeline.test.tsx`, `ui/src/components/__tests__/step-detail.test.tsx`

- [ ] **Step 1: Types**

`types.ts` `FlowStep`: after `continue_on_failure?: boolean;` add `continue_when_skipped?: boolean;`.
`JobStep`: after `carried_over: boolean;` add `skip_reason: SkipReason | null;` and at the top of the file (or just above `JobStep`) add

```ts
export type SkipReason = "condition" | "empty" | "cascade" | "unreachable";
```

Add `skip_reason: null,` to every `makeStep`/factory listed in **Files**. Run `cd ui && bunx tsc --noEmit` — Expected: clean.

- [ ] **Step 2: Failing label tests**

`ui/src/lib/__tests__/skip-reason.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { skipBadgeLabel, skipExplanation } from "../skip-reason";

describe("skipBadgeLabel", () => {
  it("maps every reason to a short label", () => {
    expect(skipBadgeLabel("condition", false)).toBe("condition");
    expect(skipBadgeLabel("empty", false)).toBe("empty loop");
    expect(skipBadgeLabel("cascade", false)).toBe("upstream skipped");
    expect(skipBadgeLabel("unreachable", false)).toBe("upstream failed");
  });

  it("falls back to the when heuristic for rows without a reason", () => {
    expect(skipBadgeLabel(null, true)).toBe("condition");
    expect(skipBadgeLabel(null, false)).toBeNull();
  });
});

describe("skipExplanation", () => {
  it("explains each reason in one sentence", () => {
    expect(skipExplanation("condition")).toContain("when condition was false");
    expect(skipExplanation("empty")).toContain("no items");
    expect(skipExplanation("cascade")).toContain("every dependency was skipped");
    expect(skipExplanation("unreachable")).toContain("upstream step failed");
    expect(skipExplanation(null)).toBe("Skipped.");
  });
});
```

Run: `cd ui && bunx vitest run src/lib/__tests__/skip-reason.test.ts` — Expected: FAIL (module not found).

- [ ] **Step 3: Implement the labels**

`ui/src/lib/skip-reason.ts`:

```ts
import type { SkipReason } from "./types";

/**
 * Short badge text for a skipped step. A `null` reason (rows written before
 * migration 046) falls back to the old `when`-based heuristic.
 */
export function skipBadgeLabel(reason: SkipReason | null, hasWhen: boolean): string | null {
  switch (reason) {
    case "condition":
      return "condition";
    case "empty":
      return "empty loop";
    case "cascade":
      return "upstream skipped";
    case "unreachable":
      return "upstream failed";
    default:
      return hasWhen ? "condition" : null;
  }
}

/** One-sentence explanation for the step detail panel. */
export function skipExplanation(reason: SkipReason | null): string {
  switch (reason) {
    case "condition":
      return "Skipped: the step's when condition was false.";
    case "empty":
      return "Skipped: for_each produced no items.";
    case "cascade":
      return "Skipped: every dependency was skipped.";
    case "unreachable":
      return "Skipped: an upstream step failed or was cancelled.";
    default:
      return "Skipped.";
  }
}
```

Run the test again — Expected: PASS.

- [ ] **Step 4: Failing timeline and detail tests**

`step-timeline.test.tsx`: extend the existing type import to `import type { JobStep, SkipReason, StepDurationStats } from "@/lib/types";`, then add a new `describe("skip reason badge")` block inside `describe("StepTimeline")`:

```tsx
  describe("skip reason badge", () => {
    const cases: Array<[SkipReason, string]> = [
      ["condition", "condition"],
      ["empty", "empty loop"],
      ["cascade", "upstream skipped"],
      ["unreachable", "upstream failed"],
    ];
    it.each(cases)("shows '%s' as '%s'", (reason, label) => {
      renderTimeline([makeStep({ status: "skipped", skip_reason: reason })]);
      expect(screen.getByTestId("step-skip-build")).toHaveTextContent(label);
    });

    it("falls back to 'condition' for a reasonless row with a when", () => {
      renderTimeline([
        makeStep({ status: "skipped", skip_reason: null, when_condition: "{{ x }}" }),
      ]);
      expect(screen.getByTestId("step-skip-build")).toHaveTextContent("condition");
    });

    it("shows no badge for a reasonless row without a when", () => {
      renderTimeline([makeStep({ status: "skipped", skip_reason: null })]);
      expect(screen.queryByTestId("step-skip-build")).not.toBeInTheDocument();
    });

    it("keeps the blue 'when' badge on a non-skipped conditional step", () => {
      renderTimeline([makeStep({ status: "completed", when_condition: "{{ x }}" })]);
      expect(screen.getByText("when")).toBeInTheDocument();
      expect(screen.queryByTestId("step-skip-build")).not.toBeInTheDocument();
    });
  });
```

`step-detail.test.tsx`, new block:

```tsx
describe("StepDetail skipped steps", () => {
  it("explains why the step was skipped instead of showing logs", async () => {
    renderDetail(makeStep({ status: "skipped", skip_reason: "unreachable" }));
    const notice = await screen.findByTestId("skipped-notice");
    expect(notice.textContent).toContain("upstream step failed");
    expect(getStepLogs).not.toHaveBeenCalled();
  });
});
```

Run: `cd ui && bunx vitest run src/components/__tests__/step-timeline.test.tsx src/components/__tests__/step-detail.test.tsx` — Expected: the new tests FAIL.

- [ ] **Step 5: Implement the components**

`step-timeline.tsx`: import `{ skipBadgeLabel } from "@/lib/skip-reason";`. Replace the step-row block

```tsx
            {step.when_condition && step.status === "skipped" && (
              <span className="rounded bg-amber-100 …">condition</span>
            )}
```

with

```tsx
            {step.status === "skipped" && skipBadgeLabel(step.skip_reason, !!step.when_condition) && (
              <span
                data-testid={`step-skip-${step.step_name}`}
                className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/30 dark:text-amber-400"
              >
                {skipBadgeLabel(step.skip_reason, !!step.when_condition)}
              </span>
            )}
```

and the placeholder-row block (`placeholder.when_condition && placeholder.status === "skipped"`) the same way using `placeholder` and `data-testid={`step-skip-${placeholder.step_name}`}`. Leave the blue `when` badges untouched.

`step-detail.tsx`: import `{ skipExplanation } from "@/lib/skip-reason";`, add `const isSkipped = step.status === "skipped";` under `isCarriedOver`, extend the effect's early return to `if (isCarriedOver || isSkipped) { setLoadingLogs(false); return; }` (and add `isSkipped` to its dependency array if the effect lists `isCarriedOver` there), and in the logs tab insert a branch between the carried-over notice and the spinner:

```tsx
          ) : isSkipped ? (
            <p
              data-testid="skipped-notice"
              className="rounded-md border bg-muted/40 px-3 py-2 text-sm text-muted-foreground"
            >
              {skipExplanation(step.skip_reason)}
            </p>
          ) : loadingLogs ? (
```

`task-detail.tsx`, after the `when` fragment:

```tsx
                    {step.continue_on_failure && <> &middot; continue on failure</>}
                    {step.continue_when_skipped && <> &middot; continue when skipped</>}
```

- [ ] **Step 6: Run the UI suite and commit**

Run: `cd ui && bunx tsc --noEmit && bun run lint && bunx vitest run`
Expected: clean, all pass.

```bash
git add ui
git commit -m "feat(ui): skip reason badges and explanation; continue_when_skipped in task detail"
```

---

### Task 9: End-to-end scenario

**Files:**
- Create: `tests/e2e-workspace/conditional.yaml`
- Modify: `tests/e2e.sh` (insert before the `# --- Summary ---` block)

- [ ] **Step 1: Workspace task**

`tests/e2e-workspace/conditional.yaml`:

```yaml
actions:
  say:
    type: script
    runner: local
    language: shell
    script: echo "ran"

tasks:
  conditional-report:
    mode: distributed
    input:
      run_check:
        type: boolean
        default: false
    flow:
      optional-check:
        action: say
        when: "{{ input.run_check }}"
      report:
        action: say
        depends_on: [optional-check]
        continue_when_skipped: true
      follow-up:
        action: say
        depends_on: [optional-check]
```

- [ ] **Step 2: Scenario in `e2e.sh`**

Insert before `# --- Summary ---`:

```bash
# --- continue_when_skipped: report runs after a condition skip, follow-up cascades ---
info "Triggering conditional-report task (continue_when_skipped)..."
EXEC_RESP_CWS=$(acurl -X POST "$BASE_URL/api/workspaces/test/tasks/conditional-report/execute" \
    -H "Content-Type: application/json" \
    -d '{"input": {}}')
CWS_JOB_ID=$(echo "$EXEC_RESP_CWS" | jq -r '.job_id')
if [ -z "$CWS_JOB_ID" ] || [ "$CWS_JOB_ID" = "null" ]; then
    fail "conditional-report execute failed: $EXEC_RESP_CWS"
fi
pass "conditional-report job created: $CWS_JOB_ID"

info "Waiting for conditional-report job to complete..."
CWS_POLLED=0
CWS_STATUS="pending"
while [ "$CWS_STATUS" != "completed" ] && [ "$CWS_STATUS" != "failed" ]; do
    sleep 2
    CWS_POLLED=$((CWS_POLLED + 2))
    if [ "$CWS_POLLED" -ge "$MAX_POLL" ]; then
        acurl "$BASE_URL/api/jobs/$CWS_JOB_ID" | jq .
        fail "conditional-report did not reach terminal state within ${MAX_POLL}s (status: $CWS_STATUS)"
    fi
    CWS_DETAIL=$(acurl "$BASE_URL/api/jobs/$CWS_JOB_ID")
    CWS_STATUS=$(echo "$CWS_DETAIL" | jq -r '.status')
    printf "."
done
echo ""
if [ "$CWS_STATUS" != "completed" ]; then
    echo "$CWS_DETAIL" | jq .
    fail "conditional-report job failed (status: $CWS_STATUS)"
fi
pass "conditional-report job completed (${CWS_POLLED}s)"

CWS_CHECK=$(echo "$CWS_DETAIL" | jq -r '.steps[] | select(.step_name == "optional-check") | "\(.status)/\(.skip_reason)"')
CWS_REPORT=$(echo "$CWS_DETAIL" | jq -r '.steps[] | select(.step_name == "report") | "\(.status)/\(.skip_reason)"')
CWS_FOLLOW=$(echo "$CWS_DETAIL" | jq -r '.steps[] | select(.step_name == "follow-up") | "\(.status)/\(.skip_reason)"')
[ "$CWS_CHECK" = "skipped/condition" ] || { echo "$CWS_DETAIL" | jq .steps; fail "optional-check expected skipped/condition, got $CWS_CHECK"; }
[ "$CWS_REPORT" = "completed/null" ] || { echo "$CWS_DETAIL" | jq .steps; fail "report expected completed/null, got $CWS_REPORT"; }
[ "$CWS_FOLLOW" = "skipped/cascade" ] || { echo "$CWS_DETAIL" | jq .steps; fail "follow-up expected skipped/cascade, got $CWS_FOLLOW"; }
pass "continue_when_skipped: report ran, follow-up cascaded, skip reasons recorded"
```

The `test` workspace is the e2e workspace mounted by `docker-compose.yml` (`./tests/e2e-workspace:/workspace-test:ro`); the artifacts scenario above uses the same path.

- [ ] **Step 3: Run the e2e suite (needs Docker; ~10 min on first build)**

Run: `./tests/e2e.sh 2>&1 | tail -30`
Expected: ends with `All E2E tests passed!` and the new `pass` line. If Docker is unavailable on this machine, say so in the task report; do not mark the step done.

- [ ] **Step 4: Commit**

```bash
git add tests/e2e-workspace/conditional.yaml tests/e2e.sh
git commit -m "test(e2e): continue_when_skipped scenario with skip reasons"
```

---

### Task 10: Documentation

**Files:**
- Modify: `docs/src/content/docs/guides/conditionals.md:129-152`, `docs/src/content/docs/guides/workflow-basics.md:264-288`, `docs/src/content/docs/reference/workflow-yaml.md:406,441-443`, `docs/src/content/docs/reference/api.md:305-320`
- Create: `docs/src/content/docs/operations/migration-046.md`
- Modify: `CLAUDE.md` (§ Step Cascade line 217 area, § Conditional Flow Steps lines 271-277), `docs/internal/TODO.md` (new section at the end)

- [ ] **Step 1: `conditionals.md`**

Replace the "How it works" paragraph at line 129 with:

```markdown
**How it works**: Skipped dependencies count as satisfied. A convergence step runs as long as at least one of its dependencies completed. If **all** dependencies are skipped, the step is skipped too (mid-branch cascade) — unless it sets `continue_when_skipped: true`, see below.
```

After the "Convergence Pattern" section (before "## Root Step Conditions") add:

````markdown
## Running After a Skipped Branch

Sometimes a step should run even when the only step it depends on was skipped — a report after an optional check, or a merge after an if/else where both arms may be off. Set `continue_when_skipped: true` on that step:

```yaml
tasks:
  optional-check:
    input:
      run_check: { type: boolean, default: false }
    flow:
      check:
        action: run-check
        when: "{{ input.run_check }}"

      report:
        action: write-report
        depends_on: [check]
        continue_when_skipped: true
        # Runs whether check completed or was skipped by its condition.
        # {{ check.output }} is null when check was skipped.
```

The flag covers skips **by choice** only: a `when` that rendered false, an empty `for_each`, or a chain of such skips. If a dependency was skipped because an upstream step **failed** or was cancelled, the step is still skipped. To run after failures as well, set `continue_on_failure: true` too:

```yaml
      cleanup:
        action: remove-temp-files
        depends_on: [last-step]
        continue_when_skipped: true
        continue_on_failure: true
        # Runs no matter what happened upstream.
```

`continue_on_failure` on its own lets a step run when a **direct** dependency failed; it does not lift the all-dependencies-skipped rule.

The step's own `when` is still evaluated: `continue_when_skipped` decides whether the step is considered at all, `when` decides whether it runs.

## Skip Reasons

Every skipped step records why it was skipped. The job detail page shows it as a badge and the API returns it as `skip_reason` on the step:

| Reason | Meaning |
|---|---|
| `condition` | the step's own `when` rendered falsy |
| `empty` | the step's `for_each` produced no items |
| `cascade` | every dependency was skipped, all of them by choice |
| `unreachable` | a dependency failed or was cancelled, or a dependency was itself unreachable |

`unreachable` travels down a chain: if `a` fails, `b` is unreachable and so is anything that depends only on `b`. That is what stops a `continue_when_skipped` step from running after a failure.
````

In "Root Step Conditions", replace the four comment lines under `main:` with:

```yaml
        # If setup is skipped and main has no other deps, main is also
        # cascade-skipped. Add `continue_when_skipped: true` to run it anyway.
```

- [ ] **Step 2: `workflow-basics.md`**

Replace the paragraph starting `Note: **skipped** dependencies …` (line 266) with:

```markdown
Note: **skipped** dependencies (from conditional `when` expressions) are treated differently from **failed** dependencies. A step with a skipped dependency proceeds normally as long as at least one dependency completed; if every dependency was skipped, the step is skipped too unless it sets `continue_when_skipped: true`. See the [Conditionals guide](/guides/conditionals/) for branching patterns and skip reasons.
```

Replace the last paragraph of the section (`You do **not** need continue_on_failure …`, line 287) with:

```markdown
`continue_on_failure` is about failures only. To run a step whose dependencies were all skipped, use `continue_when_skipped: true`; to run a step no matter what happened upstream, set both.
```

- [ ] **Step 3: `workflow-yaml.md`**

Step-fields table, after the `continue_on_failure` row:

```markdown
| `continue_when_skipped` | bool | `false` | Run even if every dependency was skipped by a `when` or empty `for_each`. Does not cover dependencies skipped because of an upstream failure — combine with `continue_on_failure` for that |
```

Change the `continue_on_failure` description to `Run even if a direct dependency fails or is cancelled; mark own failure as tolerable`.

Replace the two paragraphs under "### Dependencies" (lines 441-443) with:

```markdown
Steps without `depends_on` start immediately. Failed dependencies cause downstream steps to be skipped unless `continue_on_failure: true`.

Skipped dependencies (from `when` conditions or empty loops) are treated as satisfied — downstream steps still run as long as at least one dependency completed. A step whose dependencies were **all** skipped is skipped too, unless it sets `continue_when_skipped: true`. Every skipped step records a `skip_reason` (`condition`, `empty`, `cascade`, `unreachable`); see the [Conditionals guide](/guides/conditionals/#skip-reasons).
```

- [ ] **Step 4: `api.md`**

In the job detail example step object add `"skip_reason": null` after `"error_message": null` (keep JSON valid: add a comma to the preceding line). Below the example, add one sentence: `skip_reason` is `null` unless `status` is `skipped`; then one of `condition`, `empty`, `cascade`, `unreachable` (see the Conditionals guide).

- [ ] **Step 5: `operations/migration-046.md`**

```markdown
---
title: Migration 046 — skip reasons and continue_when_skipped
description: What changes for continue_on_failure, and how to get the old behaviour back
---

Migration `046_job_step_skip_reason.sql` adds a nullable `job_step.skip_reason`
column. Alongside it, release 0.16.2 changes one behaviour of
`continue_on_failure` and adds the `continue_when_skipped` flow-step flag.

## What changes on upgrade

Before 0.16.2, `continue_on_failure: true` also let a step run when **all** of
its dependencies were skipped. That was undocumented. From 0.16.2,
`continue_on_failure` is failure-only: it lets a step run when a **direct**
dependency failed or was cancelled, and marks the step's own failure as
tolerable. A step whose dependencies were all skipped is now skipped even with
`continue_on_failure`.

## Who is affected

Only steps that have `continue_on_failure: true` **and** whose dependencies can
all be skipped in the same run (every dependency has a `when`, or sits behind
one). Steps with at least one dependency that always runs are not affected.

## The fix

Add `continue_when_skipped: true` to the affected step. To keep the exact old
behaviour ("run no matter what"), keep `continue_on_failure: true` and add
`continue_when_skipped: true` next to it.

## The column

`skip_reason` is `NULL` on rows written before the migration. The cascade treats
`NULL` as `unreachable`, so a `continue_when_skipped` step behind such rows
stays skipped — the same outcome as before the upgrade. The migration is
additive and can run before or after the binaries roll out; during a mixed
fleet an old replica writes `NULL` reasons, which the new replicas handle the
same way.
```

- [ ] **Step 6: `CLAUDE.md`**

In § Step Cascade, append to the "Guards" bullet's paragraph group a new bullet:

```markdown
- `Change::Skip { step, reason: SkipReason }` — the reason (`condition` | `empty` | `cascade` | `unreachable`) is applied to the in-memory snapshot row so later passes see it, and `apply` batches a run of skips per reason (one `skip_steps_tx` per bucket, each guarded). Spec `docs/superpowers/specs/2026-09-09-continue-when-skipped-design.md`.
```

In § Conditional Flow Steps replace the "All-deps-skipped rule" bullet with:

```markdown
- All-deps-skipped rule: if ALL deps are skipped the step is cascade-skipped, unless `continue_when_skipped: true` AND no dep is `tainted` (skipped `unreachable` or with a NULL `skip_reason`); a tainted step still runs when it also has `continue_on_failure`. Formula: `bypass = cws && (!tainted || cof)` (`cascade.rs::all_skipped_decision`). `continue_on_failure` alone is failure-only since 0.16.2 (it used to bypass this rule). Mixed deps (≥1 completed) always promote, whatever the reasons.
- Skip reasons: `job_step.skip_reason` (migration 046) — `condition` (own `when` false), `empty` (zero `for_each` items), `cascade` (all deps skipped by choice), `unreachable` (R3/R4/R5 failure skips, and any all-skipped step with a tainted dep). Every writer of `status='skipped'` (`skip_steps_tx`, `mark_skipped`, `seed_steps_tx`) takes the reason. The CLI local runner mirrors the flag but never produces `unreachable` (it aborts on failure).
```

- [ ] **Step 7: `TODO.md`**

Append:

```markdown
## continue_when_skipped + Skip Reasons (2026-09-09)

- [ ] Expose `skip_reason` in the Tera template context (`{{ step.skip_reason }}`) so a `when` can branch on why an upstream step was skipped.
- [ ] `stroem run` (CLI) aborts on the first untolerated failure instead of skipping dependents as `unreachable`; pre-existing divergence from the server, now documented.
- [ ] `hook.failed_steps` has no skipped-steps counterpart; add `hook.skipped_steps` with reasons if a hook ever needs it.
```

- [ ] **Step 8: Build the docs and commit**

Run: `cd docs && bun install --frozen-lockfile && bun run build 2>&1 | tail -5`
Expected: build succeeds (this also regenerates `llms.txt`; include it in the commit if it changed).

```bash
git add docs CLAUDE.md
git commit -m "docs: continue_when_skipped, skip reasons, continue_on_failure split (migration 046)"
```

---

### Task 11: Full CI check suite

**Files:** none new.

- [ ] **Step 1: Rust**

```bash
cargo fmt --check --all
cargo clippy --workspace -- -D warnings
cargo test --workspace 2>&1 | grep -E "^(test result|failures:|---- |error)" 
```

Expected: fmt clean, clippy clean, every `test result:` line `ok`. The `log_storage::tests` module is known to be flaky under parallel runs; re-run `cargo test -p stroem-server --lib log_storage` alone if it is the only failure.

- [ ] **Step 2: UI**

```bash
cd ui && bun run lint && bunx tsc --noEmit && bunx vitest run
```

Expected: clean.

- [ ] **Step 3: Report**

Report the exact `test result:` lines and any test you could not run (Docker-dependent integration tests, e2e). Do not claim green for anything not executed.
