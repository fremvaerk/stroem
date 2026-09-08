# Step Cascade — Design

**Status:** Draft (2026-09-08)
**Date:** 2026-09-08
**Origin:** architecture review 2026-09-07/08, candidate "Make the step cascade a pure
module"; first internal seam of the later "Job settlement" module (separate branch, not
covered here).

## 1. Problem

Moving a job's steps after something changes — promote steps whose dependencies are
satisfied, cascade-skip steps whose dependencies are skipped, evaluate `when`, expand
`for_each` placeholders, retire placeholders, roll a loop up — is one fixpoint. Today it
is written twice, verbatim, and half of it lives in the DB crate:

- `orchestrator.rs:46-113` (`on_step_completed`, runs after every step completion)
- `job_creator.rs:618-647` (post-commit `init` block, runs once at creation)
- `stroem-db/src/repos/job_step.rs:672-818` `promote_ready_steps` evaluates Tera `when`
  conditions inside the repository (`:757`); `:855-908` `skip_unreachable_steps`
- `job_creator.rs:945-1152` `expand_for_each_steps`
- `job_creator.rs:1216-1353` `check_loop_completion`, a pre-step keyed on the one step
  that just completed, called from `job_recovery.rs:226` and `:601`

Consequences, all observed in production or flagged in code:

- The placeholder lifecycle is split over three functions in two crates. The two
  stuck-job incidents recorded in CLAUDE.md (2026-09-02: `expand_for_each_steps` called
  after `on_step_completed` as a separate pass; 2026-09-07: a placeholder retired by no
  one) came from that split.
- Every fixpoint iteration performs four full `job_step` fetches and two `job` fetches,
  each a separate autocommitted statement against the pool.
- `expand_for_each_steps` inserts instance rows, then marks the placeholder running, in
  two statements with no transaction (`job_creator.rs:1134-1138`). A crash between them
  leaves the instances committed and the placeholder `pending`; the idempotency guard at
  `:975-979` then sees `"{step}[0]"` and `continue`s forever. The placeholder is stuck.
- Read-classify-write with no lock: two orchestrators on the same job classify against
  stale snapshots. The `AND status = 'pending'` guards prevent corrupt transitions but not
  lost cascade work (`job_step.rs:684-690`, in-file TODO).
- Nothing in this path is testable without a Postgres container. `orchestrator.rs` has
  zero in-module tests; the 34 tests in `orchestrator_test.rs` all spin a container.

## 2. Decisions

Settled in the design walk on 2026-09-08 (Q1–Q13).

1. **Scope:** cascade only on this branch. The settlement module (absorbing
   `finalize_created_job` and `reconcile_settled_children`) is a separate branch with its
   own design.
2. **Behaviour policy:** preserving, except defects the new shape fixes by construction
   with no extra code. Each such fix is its own commit with its own regression test.
3. **Home:** `crates/stroem-server/src/cascade.rs`. Orchestration policy, not persistence.
4. **Shape:** a pure `run` returning `Vec<Change>` (closed enum); one `apply` composes
   transaction-taking repo primitives. No `StepStore` trait: its only second adapter would
   be a test double.
5. **Tests:** the existing container suite stays untouched as the oracle until the last
   commit; unit tests are added at the cascade interface; redundant container tests are
   pruned in one final commit.
6. **`run` inputs:** `task`, job row, step snapshot, `Option<&WorkspaceConfig>`. The
   module builds the template context itself from its own snapshot via the existing pure
   `job_creator::build_step_render_context`.
7. **Fixpoint in memory:** `run` iterates to the fixpoint on its own snapshot, synthesising
   instance rows after an expand, and returns the full change list. One read, one write.
8. **Atomic apply under the job lock:** read → run → apply inside one transaction that
   first locks the job row with `SELECT … FOR UPDATE`. Settlement stays outside.
9. **Loop rollup joins the cascade** as global rules over the snapshot;
   `check_loop_completion` is deleted. Caller order stays retry-reset → cascade → settle.
10. **Repo functions deleted:** `promote_ready_steps`, `skip_unreachable_steps` and their
    four DB-level tests. The repo keeps only transaction-taking primitives.
