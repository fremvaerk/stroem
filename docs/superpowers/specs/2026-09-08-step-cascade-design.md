# Step Cascade — Design

**Status:** Draft, revision 3 (2026-09-08, after second Codex review)
**Date:** 2026-09-08
**Origin:** architecture review 2026-09-07/08, candidate "Make the step cascade a pure
module"; first internal seam of the later "Job settlement" module (separate branch, not
covered here).

## 1. Problem

Moving a job's steps after something changes — promote steps whose dependencies are
satisfied, cascade-skip steps whose dependencies are skipped, evaluate `when`, expand
`for_each` placeholders, retire placeholders, roll a loop up — is one fixpoint. Today it
is written twice, and half of it lives in the DB crate:

- `orchestrator.rs:46-113` (`on_step_completed`, runs after every step completion)
- `job_creator.rs:618-641` (post-commit `init` block, runs once at creation; task and
  approval dispatch follow from `:643`)
- `stroem-db/src/repos/job_step.rs:672-826` `promote_ready_steps` evaluates Tera `when`
  conditions inside the repository (`:757`); `:855-916` `skip_unreachable_steps`
- `job_creator.rs:945-1152` `expand_for_each_steps`
- `job_creator.rs:1216-1353` `check_loop_completion`, a pre-step keyed on the one step
  that just completed, called from `job_recovery.rs:226` (worker completion) and `:601`
  (child-job propagation)

The two loops are the same fixpoint with different dressing: the orchestrator copy
tolerates `workspace_config == None`, adds `.context(...)` and per-phase logging; the
creation copy propagates errors into the creation compensation path. Consequences, all
observed in production or flagged in code:

- The placeholder lifecycle is split over three functions in two crates. The two
  stuck-job incidents recorded in CLAUDE.md (2026-09-02: `expand_for_each_steps` called
  after `on_step_completed` as a separate pass; 2026-09-07: a placeholder retired by no
  one) came from that split.
- Every fixpoint iteration performs four full `job_step` fetches and one `job` fetch
  (plus one `job` fetch before the loop), each a separate autocommitted statement.
- `expand_for_each_steps` inserts instance rows, then marks the placeholder running, in
  two statements with no transaction (`job_creator.rs:1134-1138`). A crash between them
  leaves the instances committed and the placeholder `pending`; the idempotency guard at
  `:973-977` then sees `"{step}[0]"` and `continue`s forever. The placeholder is stuck.
- Read-classify-write with no lock: two orchestrators on the same job classify against
  stale snapshots. The `AND status = 'pending'` guards prevent corrupt transitions but not
  lost cascade work (`job_step.rs:684-690`, in-file TODO).
- A step is observable as `failed` between `mark_failed` and the retry reset that
  follows in the same handler (`job_recovery.rs:186-209`). Any concurrent cascade in
  that window skips its dependents or fails its loop as if the failure were final.
- Loop rollup is keyed on the completing instance and its writes are unguarded
  (`job_step.rs:465-505`): a later instance completion overwrites a cancelled or
  timed-out placeholder.
- Nothing in this path is testable without a Postgres container. `orchestrator.rs` has
  zero in-module tests; the 34 tests in `orchestrator_test.rs` all spin a container.

## 2. Decisions

Settled in the design walk on 2026-09-08 (Q1–Q13), after the first Codex review
(Q14–Q23) and after the second (Q24–Q27).

1. **Scope:** cascade only on this branch. The settlement module (absorbing
   `finalize_created_job` and `reconcile_settled_children`) is a separate branch with its
   own design.
2. **Behaviour policy:** preserving, except defects the new shape fixes by construction
   with no extra code (§4.10 lists all seven). The fixes go live in the one activation
   commit (§10); each has its own regression-test commit.
3. **Home:** `crates/stroem-server/src/cascade.rs`. Orchestration policy, not persistence.
4. **Shape:** a `run` returning a `Plan` of `Change`s (closed enum) plus the snapshot it
   assumed; one `apply` composes transaction-taking repo primitives. No `StepStore` trait.
5. **Tests:** the existing container suite stays as the oracle until the last commit
   (one test migrated with the function it calls, §4.8); unit tests are added at the
   cascade interface; redundant container tests are pruned in one final commit.
6. **`run` inputs:** `task`, job row, step snapshot, `Option<&WorkspaceConfig>`. The
   module builds the template context itself from its own snapshot via the existing
   `job_creator::build_step_render_context`.
7. **Fixpoint in memory:** `run` iterates to the fixpoint on its own snapshot with the
   phase boundaries today's loop has (§4.4), synthesising instance rows after an expand,
   and returns the full plan. One read before, one verify-and-write after.
8. **Rendering outside the lock; whole-snapshot compare-and-apply inside** (Q15, Q24):
   the snapshot is read and `run` executes with no transaction open; `execute` then
   begins a transaction, takes the job's advisory lock, re-reads the job status and every
   step's `(status, retry_attempt)`, and applies only if that vector is identical to the
   one the plan was computed from. Any difference rolls back and re-runs from a fresh
   snapshot, at most three times.
9. **Loop rollup and sequential advance join the cascade** as global rules over the
   snapshot (§4.3 R5/R6) with an explicit failure-precedence policy (Q26);
   `check_loop_completion` is deleted.
10. **Repo functions deleted:** `promote_ready_steps`, `skip_unreachable_steps` and their
    four DB-level tests, whose timestamp assertions move to `apply` tests (§9.2).
11. **`None` workspace-config mode preserved** exactly: `when`-conditioned steps stay
    `pending` (R1/R3 still apply to them), placeholders are never expanded or retired.
    Removed in the settlement branch.
12. **Vocabulary:** new `CONTEXT.md` glossary at the repo root, cross-referenced from
    CLAUDE.md; CLAUDE.md gains a `### Step Cascade` section.
13. **Lock:** a transaction-scoped Postgres advisory lock in the two-integer form,
    `(CASCADE_LOCK_CLASS, hashtext(job_id))` (§4.7.1), not the job row (Q23). The
    cascade never queues behind an artifact upload holding the row lock across a blob
    write, nor behind cancellation, worker start or log-upload row updates. Creation
    compensation takes the same lock first (§4.7).