11. **`None` workspace-config mode preserved** exactly (`when` steps stay pending,
    expansion skipped). Removed in the settlement branch.
12. **Vocabulary:** new `CONTEXT.md` glossary at the repo root, cross-referenced from
    CLAUDE.md; CLAUDE.md gains a `### Step Cascade` section.
13. **Job-row lock shared with artifact uploads** (`web/worker_api/artifacts.rs:165`).
    Accepted and documented; contention is bounded by one upload and scoped to one job.

## 3. Non-Goals

- Changing any user-visible semantics of `when`, `for_each`, `sequential`,
  `continue_on_failure`, or step status transitions.
- Settlement (`settle_if_all_terminal`), terminal handling, propagation, hooks, retry.
  They are called after the cascade exactly as today.
- Moving `build_step_render_context` or unifying the render-context builders
  (review candidate 7). The cascade calls the existing builder.
- Removing the `Option<&WorkspaceConfig>` parameter from `on_step_completed`
  (about 80 test call sites pass `None`).
- Locking strategy beyond the job row (no advisory locks).
- Any schema migration. No new columns, constraints, or triggers.

## 4. Semantics

### 4.1 Vocabulary

See `CONTEXT.md`. In short: a **step cascade** is the fixpoint that moves a job's
non-running steps after a change; a **placeholder** is the `for_each` row that stands in
for its **instances** (`step[0]`, `step[1]`, …); **retirement** is a placeholder leaving
`pending` without expanding (skipped or failed); **rollup** is a running placeholder
becoming `completed`/`failed` once every instance is terminal.

### 4.2 Interface

```rust
// crates/stroem-server/src/cascade.rs

/// One state transition the cascade wants applied. Closed enum.
pub enum Change {
    /// pending → ready
    Promote { step: String },
    /// pending → skipped
    Skip { step: String },
    /// pending → failed (when-evaluation or for_each-expression error)
    Fail { step: String, error: String },
    /// Insert instance rows; placeholder pending|ready → running; job pending → running.
    Expand { placeholder: String, instances: Vec<NewJobStep> },
    /// running placeholder → completed(output) | failed(error)
    Rollup { placeholder: String, outcome: RollupOutcome },
}

pub enum RollupOutcome {
    Completed(serde_json::Value),
    Failed(String),
}

/// Pure. Iterates to the fixpoint on an in-memory copy of `steps`.
/// `workspace_config == None` reproduces today's "no template context" mode.
pub fn run(
    task: &TaskDef,
    job: &JobRow,
    steps: &[JobStepRow],
    workspace_config: Option<&WorkspaceConfig>,
) -> Vec<Change>;

/// Applies `changes` inside `tx`, in order, with per-row status guards (§4.6).
pub async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    changes: &[Change],
) -> Result<Applied>;

/// The one entry point both callers use:
/// BEGIN; lock job row FOR UPDATE; read job + steps; run; apply; COMMIT.
/// Logs one summary line. Returns the applied changes for callers and tests.
pub async fn execute(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<Vec<Change>>;
```

`Applied` is a small summary (counts per variant, plus the names of any change whose
status guard matched zero rows) used for the log line and for tests.

### 4.3 Rules evaluated by `run`

Each pass evaluates the rules below over the current snapshot, collects changes, applies
them to the snapshot, and repeats until a pass yields nothing. Rule inputs come from the
same sources they do today (row `when_condition`, flow `continue_on_failure` /
`sequential` / `depends_on`) — the implementation must not change which source a rule
reads.

**R1 — Cascade-skip (all deps skipped).** A `pending` step present in the flow, not a
placeholder, with a non-empty dependency list, every dependency `skipped`, and no
`continue_on_failure` → `Skip`. Evaluated before `when` (today's
`test_truthy_when_overridden_by_all_deps_skipped_cascade`).

**R2 — Promote.** A `pending` step present in the flow, not a placeholder, whose every
dependency is `completed` or `skipped`, or `failed`/`cancelled` when the step has
`continue_on_failure`:
- no `when` → `Promote`;
- `when` and no workspace config → no change (stays `pending`);
- `when` renders truthy → `Promote`; falsy → `Skip`; render error →
  `Fail { error: "when condition error: {:#}" }`.

Truthiness is `stroem_common::template::evaluate_condition` unchanged.

**R3 — Skip unreachable.** A `pending` step present in the flow, not a placeholder,
without `continue_on_failure`, with at least one dependency `failed` or `cancelled` →
`Skip`. A `skipped` dependency does not trigger this rule.

**R4 — Placeholder retirement / expansion.** A `pending` placeholder (row
`for_each_expr.is_some()`), evaluated only when a workspace config is present (with
`None`, placeholders stay `pending`):
- dependencies not all satisfied, and at least one `failed`/`cancelled` without
  `continue_on_failure` → `Skip`;
- dependencies not all satisfied otherwise → no change;
- non-empty dependency list, all `skipped`, no `continue_on_failure` → `Skip`;
- `when` falsy → `Skip`; `when` error → `Fail { "when condition error: …" }`;
- `for_each` render/parse error → `Fail { "for_each expression error: …" }`;
- empty array → `Skip`;
- more than `MAX_FOR_EACH_ITEMS` (10 000) items → `Fail`;
- otherwise → `Expand`, with instances built as today (copied action columns,
  `when_condition: None`, `for_each_expr: None`, `loop_*` set; all `ready` when
  parallel, `[0]` `ready` and the rest `pending` when `sequential`).

Dependency satisfaction for a placeholder is the same predicate as R2.

**R5 — Sequential advance.** For a `running` placeholder whose flow step is `sequential`,
let `i` be the lowest `loop_index` whose instance is `pending`, if any, and require every
lower-index instance to be terminal:
- instance `i-1` is `completed` or `skipped`, or the flow step has `continue_on_failure`
  → `Promote { "{placeholder}[i]" }`;
- instance `i-1` is `failed` or `cancelled` without `continue_on_failure` → `Skip` for
  instance `i` and every later `pending` instance.

For `i == 0` nothing applies: `[0]` is created `ready`. A non-terminal lower-index
instance (including one reset to `ready` by step retry) means no change.

**R6 — Rollup.** For a `running` placeholder whose instances (rows with
`loop_source == placeholder`) are all terminal:
- any instance `failed` and no `continue_on_failure` →
  `Rollup { Failed("for_each loop failed: instances {:?} failed") }` with the failed
  `loop_index` list, formatted exactly as today;
- otherwise → `Rollup { Completed(array) }`, outputs ordered by `loop_index`, `null` for
  an instance with no output.

A placeholder with zero instance rows is never rolled up (an empty array is retired by
R4 before expansion, so this state cannot arise from the cascade itself).

**R7 — Job running.** `Expand` additionally moves the job from `pending` to `running`,
as `expand_for_each_steps` does today. No other rule touches the job row.

### 4.4 In-memory application and termination

Applying a `Change` to the snapshot:
- `Promote` / `Skip` / `Fail` set the row's status (and `error_message` for `Fail`).
- `Expand` sets the placeholder to `running` and appends synthetic `JobStepRow`s for the
  instances (status per R4; timestamps `now`; worker/output/error empty). Synthetic rows
  are never handed to `apply` as rows; `apply` inserts from the `NewJobStep`s.
- `Rollup` sets the placeholder's status and `output`/`error_message`.

Termination: every change moves one row to a strictly later status in
`pending < ready < running < {completed, failed, skipped}`, or appends instance rows for a
placeholder that is simultaneously moved out of `pending`. Both are bounded by the
snapshot size, so the pass count is at most `2 × |steps| + |placeholders|`. The old
`task.flow.len() * 2 + 10` bound becomes a `debug_assert!`, not a runtime warning.

Rule evaluation order inside a pass is R1, R2, R3, R4, R5, R6, then R7 folded into
`Expand`. A row changed earlier in the pass is not re-evaluated until the next pass.
The order matters only where today's order mattered (R1 before `when`), and the fixpoint
makes the rest order-independent.

### 4.5 Template context