14. **Phase model** (Q14): a pass runs rollup/advance, then promote and cascade-skip with
    a context built before them, then skip-unreachable, then retirement/expansion with a
    context rebuilt after those changes. Changes apply to the snapshot phase by phase.
    No order-independence claim.
15. **Effective terminality with retry ownership** (Q16, Q25): wherever a rule reads a
    status — dependency satisfaction for R2/R3/R4, instance terminality for R5/R6 — a
    `failed` step with `retry_attempt < max_retries` whose failure path runs the retry
    check (`action_type != "task"`) is **not** terminal. Fix by construction.
16. **Stronger guards on placeholder writes** (Q17): retirement requires `pending`,
    rollup requires `running`. Column behaviour preserved (failure keeps `output`,
    completion keeps `error_message`). Fix by construction.
17. **Sequential failure precedence** (Q18, Q26): for a running sequential placeholder,
    any effectively-failed or cancelled instance without `continue_on_failure` skips
    every pending instance; only otherwise is `[i+1]` promoted. Not claimed equivalent to
    today's keyed rule; the divergences are listed in §4.3 and tested.
18. **Error propagation** (Q20): a rollup error is a cascade error and aborts before
    settlement, as promote/skip/expand errors already do. `check_loop_completion`'s
    log-and-continue is not preserved.
19. **Pre-existing partial expansions** (Q22): a `pending` placeholder whose `[0]`
    already exists is adopted (moved to `running`, nothing inserted). Fix by construction.
20. **Convergence contract:** the caller no longer loops. `run` returns the fixpoint;
    `execute` re-runs only on a verification mismatch.
21. **Commit sequence** (Q27): everything new lands unused first; one activation commit
    switches both callers and deletes the four old functions together; regression-test
    commits follow; adoption is a separate later commit (§10).

## 3. Non-Goals

- Changing any user-visible semantics of `when`, `for_each`, `sequential`,
  `continue_on_failure`, or step status transitions, beyond the seven fixes in §4.10.
- Settlement (`settle_if_all_terminal`), terminal handling, propagation, hooks, the
  step-retry reset, task retry. They run before or after the cascade exactly as today.
- Fixing the pre-existing defect that a `type: task` step's retry config is never
  honoured (child propagation and dispatch failure bypass the retry check,
  `job_recovery.rs:575-617`, `job_creator.rs:743`). Logged to TODO; the cascade only
  stops waiting for a retry those paths never schedule (Decision 15).
- Fixing recovery's unguarded `mark_failed` on timed-out steps (`job_step.rs:502`),
  which can overwrite a `completed` row. The cascade tolerates it through whole-snapshot
  verification; the guard belongs to the transition candidate.
- Moving `build_step_render_context` or unifying the render-context builders
  (review candidate 7). The cascade calls the existing builder.
- Removing the `Option<&WorkspaceConfig>` parameter from `on_step_completed`
  (about 80 test call sites pass `None`).
- Changing artifact upload's own job-row lock.
- Making template rendering pure or bounded. `render_template` registers the `vals`
  filter, which spawns a subprocess with no timeout (`template.rs:30-95`). The design
  keeps that I/O outside any transaction (§4.7); it does not change the filter.
- Any schema migration. No new columns, constraints, or triggers.

## 4. Semantics

### 4.1 Vocabulary

See `CONTEXT.md`. In short: a **step cascade** is the fixpoint that moves a job's
non-running steps after a change; a **placeholder** is the `for_each` row that stands in
for its **instances** (`step[0]`, `step[1]`, …); **retirement** is a placeholder leaving
`pending` without expanding (skipped or failed); **adoption** is a `pending` placeholder
whose instances already exist being moved to `running`; **rollup** is a running
placeholder becoming `completed`/`failed` once every instance is terminal; a **plan** is
the ordered list of changes one `run` produced together with the snapshot vector it
assumed; a step is **retry-pending** when it is `failed` but its own handler will reset
it (Decision 15).

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
    /// Placeholder pending → running; insert instance rows; job pending → running.
    Expand { placeholder: String, instances: Vec<NewJobStep> },
    /// Placeholder pending → running, instances already exist (§4.3 R0). No insert.
    Adopt { placeholder: String },
    /// running placeholder → completed(output) | failed(error)
    Rollup { placeholder: String, outcome: RollupOutcome },
}

pub enum RollupOutcome {
    Completed(serde_json::Value),
    Failed(String),
}

/// The pre-run state `run` computed from. One entry per step row, in `step_name`
/// order, plus the job status. `execute` applies a plan only if the database still
/// holds exactly this vector.
pub struct Snapshot {
    pub job_status: JobStatus,
    pub steps: Vec<(String, StepStatus, i32 /* retry_attempt */)>,
}

/// What one `run` decided.
pub struct Plan {
    pub changes: Vec<Change>,
    pub assumed: Snapshot,
}

/// Deterministic given its inputs and the templates' filters. Renders `when` and
/// `for_each` templates (which may run the `vals` filter). Never touches the database.
/// Iterates to the fixpoint on an in-memory copy of `steps` (§4.4).
/// `workspace_config == None` reproduces today's "no template context" mode.
pub fn run(
    task: &TaskDef,
    job: &JobRow,
    steps: &[JobStepRow],
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<Plan>;

/// Applies `plan.changes` inside `tx`, in order. Every step-row statement carries the
/// guard from §4.6; after a successful verification a zero-row match on a step row is a
/// bug and returns an error (the caller rolls back). The job-row update of R7 is the one
/// statement allowed to match zero rows.
pub async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    plan: &Plan,
) -> Result<Applied>;