`run` builds the context for R2 and R4 by calling
`job_creator::build_step_render_context(job, &snapshot, workspace_config)` at the start of
each pass. The snapshot includes synthetic instance rows, which the builder already
ignores (`loop_source.is_some()`), so expanded placeholders are visible to downstream
`when` exactly as they are after a DB round-trip today.

### 4.6 `apply`

Runs inside the caller's transaction, in change order, using only transaction-taking repo
primitives. Guards are the ones today's statements carry:

| Change | Statements | Guard |
|---|---|---|
| `Promote` (batched) | `UPDATE job_step SET status='ready', ready_at=NOW() … step_name = ANY($2)` | `status='pending'` |
| `Skip` (batched) | `UPDATE … SET status='skipped', completed_at=NOW() …` | `status='pending'` |
| `Fail` (per step) | `UPDATE … SET status='failed', error_message=$3, completed_at=NOW() …` | `status='pending'` |
| `Expand` | 1. placeholder `UPDATE … SET status='running', started_at=NOW()`; 2. multi-row `INSERT` of instances (existing `create_steps_tx`); 3. `UPDATE job SET status='running', started_at=NOW()` | 1. `status IN ('pending','ready')`; 3. `status='pending'` |
| `Rollup` | `UPDATE … SET status=$3, output/error_message, completed_at=NOW()` | `status='running'` |

For `Expand` the placeholder update runs **first**; if it matches zero rows (the
placeholder was cancelled between snapshot and apply) the instances are **not** inserted
and the change is recorded in `Applied` as guard-skipped. This ordering is what makes the
crash hole unrepresentable: instances exist only if the placeholder transition is in the
same transaction.

Zero-row matches on `Promote`/`Skip`/`Fail`/`Rollup` are not errors: under the job lock
the only concurrent writer that can move a `pending` or `running` placeholder row is
cancellation, and cancellation wins. They are counted and logged at `warn`.

### 4.7 `execute`

```
BEGIN
  SELECT … FROM job WHERE job_id = $1 FOR UPDATE      -- JobRepo::get_for_update_tx
  SELECT … FROM job_step WHERE job_id = $1 ORDER BY step_name   -- get_steps_for_job_tx
  changes = run(task, &job, &steps, workspace_config)  -- pure, in memory
  applied = apply(&mut tx, job_id, &changes)
COMMIT
info!(job_id, promoted, skipped, failed, expanded, rolled_up, guard_skipped)
```

The lock serialises concurrent cascades on one job; the second one re-reads after the
first commits and sees its writes, so no cascade work is lost. Tera rendering happens
while the transaction is open; it is in-memory and bounded by the template sizes. The
lock is the same row lock artifact uploads take (`artifacts.rs:165`); the two queue behind
each other per job.

### 4.8 Callers after the change

- `orchestrator::on_step_completed(pool, job_id, step_name, task, workspace_config)`:
  signature unchanged. Body becomes `cascade::execute(...)` then
  `settle_if_all_terminal(...)`. The three `tracing::info!` lines are replaced by
  `execute`'s summary line.
- `job_creator::create_job_for_task_inner`, `init` block: the loop at `:618-647` becomes
  one `cascade::execute(pool, job_id, task, Some(workspace_config))`. The
  compensation path (`:663-694`) is unchanged: an `Err` from `execute` still fails live
  steps and marks the job failed in one transaction.
- `job_recovery.rs:226` and `:601`: the `check_loop_completion` calls are removed. The
  preceding step-retry reset (`:186-222`) stays where it is; the invariant "retry reset
  before rollup" holds because the cascade runs after it, as `check_loop_completion` did.
- `job_creator::orchestrate_after_server_step_failure` is unchanged (it calls
  `on_step_completed`).

### 4.9 Preserved quirks (deliberately not changed)

- `when` is read from the row's `when_condition`, not the flow definition, where that is
  what today's code reads.
- `job` is inserted before completed-step outputs in the context (a step named `job`
  shadows it).
- `None` workspace config: `when` steps and placeholders stay `pending`.
- `MAX_FOR_EACH_ITEMS = 10_000` and all error-message texts, byte for byte.
- `Expand` moves the job to `running`; `Promote` does not.
- Rows present in the DB but absent from `task.flow` are ignored by R1–R3, as today.

### 4.10 Fixed by construction (Decision 2)

Each gets its own commit and regression test (§9):

1. **for_each crash hole** — instance insert and placeholder transition commit together
   (§4.6).
2. **Lost cascade work under concurrency** — the job-row lock serialises cascades (§4.7).
3. **Self-healing rollup** — R5/R6 are global rules re-evaluated on every cascade, so a
   rollup or sequential advance that a previous pass missed is picked up by the next.
   Today's keyed `check_loop_completion` would leave the placeholder `running` forever.

## 5. Data Model

No migration. No new columns. The `for_each` instance/placeholder relationship stays a
naming convention plus `loop_source`, and no trigger or foreign key is added.

## 6. Server

### 6.1 New

- `crates/stroem-server/src/cascade.rs` — `Change`, `RollupOutcome`, `Applied`, `run`,
  `apply`, `execute`, plus the moved helpers `parse_for_each_items`,
  `render_for_each_template`, `MAX_FOR_EACH_ITEMS`, and an internal snapshot type.
- `stroem-db` transaction primitives (thin, one statement each):
  `JobRepo::get_for_update_tx`, `JobRepo::mark_running_if_pending_tx`,
  `JobStepRepo::get_steps_for_job_tx`, `promote_steps_tx`, `skip_steps_tx`,
  `fail_pending_step_tx`, `mark_running_server_tx`, `rollup_placeholder_tx`.
  `create_steps_tx` already exists.

### 6.2 Deleted

- `JobStepRepo::promote_ready_steps`, `JobStepRepo::skip_unreachable_steps` and their
  tests (`stroem-db/tests/integration_test.rs::test_promote_ready_steps`,
  `job_step_status_tests.rs` × 3).
- `job_creator::expand_for_each_steps`, `job_creator::check_loop_completion`.
- `stroem_common::template::evaluate_condition` stays; the DB crate simply stops
  calling it. If `stroem-db`'s dependency on the `template` module becomes unused,
  drop the import.

### 6.3 Unchanged

`settle_if_all_terminal`, `handle_task_steps`, `handle_approval_steps`,
`orchestrate_after_step`, `propagate_to_parent`, retry, hooks, cancellation, recovery.

## 7. Documentation

- `CONTEXT.md` (new): step cascade, placeholder, instance, retirement, rollup, change,
  settlement (pointer to the later design). Links back to CLAUDE.md.
- `CLAUDE.md`: link to `CONTEXT.md` in the overview; new `### Step Cascade` section
  (interface, the lock, callers, the rule that new context variables keep `job` first);
  rewrite the "Placeholder resolution lives inside the orchestrator cascade" paragraph
  under For-Each Loops and the failed-dep note to point at R4/R5/R6; remove the
  "Do NOT call `expand_for_each_steps` after `on_step_completed`" warning (the function
  no longer exists).
- `docs/internal/TODO.md`: close the in-file TODOs from `job_step.rs:678-690` if tracked;
  add follow-ups: remove the `None` workspace-config mode (settlement branch), move
  `build_step_render_context` (candidate 7).
- No user-facing docs change: semantics are unchanged.

## 8. Known Limitations

- `run` depends on `job_creator::build_step_render_context`, and `job_creator` calls
  `cascade::execute`. Module-level mutual reference within the crate; resolved when the
  builder moves (candidate 7).
- The `Option<&WorkspaceConfig>` mode is a test-only concept carried through one more
  branch.
- The job-row lock adds a wait behind an in-flight artifact upload for the same job.

## 9. Tests

### 9.1 Unit, at the cascade interface (`cascade.rs`, no DB)

A row builder (`step(name).status(..).deps(..).when(..).for_each(..).loop_of(..)`) and a
flow builder. Cases, each asserting on the returned `Vec<Change>`:

- linear promotion; diamond join waits for both branches
- failed dep skips dependents; `continue_on_failure` promotes; mixed skipped+failed
  without cof blocks; cancelled dep with cof promotes