/// The one entry point both callers use (§4.7). Returns the applied plan for callers
/// and tests; an empty plan means the job was already at its fixpoint.
pub async fn execute(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<Plan>;
```

`Applied` is a small summary (counts per variant) used for the log line and for tests.
`run` returns `Result` only because `build_step_render_context` and the template
functions do; no rule error is propagated (rule errors become `Fail` changes, §4.3).
`assumed` is captured once, before the first pass; sequential changes to one row inside
a plan (an `Adopt` followed by a `Rollup` of the same placeholder) are ordered by the
change list and guarded by §4.6, not by `assumed`.

### 4.3 Rules evaluated by `run`

Rule inputs come from the same sources they do today and the implementation must not
change which source a rule reads:

- row: `status`, `when_condition`, `for_each_expr`, `loop_source`, `loop_index`,
  `output`, `retry_attempt`, `max_retries`, `action_type`;
- flow (`task.flow[step]`): `depends_on`, `continue_on_failure`, `sequential`. A row
  whose name is absent from the flow is ignored by R1–R4; for R5/R6 a placeholder absent
  from the flow behaves as `sequential = false`, `continue_on_failure = false` (today's
  `unwrap_or_default`, `job_creator.rs:1236`).

**Effective status** (Decision 15). A row is **retry-pending** when
`status = failed`, `max_retries` is `Some(m)` with `retry_attempt < m`, and
`action_type != "task"`. Every rule below that tests for `failed`, or for terminality,
treats a retry-pending row as **not failed and not terminal** (as if it were `running`).
The ownership condition mirrors which failure paths run the retry check: worker
completion, recovery sweeps and approval reject go through `orchestrate_after_step`
(`job_recovery.rs:186`); `type: task` failures go through `propagate_to_parent` and
`fail_task_step`, which never reset the row.

**Dependency satisfaction** (shared by R2 and R4): every dependency is `completed` or
`skipped`, or `failed` (not retry-pending) / `cancelled` when the step has
`continue_on_failure`.

**R0 — Adopt.** A `pending` placeholder present in the flow for which a row named
`"{placeholder}[0]"` exists → `Adopt`. Evaluated before R4 so a partially expanded
placeholder is never re-expanded. (Fix; today the guard at `job_creator.rs:973-977`
leaves it `pending` forever.)

**R1 — Cascade-skip (all deps skipped).** A `pending` step present in the flow, not a
placeholder, with a non-empty dependency list, every dependency `skipped`, and no
`continue_on_failure` → `Skip`. Evaluated before `when` (today's
`test_truthy_when_overridden_by_all_deps_skipped_cascade`). Applies with or without a
workspace config.

**R2 — Promote.** A `pending` step present in the flow, not a placeholder, whose
dependencies are satisfied:
- no `when` → `Promote`;
- `when` and no workspace config → no change (stays `pending`);
- `when` renders truthy → `Promote`; falsy → `Skip`; render error →
  `Fail { error: "when condition error: {:#}" }`.

Truthiness is `stroem_common::template::evaluate_condition` unchanged (empty, `false`,
`0`, `null`, `none` after trim, case-insensitive → false).

**R3 — Skip unreachable.** A `pending` step present in the flow, not a placeholder,
without `continue_on_failure`, with at least one dependency `failed` (not retry-pending)
or `cancelled` → `Skip`. A `skipped` dependency does not trigger this rule. Applies with
or without a workspace config.

**R4 — Placeholder retirement / expansion.** A `pending` placeholder (row
`for_each_expr.is_some()`) present in the flow, not adopted by R0, evaluated only when a
workspace config is present (with `None`, placeholders stay `pending` whatever their
dependencies):
- dependencies not satisfied, and at least one `failed` (not retry-pending) /
  `cancelled` without `continue_on_failure` → `Skip`;
- dependencies not satisfied otherwise → no change;
- non-empty dependency list, all `skipped`, no `continue_on_failure` → `Skip`;
- `when` falsy → `Skip`; `when` error → `Fail { "when condition error: …" }`;
- `for_each` render/parse error → `Fail { "for_each expression error: …" }`;
- empty array → `Skip`;
- more than `MAX_FOR_EACH_ITEMS` (10 000) items →
  `Fail { "for_each produced {} items (max {})" }`;
- otherwise → `Expand`, with instances built as today (copied action columns and retry
  columns, `when_condition: None`, `for_each_expr: None`, `loop_*` set; all `ready` when
  parallel, `[0]` `ready` and the rest `pending` when `sequential`).

The `when` and `for_each` templates are rendered against the context built for this
phase (§4.4), as today's two `build_step_render_context` calls at
`job_creator.rs:1040`/`:1055` both use the post-promote snapshot.

**Instance terminality** (shared by R5 and R6): an instance is terminal when its status
is `completed`, `skipped`, `cancelled`, or `failed` and not retry-pending.

**R5 — Sequential advance.** For a `running` placeholder present in the flow with
`sequential = true`, with instance rows sorted by `loop_index`:
1. if any instance is `failed` (not retry-pending) or `cancelled`, and the flow step has
   no `continue_on_failure` → `Skip` for every `pending` instance; nothing else for this
   placeholder;
2. otherwise, for each terminal instance `i` whose `[i+1]` exists and is `pending` →
   `Promote { "{placeholder}[i+1]" }`.

Divergences from today's keyed `check_loop_completion` (`job_creator.rs:1248-1296`),
deliberate and tested (§9.1):
- R5 runs on every cascade of the job, not only when an instance of this loop completes
  (self-healing, fix 3).
- In the state `[failed, completed, pending]` (reachable via recovery's unguarded
  timeout write landing on `[0]` after `[1]` completed) today promotes `[2]` on `[1]`'s
  completion and skips it on `[0]`'s later re-orchestration; R5 skips it immediately.
  Same eventual state, no intermediate promotion.
- A retry-pending instance is neither terminal nor failed; today the keyed check runs
  only after the reset, so it never sees this state at all. After the reset the row is
  `ready`, and both agree.
- R5 requires the placeholder to be `running` (fix 5).

**R6 — Rollup.** For a `running` placeholder that has at least one instance row and
whose instances are all terminal:
- any instance `failed` (not retry-pending) and no `continue_on_failure` →
  `Rollup { Failed("for_each loop failed: instances {:?} failed") }` with the failed
  `loop_index` list in ascending order, formatted exactly as today;
- otherwise → `Rollup { Completed(array) }`: one element per existing instance row,
  ordered by `loop_index`, the instance's `output` or `null` when it has none.

`cancelled` instances count as terminal and do not fail the placeholder (only `failed`
does), as today. A placeholder that is not `running` (cancelled, timed out and failed by
recovery, already rolled up) yields no change (fix 5). A running placeholder with zero
instance rows yields no change.

**R7 — Job running.** `Expand` and `Adopt` additionally move the job from `pending` to
`running`, as `expand_for_each_steps` does today. No other rule touches the job row.

### 4.4 Phase model, in-memory application, termination

One **pass** is:

```
P0  R5, R6 over the snapshot
    apply P0 changes to the snapshot
ctx_A = build_step_render_context(job, snapshot)   (today: the loop's first fetch)
P1  R1, R2 using ctx_A                             (today: promote_ready_steps)
    apply P1 changes
P2  R3                                             (today: skip_unreachable_steps)
    apply P2 changes
ctx_B = build_step_render_context(job, snapshot)   (today: expand's own fetch)
P3  R0, R4 using ctx_B                             (today: expand_for_each_steps)
    apply P3 changes
```

`run` repeats passes until a pass produces no change, then returns the concatenated
changes in production order together with `assumed`. Within a phase every eligible row
is evaluated against the snapshot as it stood at the start of that phase; a row changed
in an earlier phase of the same pass is visible to later phases. P1–P3 reproduce today's
commit points: promote's changes are visible to skip-unreachable; both are visible to
expansion's context; nothing inside a pass is visible to `when` evaluation in P1 until
the next pass. Codex's counterexample (root `a` with `when: "false"`, independent root
placeholder `p` whose `when` tests `a is defined`) therefore expands `p` in pass 1, as
today.

P0 is an extension of today, not a reproduction: today's keyed `check_loop_completion`
runs once per completed instance before the loop, and creation's loop has no rollup at
all. Running P0 at the start of every pass, for every caller, is fix 3 (self-healing).

Ordering caveat, stated rather than hidden: outcomes depend on this phase order wherever
a template references a step it does not declare as a dependency (validation permits
that, `validation.rs:181`). The order is part of the interface and is pinned by the
unit tests in §9.1.

Applying a `Change` to the snapshot:
- `Promote` / `Skip` / `Fail` set the row's status (and `error_message` for `Fail`).
- `Expand` sets the placeholder to `running` and appends synthetic `JobStepRow`s for the
  instances (status per R4; `loop_source` set; timestamps `now`; worker/output/error
  empty). `Adopt` sets the placeholder to `running`.
- `Rollup` sets the placeholder's status and `output`/`error_message`.

Synthetic rows never reach `apply` as rows; `apply` inserts from the `NewJobStep`s.
`build_step_render_context` ignores every row with `loop_source.is_some()`
(`job_creator.rs:1619`), so synthetic rows do not alter the context.

**Termination.** Let `U` be the universe of rows that can exist during one `run`: the
initial rows plus, for each initial `pending` placeholder, up to `MAX_FOR_EACH_ITEMS`
instance rows; `|U| ≤ |initial| + 10 000 × |pending placeholders|`. Define
`rank(pending) = 0`, `rank(ready) = 1`, `rank(claimed) = 2`,
`rank(running) = rank(suspended) = 3`, `rank(terminal) = 4`, and
`M = Σ_{row ∈ snapshot} rank(status)`. The cascade only ever produces these
transitions: `Promote` 0→1, `Skip`/`Fail` 0→4, `Expand`/`Adopt` 0→3 on the placeholder
(plus appended rows of rank ≥ 0), `Rollup` 3→4. Each strictly increases `M` by at least
1 and none decreases any row's rank, so a non-empty pass increases `M` by ≥ 1, and
`M ≤ 4|U|`. Hence at most `4|U|` non-empty passes. A
`debug_assert!(passes <= 4 * universe_size + 1)` guards the loop; a release build breaks
out at that bound with a `warn!`, matching today's defensive behaviour. This bounds rule
evaluations, not wall-clock time: rendering may run `vals` (§3), which is why nothing
here executes under a lock.

### 4.5 Template context

Built twice per pass by `job_creator::build_step_render_context(job, &snapshot,
workspace_config)`, at the points marked in §4.4. `job` metadata is inserted before
step outputs (a step named `job` shadows it), unchanged. Only `completed`, `skipped`,
`failed` and `suspended` rows enter the context (`job_creator.rs:1629-1662`); a row
promoted to `ready` in P1 is therefore invisible to templates in every phase, as today.

### 4.6 `apply`

Runs inside the caller's transaction, in change order, using only transaction-taking
repo primitives. Guards, with today's equivalent noted:

| Change | Statements | Guard | vs today |
|---|---|---|---|
| `Promote` (batched) | `UPDATE job_step SET status='ready', ready_at=NOW() … step_name = ANY($2)` | `status='pending'` | same |
| `Skip` (batched) | `UPDATE … SET status='skipped', completed_at=NOW() …` | `status='pending'` | same |
| `Fail` (per step) | `UPDATE … SET status='failed', error_message=$3, completed_at=NOW() …` | `status='pending'` | same for ordinary steps; **stronger** for placeholders (today unguarded `mark_failed`, `job_creator.rs:1046`) |
| `Expand` | 1. placeholder `UPDATE … SET status='running', started_at=NOW()`; 2. multi-row `INSERT` of instances (existing `create_steps_tx`); 3. `UPDATE job SET status='running', started_at=NOW()` | 1. `status='pending'`; 3. `status='pending'` | 1. today `IN ('pending','ready')`; a placeholder is never `ready`, so equivalent |
| `Adopt` | placeholder `UPDATE` as Expand step 1; job `UPDATE` as step 3 | as Expand | new (fix 6) |
| `Rollup` | `UPDATE … SET status=$3, output=$4 / error_message=$4, completed_at=NOW()` | `status='running'` | **stronger** (today unguarded `mark_completed`/`mark_failed`) |

Column preservation as today: `Fail` and `Rollup::Failed` do not touch `output`;
`Rollup::Completed` does not touch `error_message`.

Zero-row policy. After §4.7's verification every step-row guard is known to hold, so a
zero-row match on a step-row statement is a programming error: `apply` returns `Err` and
the caller rolls back. The job-row update (Expand/Adopt step 3) is exempt: it is
conditional on `job.status = 'pending'` and legitimately matches zero rows once the job
is already `running` (today's `mark_running_if_pending_server`, `job.rs:434`).

For `Expand` the placeholder update runs first, then the insert, so instances exist only
if the placeholder transition is in the same committed transaction (fix 1).

### 4.7 `execute`: render outside, verify and apply inside

```
attempt = 0
loop:
  job   = JobRepo::get(pool, job_id)                       -- no transaction
  steps = JobStepRepo::get_steps_for_job(pool, job_id)
  plan  = run(task, &job, &steps, workspace_config)       -- may render, may run vals
  if plan.changes.is_empty(): return plan

  BEGIN
    SELECT pg_advisory_xact_lock(CASCADE_LOCK_CLASS, hashtext($1))   -- §4.7.1
    current = Snapshot { job_status: SELECT status FROM job WHERE job_id=$1,
                         steps: SELECT step_name, status, retry_attempt
                                FROM job_step WHERE job_id=$1 ORDER BY step_name }
    if current != plan.assumed:
        ROLLBACK; attempt += 1
        if attempt == 3: return Err("cascade verification failed 3 times")
        continue
    applied = apply(&mut tx, job_id, &plan)
  COMMIT
  info!(job_id, attempt, promoted, skipped, failed, expanded, adopted, rolled_up)
  return plan
```

**Verification contract.** A plan is applied only if the job's status and every step
row's `(status, retry_attempt)` are exactly what `run` computed from. There is no
argument about which rows matter: any status change to any row of the job between
snapshot and apply — cancellation (`JobRepo::cancel` on the job row,
`cancel_pending_steps` on step rows), a recovery timeout write, a worker claim or
completion, an approval, a retry reset (which also bumps `retry_attempt`), another
cascade's commit — makes the vector differ and forces a re-run on a fresh snapshot.
Template-visible data (`output`, `error_message`) changes only together with a status
change, so it is covered. The one write that verification cannot see is a retry reset
that has not happened yet: a row that is `failed` at snapshot and will be reset by its
own handler after apply. Decision 15 handles that by never letting a rule act on a
retry-pending row.

**What the lock serialises.** Two `execute`s for one job run their verify-and-apply
phases one at a time; the second's verification sees the first's commits and re-runs.
Creation compensation (`job_creator.rs:663-694`) takes the same advisory lock as its
first statement, so a cascade can never interleave with a compensation on the same job;
its step-then-job write order is otherwise unchanged.

**Lock inventory (unchanged elsewhere).** Worker claim locks `job_step` rows with
`FOR UPDATE SKIP LOCKED` and commits before any orchestration; completion, approval, and
recovery write through the pool and commit before calling the cascade; artifact upload
holds a job-row lock across its blob write, which the advisory lock never waits on;
cancellation, worker start and log upload are single-row `UPDATE`s on `job`. `apply`'s
own job `UPDATE` (R7) is one statement and briefly takes the row lock; it can wait behind
an artifact upload for that duration only.

**Commit before anything else.** `execute` commits before returning. Settlement, task
and approval dispatch, child-job creation and propagation to a parent all happen after
it, outside any transaction, as today. This is a stated invariant: no caller may wrap
`execute` in a larger transaction or hold the advisory lock across dispatch, because
child creation references the parent row (`020_fk_on_delete_set_null.sql:8`) and
propagation runs the parent's cascade.

#### 4.7.1 Advisory lock key

`pg_advisory_xact_lock(CASCADE_LOCK_CLASS, hashtext(job_id::text))`, the two-`int4`
form. Postgres keys the two-integer form separately from the single-`bigint` form
(`objsubid` 2 vs 1), so it cannot collide with leader election's
`pg_try_advisory_lock(0x5354524D4C445201)` (`leader.rs:26-35`) whatever the hash
yields. `CASCADE_LOCK_CLASS` is a `pub const i32` in stroem-db (`0x5354_5243`, "STRC");
any future two-integer advisory lock must use a different class. Precedent for the
transaction-scoped form: creation compensation already runs a multi-statement
transaction on a pooled connection (`job_creator.rs:669-680`); `pg_advisory_xact_lock`
releases at transaction end, commit or rollback.

### 4.8 Callers after the change

- `orchestrator::on_step_completed(pool, job_id, step_name, task, workspace_config)`:
  signature unchanged. Body becomes `cascade::execute(...)` then
  `settle_if_all_terminal(...)`. The three `tracing::info!` lines are replaced by
  `execute`'s summary line. Note: with `None`, this function can now roll up or advance a
  loop (R5/R6 need no context) where today it cannot; no production caller passes `None`.
- `job_creator::create_job_for_task_inner`, `init` block: the loop at `:618-641`
  becomes one `cascade::execute(pool, job_id, task, Some(workspace_config))`. The
  compensation path (`:663-694`) additionally takes the cascade advisory lock as its
  first statement; its transaction and write order are otherwise unchanged, so the
  atomic-compensation invariant in CLAUDE.md (Task Actions) holds.
- `job_recovery.rs:226` and `:601`: the `check_loop_completion` calls are removed. The
  step-retry reset (`:186-209`) is unchanged: when it resets the step it returns early
  and **no cascade runs**, as today. `propagate_to_parent` (`:563-624`) has no retry
  check before its cascade, as today (see §3, task-step retry). A rollup error now
  propagates out of `on_step_completed` (Decision 18) where `check_loop_completion`'s
  was logged and ignored.
- `job_creator::orchestrate_after_server_step_failure` is unchanged (it calls
  `on_step_completed`).
- `crates/stroem-server/tests/orchestrator_test.rs:1017`
  `test_convergence_without_continue_on_failure` calls `promote_ready_steps` directly
  with a hand-built context. It is rewritten in the activation commit: the `input` it
  hand-builds is persisted on the job row, the call becomes `cascade::execute`, and the
  assertions on the returned names become assertions on the returned `Plan`. The
  scenario itself (`b` promoted, `c` skipped under `input.use_fast = true`) is also
  added as a `run` unit test.

### 4.9 Preserved quirks (deliberately not changed)

- `when` is read from the row's `when_condition` for both ordinary steps and
  placeholders; `depends_on`, `continue_on_failure`, `sequential` come from the flow.
- `job` is inserted before completed-step outputs in the context.
- `None` workspace config: `when`-conditioned steps and placeholders stay `pending`;
  R1 and R3 still apply.
- `MAX_FOR_EACH_ITEMS = 10_000` and all error-message texts, byte for byte.
- `Expand`/`Adopt` move the job to `running`; `Promote` does not.
- Rows present in the DB but absent from `task.flow` are ignored by R1–R4.
- A placeholder absent from the flow rolls up as non-sequential without
  `continue_on_failure`.
- Only `failed` instances fail a loop; `cancelled` instances do not.
- Rollup output has one element per existing instance row, `null` where the row has no
  output. Missing indices are not padded.
- The step-retry reset's early return: no cascade after a successful reset.
- A `type: task` step's retry config is not honoured on failure (pre-existing, §3).

### 4.10 Fixed by construction (Decision 2)

All go live in the activation commit (§10); each has its own regression-test commit
(§9.2):

1. **for_each crash hole** — instance insert and placeholder transition commit together
   (§4.6).
2. **Lost cascade work under concurrency** — advisory lock plus whole-snapshot
   compare-and-apply (§4.7).
3. **Self-healing rollup and advance** — R5/R6 run on every cascade. Covers the case
   where today's sequential failure path skips rows and then checks terminality against
   a stale snapshot (`job_creator.rs:1282`, `:1307`), leaving the rollup for a later
   keyed call that may never come.
4. **Retry-pending rows are not acted on** — a concurrent cascade in the window between
   `mark_failed` and the retry reset no longer skips the step's dependents (R2/R3/R4)
   nor fails its loop (R5/R6).
5. **Placeholder writes guarded** — a cancelled or timed-out placeholder is never
   overwritten by a later rollup or retirement (§4.6).
6. **Adoption of partial expansions** — placeholders stranded by past crash-hole
   incidents are moved to `running` and roll up normally (R0).
7. **Stale reads cannot be applied** — any write to the job's rows between snapshot and
   apply forces a re-run (§4.7); today's autocommitted statements apply stale decisions.

Cost named for fix 4: a handler that crashes between `mark_failed` and the reset leaves
a retry-pending row forever; its dependents then stay `pending` where today a later
cascade would skip them and settle the job `failed`. Today the same crash leaves the job
stuck unless another step happens to complete; the "failed but never orchestrated"
sweep belongs to the settlement branch (TODO).

## 5. Data Model

No migration. No new columns. The `for_each` instance/placeholder relationship stays a
naming convention plus `loop_source`, and no trigger or foreign key is added.

## 6. Server

### 6.1 New

- `crates/stroem-server/src/cascade.rs` — `Change`, `RollupOutcome`, `Snapshot`, `Plan`,
  `Applied`, `run`, `apply`, `execute`, the moved helpers `parse_for_each_items`,
  `render_for_each_template`, `MAX_FOR_EACH_ITEMS` (with their existing unit tests,
  `job_creator.rs:1979-2060`), and an internal snapshot type.
- `stroem-db` transaction primitives (thin, one statement each):
  `cascade_lock_tx(tx, job_id)` and `CASCADE_LOCK_CLASS`,
  `JobRepo::get_status_tx`, `JobRepo::mark_running_if_pending_tx`,
  `JobStepRepo::get_status_vector_tx`, `promote_steps_tx`, `skip_steps_tx`,
  `fail_pending_step_tx`, `start_placeholder_tx`, `rollup_placeholder_tx`.
  `create_steps_tx` already exists.

### 6.2 Deleted (activation commit)

- `JobStepRepo::promote_ready_steps`, `JobStepRepo::skip_unreachable_steps` and their
  tests (`stroem-db/tests/integration_test.rs::test_promote_ready_steps`,
  `job_step_status_tests.rs` × 3). Their `ready_at` / `completed_at` assertions move to
  the `apply` tests (§9.2).
- `job_creator::expand_for_each_steps`, `job_creator::check_loop_completion`.
- `stroem-db`'s use of `stroem_common::template::evaluate_condition`; drop the import if
  it becomes unused.

### 6.3 Unchanged

`settle_if_all_terminal`, `handle_task_steps`, `handle_approval_steps`,
`orchestrate_after_step` (minus the deleted call), `propagate_to_parent` (minus the
deleted call), the step-retry reset, task retry, hooks, cancellation, recovery.

## 7. Documentation

- `CONTEXT.md` (new): step cascade, placeholder, instance, retirement, adoption,
  rollup, plan, snapshot, change, retry-pending, settlement (pointer to the later
  design). Links back to CLAUDE.md.
- `CLAUDE.md`: link to `CONTEXT.md` in the overview; new `### Step Cascade` section
  (interface, phase order, effective status, the advisory lock class and the two-integer
  rule, "commit before dispatch", callers, the rule that new context variables keep
  `job` first); rewrite the "Placeholder resolution lives inside the orchestrator
  cascade" paragraph under For-Each Loops and the failed-dep note to point at
  R0/R4/R5/R6; remove the "Do NOT call `expand_for_each_steps` after
  `on_step_completed`" warning; correct the truthiness summary under Conditional Flow
  Steps (`null`/`none` are also false, trimmed, case-insensitive); under Retry
  Mechanism, describe retry-pending and the task-step ownership gap.
- `crates/stroem-db/README.md:86`: remove the orchestration API it advertises.
- `docs/internal/TODO.md`: close the in-file TODOs from `job_step.rs:678-690` if tracked;
  add: `type: task` step retry config never honoured (child propagation / dispatch
  failure bypass the retry check); recovery's unguarded `mark_failed` can overwrite a
  `completed` row; "failed but never orchestrated" sweep (settlement branch); remove the
  `None` workspace-config mode (settlement branch); move `build_step_render_context`
  (candidate 7).
- No user-facing docs change: user-visible semantics are unchanged except the seven
  fixes, none of which changes a documented behaviour.

## 8. Known Limitations

- `run` depends on `job_creator::build_step_render_context`, and `job_creator` calls
  `cascade::execute`. Module-level mutual reference within the crate; resolved when the
  builder moves (candidate 7).
- The `Option<&WorkspaceConfig>` mode is a test-only concept carried through one more
  branch.
- Rendering can run `vals`; the cascade runs it outside any lock but does not bound it.
- Three verification failures in a row surface as an error from `on_step_completed`.
  Under the advisory lock the only writers that can cause a mismatch are cancellation,
  recovery timeouts, worker claims/completions on other steps of the job, approvals and
  the retry reset; three in a row on one job is pathological and the error is logged
  with the job id.
- Retry-pending detection is a heuristic over `(status, retry_attempt, max_retries,
  action_type)`; it is exact for today's failure paths and must be revisited if a new
  failure path is added without the retry check (CLAUDE.md records the rule).

## 9. Tests

### 9.1 Unit, at the cascade interface (`cascade.rs`, no DB)

A row builder and a flow builder. Cases assert on the returned `Plan`:

- linear promotion; diamond join waits for both branches
- failed dep skips dependents; `continue_on_failure` promotes; mixed skipped+failed
  without cof blocks; cancelled dep with cof promotes
- all-deps-skipped cascade-skip, including overriding a truthy `when`
- `when` true / false / error; sibling truthy and falsy `when` evaluated in one phase;
  `when` with `None` config stays pending while R1/R3 still fire
- multi-level cascade skip reaches the fixpoint in one `run`
- **phase-order fixtures**: Codex's counterexample (`p` expands because `a` was skipped
  earlier in the same pass); a `when` that inspects a loop rolled up in P0 of the same
  pass sees it; a `when` that references a step **skipped** in P1 of the same pass does
  not see it until the next pass (a `ready` row never enters the context, so promotion
  is not the right fixture)
- **retry-pending fixtures**: dep `a` failed with budget, `b` stays pending (R3 does not
  fire) and is not satisfied for R2; the same with `a` a `type: task` step → `b` is
  skipped (ownership rule); `a` failed with budget exhausted → `b` skipped; a placeholder
  whose dep is retry-pending stays pending
- placeholder: failed dep skips it; cof keeps it; all-deps-skipped skips it; `when`
  false skips; `when` error fails; expression error fails; empty array skips; >10 000
  fails with the exact text; literal JSON array vs Tera string; parallel vs sequential
  instance statuses; placeholder absent from the flow is ignored; `[0]` exists → `Adopt`
- sequential advance: promote next; failure skips all pending, including a pending
  successor of a completed later instance (`[failed, completed, pending]`); cof promotes
  past a failure; no-op while `[i]` is `ready`/`running`; no-op while `[i]` is
  retry-pending; a `type: task` instance failed with budget is terminal and skips the
  rest; failure followed by immediate rollup in the same `run`
- rollup: completed array in `loop_index` order with `null` for missing output; failed
  list text; only `failed` fails the loop, `cancelled` does not; no rollup while any
  instance is non-terminal or retry-pending; a `type: task` instance failed with budget
  rolls up as failed; no rollup for a non-running placeholder; no rollup with zero
  instances; missing flow entry → non-sequential, no cof
- downstream `when` sees an expanded placeholder's rollup output within the same `run`
- termination: a large DAG reaches the fixpoint; a snapshot at fixpoint returns an
  empty plan (idempotency); `assumed` equals the input vector
- rows absent from the flow are ignored
- the `test_convergence_without_continue_on_failure` scenario

The parser tests moved from `job_creator.rs` (JSON-encoded template, `[object]` /
`json_encode()` diagnostic) stay with the helpers.

Mapping: each `orchestrator_test.rs` test whose assertions are on step statuses after a
cascade has a `run` twin above; the tests that also assert settlement, job output,
cancellation timestamps or "no settlement while live" (`:371`, `:1724`, `:1753`,
`:1772`) are retained as integration tests and are not claimed as reproduced.

### 9.2 Integration (container), new

- `apply` is transactional: apply an `Expand`, drop the transaction without commit,
  assert no instance rows and placeholder still `pending`. (fix 1)
- `apply` timestamps: `Promote` sets `ready_at`; `Skip`/`Fail`/`Rollup` set
  `completed_at`; `Expand`/`Adopt` set `started_at` (assertions transferred from the
  deleted DB tests).
- **Verification-mismatch handshake** (no code hook): connection A begins a transaction
  and takes the cascade lock for the job; the test spawns `execute` on the pool, which
  computes its plan and blocks on the lock; connection C cancels one pending step and
  commits; A rolls back, releasing the lock; `execute` verifies, sees the mismatch,
  re-runs, and its returned plan carries no change for the cancelled row; final state
  reflects the cancellation. A second variant has C reset a `failed` instance for retry
  (`reset_for_retry`) and asserts the loop is not rolled up. (fixes 2, 7)
- Concurrency: two `execute`s for the same job run concurrently after two sibling steps
  complete; the join step is promoted exactly once and the final state equals a serial
  run. (fix 2)
- Compensation interleaving: connection A holds the cascade lock; a creation whose init
  fails is driven to its compensation on the pool and blocks; A releases; compensation
  completes; final job `failed` with all non-terminal steps failed; then an `execute`
  on the job returns an empty plan.
- Retry vs unrelated cascade: instance `x[1]` `failed` with a retry remaining, a
  cascade triggered by an unrelated step does not roll up `x` and does not skip a step
  depending on `x`; after the reset and a successful re-run, `x` rolls up `completed`.
  (fix 4)
- Task-step retry ownership: a `type: task` step failed by child propagation with retry
  budget is treated as terminal; its dependents are skipped. (fix 4, ownership)
- Timeout vs rollup: recovery fails a running placeholder by timeout; a later instance
  completion does not overwrite it. (fix 5)
- Cancelled placeholder: rollup leaves it `cancelled`. (fix 5)
- Adoption: instances exist, placeholder `pending`; one `execute` moves it to `running`
  and, once instances are terminal, rolls it up. (fix 6)
- Self-healing: placeholder `running`, all instances terminal, no prior rollup → one
  `execute` rolls it up. (fix 3)
- Parent/child: a child job's cascade and its parent's cascade run concurrently; both
  complete.
- `on_step_completed` and creation-time cascade produce identical results for the same
  DAG.

### 9.3 Oracle

`crates/stroem-server/tests/{orchestrator_test,integration_test,restart_integration_test,
rerun_integration_test,propagate_to_parent_test,metrics_test}.rs` and
`crates/stroem-db/tests/*` run green after every commit. Changes to them before the
final pruning commit: the four DB-level tests deleted with their functions and
`test_convergence_without_continue_on_failure` rewritten, both in the activation commit.
Explicitly retained, never pruned: retry reset/history and suppression of downstream work
while retrying (`integration_test.rs:21672`, `:22138`), approval-to-promotion
(`:19987`), instance timeout inheritance (`:23312`), dispatch-failure re-orchestration
(`:23528`), child settlement propagation (`:23852`), injected approval-dispatch failure
with compensation (`:25130`), and the four settlement-asserting orchestrator tests named
in §9.1.

### 9.4 E2E

`tests/e2e.sh` unchanged; it already covers for_each, `when`, and task-action flows.

## 10. Rollout

- Pure server change, no migration, no config. Ships as a patch release.
- Commit sequence (each green against the oracle; nothing before commit 5 changes
  runtime behaviour):
  1. `cascade::run`, `Snapshot`, `Plan`, `Change`, the internal snapshot type, and the
     `run` unit suite (§9.1 minus adoption, which lands with commit 8). The parser
     helpers are **not** moved yet; `run` calls them at their current location.
  2. stroem-db transaction primitives, `CASCADE_LOCK_CLASS`, `cascade_lock_tx`, and
     `apply` + its integration tests (transactional, timestamps).
  3. `execute` (lock, verify, retry) + the verification-handshake and concurrency tests,
     driven directly against `execute` on fixture jobs.
  4. Creation compensation takes the cascade lock as its first statement +
     compensation-interleaving test. No cascade uses the lock in production yet, so this
     is inert.
  5. **Activation**: `on_step_completed` and the creation-time loop switched to
     `execute`; `check_loop_completion` and its two calls, `expand_for_each_steps`,
     `promote_ready_steps`, `skip_unreachable_steps` and their four DB tests deleted;
     parser helpers and their tests moved into `cascade.rs`;
     `test_convergence_without_continue_on_failure` rewritten. Fixes 1–5 and 7 go live
     here.
  6. Regression-test commits, one per fix: self-healing (3), retry vs unrelated cascade
     and task-step ownership (4), timeout-vs-rollup and cancelled placeholder (5),
     `on_step_completed` vs creation equivalence.
  7. Adoption: R0 in `run`, `Adopt` in `apply`, unit fixtures and the integration test
     (fix 6).
  8. Docs: `CONTEXT.md`, CLAUDE.md, DB README, TODO.md.
  9. Prune container tests whose every assertion is now covered by a `run` twin.

## 11. Resolved Questions

| Q | Decision |
|---|---|
| Q1 slicing | cascade first, settlement on its own branch |
| Q2 behaviour | preserving; by-construction fixes with their own regression-test commits |
| Q3 home | `stroem-server/src/cascade.rs` |
| Q4 output | `Plan` of `Change`s + assumed snapshot, repo primitives apply |
| Q5 tests | container suite is the oracle; unit tests at the interface; prune last |
| Q6 inputs | task, job row, steps, `Option<&WorkspaceConfig>`; context built inside |
| Q7 fixpoint | in memory, inside `run` |
| Q8 atomicity | one transaction (rev 1: job row lock; superseded by Q15/Q23/Q24) |
| Q9 rollup | absorbed as global rules; `check_loop_completion` deleted |
| Q10 repo fns | deleted with their four tests |
| Q11 `None` mode | preserved on this branch |
| Q12 vocabulary | `CONTEXT.md`, cross-referenced from CLAUDE.md |
| Q13 lock sharing | accept sharing the job row lock (superseded by Q23) |
| Q14 phase model | reproduce today's phase boundaries; two contexts per pass; P0 is an extension |
| Q15 rendering vs lock | render outside; verify and apply inside; retry ×3 |
| Q16 retry-aware terminality | retry-pending rows are non-terminal (extended by Q25) |
| Q17 placeholder guards | keep stronger guards; preserve column behaviour |
| Q18 sequential advance | global rule (policy restated by Q26) |
| Q20 rollup errors | propagate (abort before settlement) |
| Q22 partial expansions | adopt (R0) |
| Q23 lock | advisory xact lock keyed on job id, not the row lock |
| Q24 verification | whole-snapshot vector (job status + every step's status and retry_attempt) |
| Q25 retry ownership | effective status everywhere; `action_type != "task"` |
| Q26 sequential failure | failure precedence over successor promotion; no equivalence claim |
| Q27 commit sequence | land unused, one activation commit, test commits after, adoption later |

## 12. Review Log

- 2026-09-08 — rev 1 written after the design walk.
- 2026-09-08 — Codex review of rev 1 (session `01a07fd5-bf6b-7551-9d70-14f51079e953`),
  verdict "not ready": phase ordering observable; `run` not pure (`vals`); compensation
  lock-order inversion; job lock does not stabilise steps; missed test caller; plus
  should-fixes on R4/R5/R6 semantics, guards, termination, lock inventory, tests, docs.
- 2026-09-08 — rev 2: phase model (§4.4), render-outside/verify-touched-rows (§4.7),
  advisory lock, compensation lock, retry-aware terminality for R5/R6, adoption, R5
  keyed-equivalent form, guard table, test mapping.
- 2026-09-08 — Codex second pass on rev 2, verdict "not ready": three blockers closed
  (phase order, rendering, compensation, test caller); touched-row verification
  insufficient (retry window on ordinary deps, unguarded recovery overwrite, job-status
  cancellation not covered, `assumed` contract, R7 zero-row); retry budget ≠ scheduled
  retry for `type: task` instances; R5 not exactly equivalent
  (`[failed, completed, pending]`); measure ranks and pass bound; P0 is an extension;
  wrong next-pass fixture; advisory key collision with leader; commit sequence gaps.
- 2026-09-08 — rev 3 (this document): whole-snapshot verification (Q24, §4.7); effective
  status with retry ownership across all rules (Q25, §4.3); R5 failure precedence and
  listed divergences (Q26); `Snapshot` in `Plan`, R7 zero-row exemption (§4.2, §4.6);
  two-integer lock key with class const (§4.7.1); measure over all six statuses, bound
  `4|U|`, P0 stated as extension (§4.4); fixtures corrected (§9.1); handshake spec'd
  (§9.2); activation-commit sequence (Q27, §10); task-step retry bypass and recovery
  overwrite logged as pre-existing (§3, §7). Third Codex pass pending.