- all-deps-skipped cascade-skip, including overriding a truthy `when`
- `when` true / false / error; sibling truthy and falsy `when` evaluated in one pass;
  `when` with `None` config stays pending
- multi-level cascade skip reaches the fixpoint in one `run`
- placeholder: failed dep skips it; cof keeps it; all-deps-skipped skips it; `when` false
  skips; `when` error fails; expression error fails; empty array skips; >10 000 fails;
  literal JSON array vs Tera string; parallel vs sequential instance statuses
- sequential advance: promote next; skip remaining after failure; no-op while `[i-1]` is
  non-terminal (retry-reset case)
- rollup: completed array in `loop_index` order with `null` gaps; failed list text; no
  rollup while any instance non-terminal; no rollup for a non-running placeholder
- downstream `when` sees an expanded placeholder's rollup output within the same `run`
- termination: a large DAG reaches the fixpoint and the `debug_assert` bound holds
- idempotency: `run` on a snapshot at fixpoint returns an empty vec
- rows absent from the flow are ignored

Target: every scenario in `orchestrator_test.rs` reproduced as a unit test; the count is
checked against the list in the plan.

### 9.2 Integration (container), new

- `apply` is transactional: apply an `Expand`, drop the transaction without commit,
  assert no instance rows and placeholder still `pending`.
- `Expand` guard-skip: placeholder cancelled after snapshot → no instances inserted,
  `Applied.guard_skipped` names it.
- Concurrency regression: two `execute`s for the same job run concurrently after two
  sibling steps complete; the join step is promoted exactly once and the final state
  equals a serial run.
- Self-healing rollup regression: placeholder `running`, all instances terminal, no
  prior rollup → one `execute` rolls it up.
- `on_step_completed` and creation-time cascade produce identical results for the same
  DAG (both callers go through `execute`).

### 9.3 Oracle

`crates/stroem-server/tests/{orchestrator_test,integration_test,restart_integration_test,
rerun_integration_test,propagate_to_parent_test,metrics_test}.rs` and
`crates/stroem-db/tests/*` run unchanged and green after every commit, except the four
DB-level tests deleted with their functions (§6.2) and the final pruning commit.

### 9.4 E2E

`tests/e2e.sh` unchanged; it already covers for_each, `when`, and task-action flows.

## 10. Rollout

- Pure server change, no migration, no config. Ships as a patch release.
- Commit sequence (each green): (1) `cascade::run` + unit tests, unused; (2) transaction
  primitives in stroem-db; (3) `apply` + `execute` + transactional tests; (4) switch
  `on_step_completed`; (5) switch creation-time loop; (6) absorb `check_loop_completion`
  and delete it, with the self-healing test; (7) delete repo functions and their tests;
  (8) crash-hole and concurrency regression tests, each as its own commit if not already
  covered by (3); (9) docs: `CONTEXT.md`, CLAUDE.md, TODO.md; (10) prune redundant
  container tests.

## 11. Resolved Questions

| Q | Decision |
|---|---|
| Q1 slicing | cascade first, settlement on its own branch |
| Q2 behaviour | preserving; by-construction fixes in separate commits |
| Q3 home | `stroem-server/src/cascade.rs` |
| Q4 output | `Vec<Change>`, repo primitives apply |
| Q5 tests | container suite is the oracle; unit tests at the interface; prune last |
| Q6 inputs | task, job row, steps, `Option<&WorkspaceConfig>`; context built inside |
| Q7 fixpoint | in memory, inside `run` |
| Q8 atomicity | one transaction, job row `FOR UPDATE` |
| Q9 rollup | absorbed as global rules; `check_loop_completion` deleted |
| Q10 repo fns | deleted with their four tests |
| Q11 `None` mode | preserved on this branch |
| Q12 vocabulary | `CONTEXT.md`, cross-referenced from CLAUDE.md |
| Q13 lock sharing | accept sharing the job row lock with artifact uploads |

## 12. Review Log

- 2026-09-08 — draft written after the design walk; Codex review pending.
