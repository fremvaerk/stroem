# Step Cascade — Design

**Status:** Reviewed, revision 7 (2026-09-08; Codex sixth pass "ready with listed
changes", all applied). Awaiting owner sign-off before the implementation plan.
**Date:** 2026-09-08
**Origin:** architecture review 2026-09-07/08, candidate "Make the step cascade a pure
module"; first internal seam of the later "Job settlement" module (separate branch, not
covered here).
**Builds on:** `2026-09-08-fail-or-retry-design.md` (prerequisite branch, ships first
and must be running on **every** server replica before this design's activation commit
is deployed). After it lands, no step row is ever observable as `failed` while a retry
is owed, and this design is purely status-driven: it contains no retry logic.

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
- Loop rollup is keyed on the completing instance and its writes are unguarded
  (`job_step.rs:465-505`): a later instance completion overwrites a cancelled or
  timed-out placeholder.
- Nothing in this path is testable without a Postgres container. `orchestrator.rs` has
  zero in-module tests; the 34 tests in `orchestrator_test.rs` all spin a container.

## 2. Decisions

Settled in the design walk on 2026-09-08 (Q1–Q13) and after three Codex reviews
(Q14–Q30).

1. **Scope:** cascade only on this branch. The settlement module (absorbing
   `finalize_created_job` and `reconcile_settled_children`) is a separate branch with its
   own design. The atomic fail-or-retry change is a separate prerequisite branch.
2. **Behaviour policy:** preserving, except defects the new shape fixes by construction
   with no extra code (§4.10 lists all six). The fixes go live in the one activation
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
8. **Rendering outside the lock; verified, row-locked apply inside** (Q15, Q24, Q29,
   Q30): the snapshot is read and `run` executes with no transaction open; `execute`
   then begins a transaction, takes the job's advisory lock, re-reads the job status and
   every step row's `(status, output::text, error_message)` **with `FOR UPDATE` on the
   step rows**, where the two value columns are read as-is (SQL `NULL` preserved) for
   every row whose `output`/`error_message` a rule or template can read (`completed`,
   `skipped`, `failed`, `suspended`, `cancelled`) and forced to `NULL` otherwise, and
   applies only if that vector is identical to the one the plan was computed from.
   Verification is therefore exact over the values the plan depended on: no hash, no
   version token, and `NULL` stays distinct from `''` (the renderer distinguishes them,
   `job_creator.rs:1643-1652`). Row data and the verification columns always come from
   one `SELECT`. Any difference, and a Postgres deadlock report (`40P01`), rolls back
   and re-runs from a fresh snapshot, at most three times. Batched statements assert
   their row counts. (Q29 as refined by Q34.)
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
    `(CASCADE_LOCK_CLASS, hashtext(job_id))` (§4.7.1), not the job row (Q23). Acquiring
    it never waits on an artifact upload, cancellation, worker start or log upload. The
    one job-row write the cascade makes (R7) can wait behind an artifact upload's row
    lock for the duration of that upload; see §4.7 lock inventory. Creation compensation
    and cancellation's step sweeps take the same advisory lock first (§4.7, Q31), so
    the only multi-row `job_step` writers are serialised against the cascade rather than
    racing its ordered row locks.
14. **Phase model** (Q14): a pass runs rollup/advance, then promote and cascade-skip with
    a context built before them, then skip-unreachable, then retirement/expansion with a
    context rebuilt after those changes. Changes apply to the snapshot phase by phase.
    No order-independence claim.
15. **No retry logic in the cascade** (Q28, reversing Q16/Q25): a `failed` row is
    failed. The prerequisite branch guarantees a row owed a retry is never `failed`.
16. **Stronger guards on placeholder writes** (Q17): retirement requires `pending`,
    rollup requires `running`. Column behaviour preserved (failure keeps `output`,
    completion keeps `error_message`). Fix by construction.
17. **Sequential failure precedence** (Q18, Q26): for a running sequential placeholder,
    any `failed` or `cancelled` instance without `continue_on_failure` skips every
    pending instance; only otherwise is `[i+1]` promoted. Not claimed equivalent to
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
22. **Cancellation takes the cascade lock** (Q31): `cancel_pending_steps` and
    `cancel_server_managed_steps` run in one transaction that first calls
    `cascade_lock_tx`. A multi-row `UPDATE` acquires row locks in scan order, which is
    not the cascade's `ORDER BY step_name` order; without the shared lock the two can
    form a lock-wait cycle. The rule, recorded in CLAUDE.md: any statement that updates
    more than one `job_step` row of a job must hold the cascade lock or be split per row.
23. **Deadlock is a mismatch, on the cascade side** (Q32): if Postgres reports `40P01`
    inside `execute`, the transaction is rolled back and counted as a verification
    failure; the re-run sees the committed state. Postgres chooses the victim and the
    cascade cannot influence it, so this is defence in depth, not a guarantee: after
    Decisions 22 and 24 every known writer that could cycle with the cascade holds the
    cascade lock, and a writer that still deadlocks is a bug in that writer, surfaced
    through its own error path (cancellation returns 500; recovery logs and moves on;
    task retry logs and leaves the job failed).
24. **Task-level retry takes the cascade lock** (Q33): `try_retry_job`'s transaction
    (`job_recovery.rs:1165-1200`: new job row → old job row → new job's `ready` steps)
    runs after the new job's creation-time cascade has committed, so a cascade on the
    new job can be in flight. The transaction takes `cascade_lock_tx(new_job_id)` as its
    first statement. The old job is terminal and has no cascade; no second lock.

## 3. Non-Goals

- Changing any user-visible semantics of `when`, `for_each`, `sequential`,
  `continue_on_failure`, or step status transitions, beyond the six fixes in §4.10.
- Settlement (`settle_if_all_terminal`), terminal handling, propagation, hooks, task
  retry. They run before or after the cascade exactly as today.
- Retry. Decided at the failure mark by the prerequisite branch. The cascade neither
  reads `retry_attempt`/`max_retries` nor distinguishes failure origins.
- Fixing the pre-existing gaps in which failure paths honour step retry: child-job
  propagation (`job_recovery.rs:575`), task dispatch failure (`job_creator.rs:743`) and
  approval dispatch/render failure (`job_creator.rs:1435`, `:1473`) never retry, while
  a `type: task` step timed out by recovery does. Logged to TODO for the transition
  candidate; unchanged here.
- Fixing recovery's unguarded `mark_failed` on timed-out steps (`job_step.rs:502`),
  which can overwrite a `completed` row. The cascade tolerates it through verification
  and row locks; the guard belongs to the transition candidate.
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
assumed.

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
/// order, plus the job status. `values` is `Some((output::text, error_message))`, each
/// inner value nullable exactly as in the row, for rows whose `output`/`error_message`
/// a rule or template can read — the context statuses (completed, skipped, failed,
/// suspended) plus `cancelled`, whose `output` R6 copies into a rollup — and `None`
/// otherwise. Row data and `values` come from ONE `SELECT` (`get_steps_for_job`
/// already returns both columns), so a plan is never built from one tuple's data and
/// another's values. `execute` applies a plan only if the database still holds this
/// vector, compared field by field with SQL `NULL` distinct from `''`.
pub struct Snapshot {
    pub job_status: JobStatus,
    pub steps: Vec<(String, StepStatus, Option<(Option<String>, Option<String>)> /* values */)>,
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
/// guard from §4.6 and asserts its affected-row count equals the number of rows the
/// change names; after a successful verification under row locks a shortfall is a
/// bug and returns an error (the caller rolls back). The job-row update of R7 is the
/// one statement allowed to match zero rows.
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

**Columns a rule or template can read**, and how each is covered by verification:
`status` (all rows, compared directly); `output`, `error_message` (context rows and
`cancelled` instance rows, whose `output` R6 reads and which cancellation's separate
sweeps can leave open to a late output-only write, `cancellation.rs:53`,
`job_creator.rs:1491`; compared as values, `NULL`-preserving); `when_condition`, `for_each_expr`, `loop_source`, `loop_index`,
`loop_total`, `loop_item`, `action_type`, `action_spec`, `timeout_secs`, the retry
columns (immutable after row creation except the retry columns, which only change with
a status change under the prerequisite branch); `job.status` (compared directly);
`job.input`, `job.revision` (immutable after creation). Columns no rule reads:
`agent_state`, `suspended_at`, `ready_at`, `started_at`, `completed_at`, `retry_at`,
`worker_id`. A row whose status is `running`/`claimed`/`ready`/`pending` may have those
written (agent-state saves, claims, heartbeats) without invalidating a plan, which is
why `version` is `None` for it.

### 4.3 Rules evaluated by `run`

Rule inputs come from the same sources they do today and the implementation must not
change which source a rule reads:

- row: `status`, `when_condition`, `for_each_expr`, `loop_source`, `loop_index`,
  `output`;
- flow (`task.flow[step]`): `depends_on`, `continue_on_failure`, `sequential`. A row
  whose name is absent from the flow is ignored by R1–R4; for R5/R6 a placeholder absent
  from the flow behaves as `sequential = false`, `continue_on_failure = false` (today's
  `unwrap_or_default`, `job_creator.rs:1236`).

Statuses are read literally. A `failed` row is failed (Decision 15).

**Dependency satisfaction** (shared by R2 and R4): every dependency is `completed` or
`skipped`, or `failed`/`cancelled` when the step has `continue_on_failure`.

**R0 — Adopt.** A `pending` placeholder present in the flow for which a row named
`"{placeholder}[0]"` exists → `Adopt`. Evaluated before R4 so a partially expanded
placeholder is never re-expanded. (Fix 6; today the guard at `job_creator.rs:973-977`
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
without `continue_on_failure`, with at least one dependency `failed` or `cancelled` →
`Skip`. A `skipped` dependency does not trigger this rule. Applies with or without a
workspace config.

**R4 — Placeholder retirement / expansion.** A `pending` placeholder (row
`for_each_expr.is_some()`) present in the flow, not adopted by R0, evaluated only when a
workspace config is present (with `None`, placeholders stay `pending` whatever their
dependencies):
- dependencies not satisfied, and at least one `failed`/`cancelled` without
  `continue_on_failure` → `Skip`;
- dependencies not satisfied otherwise → no change;
- non-empty dependency list, all `skipped`, no `continue_on_failure` → `Skip`;
- `when` falsy → `Skip`; `when` error → `Fail { "when condition error: …" }`;
- `for_each` render/parse error → `Fail { "for_each expression error: …" }`;
- empty array → `Skip`;
- more than `MAX_FOR_EACH_ITEMS` (10 000) items →
  `Fail { "for_each produced {} items (max {})" }`;
- otherwise → `Expand`, with instances built as today (copied action columns and retry
  configuration columns, `retry_attempt` left to its DB default of 0,
  `when_condition: None`, `for_each_expr: None`, `loop_*` set; all `ready` when
  parallel, `[0]` `ready` and the rest `pending` when `sequential`).

The `when` and `for_each` templates are rendered against the context built for this
phase (§4.4), as today's two `build_step_render_context` calls at
`job_creator.rs:1040`/`:1055` both use the post-promote snapshot.

**Instance terminality** (shared by R5 and R6): status is `completed`, `skipped`,
`cancelled` or `failed`.

**R5 — Sequential advance.** For a `running` placeholder present in the flow with
`sequential = true`, with instance rows sorted by `loop_index`:
1. if any instance is `failed` or `cancelled`, and the flow step has no
   `continue_on_failure` → `Skip` for every `pending` instance; nothing else for this
   placeholder;
2. otherwise, for each terminal instance `i` whose `[i+1]` exists and is `pending` →
   `Promote { "{placeholder}[i+1]" }`.

Divergences from today's keyed `check_loop_completion` (`job_creator.rs:1248-1296`),
deliberate and tested (§9.1):
- R5 runs on every cascade of the job, not only when an instance of this loop completes
  (self-healing, fix 3).
- **Behavioural change:** in the state `[failed, completed, pending]` (reachable via
  recovery's unguarded timeout write landing on `[0]` after `[1]` completed) today
  promotes `[2]` on `[1]`'s completion, and a worker may claim and run it before `[0]`'s
  later re-orchestration skips whatever is still pending. R5 never promotes `[2]`. The
  new outcome is "the loop stops at the first failure", which is the documented meaning
  of `sequential` without `continue_on_failure`.
- R5 requires the placeholder to be `running` (fix 5).

**R6 — Rollup.** For a `running` placeholder that has at least one instance row and
whose instances are all terminal:
- any instance `failed` and no `continue_on_failure` →
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
  instances (status per R4; `loop_source` set; `retry_attempt` 0; timestamps `now`;
  worker/output/error empty). `Adopt` sets the placeholder to `running`.
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

Row-count policy. Every step-row statement compares its affected-row count with the
number of step names it carries. After §4.7's verification under `FOR UPDATE` no other
writer can have moved those rows, so a shortfall is a programming error: `apply` returns
`Err` and the caller rolls back. The job-row update (Expand/Adopt step 3) is exempt: it
is conditional on `job.status = 'pending'` and legitimately matches zero rows once the
job is already `running` (today's `mark_running_if_pending_server`, `job.rs:434`).

For `Expand` the placeholder update runs first, then the insert, so instances exist only
if the placeholder transition is in the same committed transaction (fix 1).

### 4.7 `execute`: render outside, verify and apply inside

```
attempt = 0
loop:
  job   = JobRepo::get(pool, job_id)                             -- no transaction
  rows  = JobStepRepo::get_steps_for_job(pool, job_id)          -- <STEP_COLUMNS>, one SELECT
  plan  = run(task, &job, &rows, workspace_config)              -- may render, may run vals
  if plan.changes.is_empty(): return plan

  BEGIN
    SELECT pg_advisory_xact_lock(CASCADE_LOCK_CLASS, hashtext($1))   -- §4.7.1
    current.steps = SELECT step_name, status,
                           CASE WHEN status IN ('completed','skipped','failed','suspended','cancelled')
                                THEN output::text END      AS output_text,
                           CASE WHEN status IN ('completed','skipped','failed','suspended','cancelled')
                                THEN error_message END     AS error_message
                    FROM job_step WHERE job_id = $1
                    ORDER BY step_name
                    FOR UPDATE                                        -- row locks, all steps
    current.job_status = SELECT status FROM job WHERE job_id = $1     -- no lock
    if current != plan.assumed:
        ROLLBACK; attempt += 1; continue-or-fail
    applied = apply(&mut tx, job_id, &plan)
  COMMIT
  info!(job_id, attempt, promoted, skipped, failed, expanded, adopted, rolled_up)
  return plan

on SQLSTATE 40P01 (deadlock detected) anywhere inside BEGIN..COMMIT:
    ROLLBACK; attempt += 1; continue-or-fail                            -- Decision 23
continue-or-fail: if attempt == 3: return Err("cascade verification failed 3 times")
```

`assumed` is built from the same `JobStepRow`s `run` reads: `Snapshot::from_rows`
applies the readable-status predicate in Rust and keeps `output` (serialised with
`serde_json::to_string`, which for a `jsonb` value round-tripped through `::text` is
byte-identical because Postgres renders `jsonb` canonically) and `error_message` as
`Option`s. The in-transaction check reads the two columns with the same predicate in
SQL, under `FOR UPDATE`. The predicate (the status list) is one shared const used by
both, so it cannot drift. Comparison is field by field on `Option` values: a row whose
`error_message` went from `NULL` to `''` (or back) mismatches, as the renderer treats
those differently (`job_creator.rs:1643-1652`, no `error` key vs an empty one). No
hash, so no collision argument is needed.

**Verification contract.** A plan is applied only if the job's status, every step row's
status, and the `output`/`error_message` values of every row whose values can be read
are exactly what `run` computed from. Any status change to any row of the job between snapshot and verification —
cancellation (`JobRepo::cancel` on the job row, `cancel_pending_steps` on step rows), a
recovery timeout write, a worker claim or completion, an approval, a retry (which under
the prerequisite is a `running → ready` status change), another cascade's commit — and
any same-status rewrite of a context-visible row (approval message,
`job_creator.rs:1487`; an unguarded `mark_completed`/`mark_failed` on an already
terminal row, `job_step.rs:480`/`:504`) changes the vector and forces a re-run on a
fresh snapshot.

**Protection through apply.** The verification `SELECT … FOR UPDATE` locks every step
row of the job until commit. Any writer that would change one of those rows between
verification and apply (recovery's select-then-fail, `recovery.rs:123-143`; a worker
completing; cancellation's step sweep) blocks until the cascade commits, then proceeds
against the committed state with its own guards. The job row is not locked by
verification; R7's `UPDATE` takes its row lock at write time and is guarded on
`status = 'pending'`, so a cancellation landing between verification and R7 makes R7 a
no-op, and the cancelled job's steps are cancelled by the sweep once the cascade's row
locks release.

**Lock order and inventory.** The cascade's order is: advisory lock → step rows
(`FOR UPDATE`, acquired in `step_name` order) → job row (R7 `UPDATE`). Two kinds of
writer matter: multi-statement transactions (can hold one lock while waiting for
another) and multi-row single statements (acquire several row locks in scan order,
which can cycle with the cascade's ordered scan).

Multi-statement transactions touching `job` or `job_step`, and their order:
- creation compensation (`job_creator.rs:669-680`): advisory lock →
  `fail_non_terminal_steps_tx` (multi-row) → `mark_failed_tx` (job). Same order as the
  cascade; the multi-row update runs under the advisory lock.
- cancellation, after Decision 22: advisory lock → `cancel_pending_steps` (multi-row)
  → `cancel_server_managed_steps` (multi-row). `JobRepo::cancel` on the job row stays a
  separate autocommitted statement before it, as today.
- task-level retry `try_retry_job` (`job_recovery.rs:1145-1200`): the new job is
  created and its creation-time cascade committed **before** this transaction begins
  (`create_job_for_task_detailed`, `:1145`); the transaction then updates the new job
  row (`:1172`, setting `retry_of_job_id`, which takes a `KEY SHARE` lock on the
  referenced retry-chain root job row through the FK in `027_step_retry.sql:20`), the
  old job row (`:1182`) and, when the delay is positive, every `ready` step of the new
  job (`:1191`, `UPDATE job_step SET retry_at … WHERE job_id AND status='ready'`, a
  multi-row statement). Today this is job → steps, the reverse of the cascade. After
  Decision 24 it is advisory lock(new job) → new job row → root job row (`KEY SHARE`)
  → old job row → new job's steps. Why this does not cycle, stated narrowly: the new
  job's advisory lock serialises this transaction against the new job's cascades; the
  transaction never touches an old-job or root-job `job_step` row, so it can never
  wait behind a cascade's step-row locks on those jobs; a cascade on the old or root
  job (which is possible — nothing in `orchestrate_after_step` excludes terminal jobs,
  `job_recovery.rs:146-185` — for instance a late worker report) holds that job's step
  rows and takes its job row only for R7, which is guarded on `pending` and touches the
  row only if the job is still pending, which a retried job is not; `KEY SHARE` on the
  root row conflicts only with `FOR UPDATE`/key updates on that row, which no cascade
  issues; and the production caller runs task retry only for top-level jobs
  (`parent_job_id.is_none()`, `:390-397`), so no parent cascade holds anything on the
  old job. The test in §9.2 covers the new-job cascade interleaving.
- job creation and restart seeding (`create_with_parent_tx`, `create_steps_tx`,
  `seed_steps_tx` which inserts and then updates the rows it just inserted,
  `job_step.rs:301-320`): all inside the creation transaction, on rows no other
  transaction can see until it commits. No cascade on that job exists yet. No cycle.
- retention `JobRepo::delete` (`job.rs:859`, called from `recovery.rs:427`):
  `DELETE FROM job WHERE job_id = $1`, whose FK cascades to `job_step` — job row then
  step rows, inside one statement. Retention only selects terminal jobs older than
  `retention.job_days`. A cascade on such a job is not impossible (see the task-retry
  entry: a late worker report can orchestrate a terminal job), only implausible after
  days. If the two coincide, Postgres aborts one side: if it is the cascade, it re-runs
  (Decision 23) and finds the job gone or unchanged; if it is retention, it logs and
  continues (`recovery.rs:427-430`) and deletes the job on its next sweep. Accepted;
  no coordination added, because retention must not take per-job locks for thousands
  of rows.
- artifact upload (`artifacts.rs:165-206`): job row `FOR UPDATE` → `job_artifact`.
  Never touches `job_step`. The cascade's R7 can wait on it; it never waits on the
  cascade.
- agent state save / suspend, state and artifact upload endpoints, worker start, log
  upload: single-row statements on one `job_step` row or the `job` row, no transaction
  held across a wait.

Multi-row single statements on `job_step` outside the cascade (found by grepping
`UPDATE job_step` for predicates without `step_name = $n`): `cancel_pending_steps`,
`cancel_server_managed_steps` (both under the advisory lock after Decision 22),
`fail_non_terminal_steps_tx` (under the advisory lock in compensation),
`try_retry_job`'s `retry_at` update (under the advisory lock after Decision 24), the
legacy batched promote/skip statements (`job_step.rs:775`, `:791`, `:903`,
`job_creator.rs:1290`; deleted in the activation commit), and the cascade's own
batched `Promote`/`Skip` (under the advisory lock by construction). Recovery marks
steps one row at a time (`recovery.rs:100-264`). Rule for future code (CLAUDE.md): a
statement updating more than one `job_step` row of a job holds the cascade lock or is
split per row.

Single-row writers (worker claim with `SKIP LOCKED`, completion, approval, recovery,
the retry write, agent state) either skip locked rows or wait on one row until the
cascade commits; they hold nothing while waiting. Decision 23 is the backstop for a
writer this inventory missed: the cascade side re-runs; the other side's own error
path applies, because Postgres picks the victim.

R7 can wait behind an in-flight artifact upload for the duration of that upload's blob
write; that wait happens while the cascade holds the step-row locks, so workers polling
that job skip it for the same duration. Accepted (Decision 13).

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
any future two-integer advisory lock must use a different class. `hashtext` is
deterministic within a database (no per-session seed). Residual risk: two job ids
hashing to the same 32-bit value serialise their cascades against each other; harmless.
One SQL helper, `JobRepo::cascade_lock_tx`, is the only place the expression is written;
compensation, cancellation and task-level retry call it too. Precedent for the transaction-scoped form: creation
compensation already runs a multi-statement transaction on a pooled connection
(`job_creator.rs:669-680`); `pg_advisory_xact_lock` releases at transaction end, commit
or rollback.

### 4.8 Callers after the change

- `orchestrator::on_step_completed(pool, job_id, step_name, task, workspace_config)`:
  signature unchanged. Body becomes `cascade::execute(...)` then
  `settle_if_all_terminal(...)`. The three `tracing::info!` lines are replaced by
  `execute`'s summary line. Note: with `None`, this function can now roll up or advance a
  loop (R5/R6 need no context) where today it cannot; no production caller passes `None`.
- `job_creator::create_job_for_task_inner`, `init` block: the loop at `:618-641`
  becomes one `cascade::execute(pool, job_id, task, Some(workspace_config))`. The
  compensation path (`:663-694`) additionally calls `cascade_lock_tx` as its first
  statement; its transaction and write order are otherwise unchanged, so the
  atomic-compensation invariant in CLAUDE.md (Task Actions) holds.
- `job_recovery.rs:226` and `:601`: the `check_loop_completion` calls are removed.
  `propagate_to_parent` (`:563-624`) is otherwise unchanged. A rollup error now
  propagates out of `on_step_completed` (Decision 18) where `check_loop_completion`'s
  was logged and ignored.
- `job_creator::orchestrate_after_server_step_failure` is unchanged (it calls
  `on_step_completed`).
- `cancellation.rs:36-70` `cancel_job`: `cancel_pending_steps` and
  `cancel_server_managed_steps` move into one transaction that calls `cascade_lock_tx`
  first (Decision 22) and **commits before** the running-step inspection, the
  cancelled-set insert, the event-bus publish, child cancellation and terminal handling
  that follow today (`:76-125`). `JobRepo::cancel` stays a separate autocommitted
  statement before it, so cancellation never holds the job row while waiting for the
  advisory lock. Behavioural consequences, recorded: the two sweeps become atomic (a
  failure of the second now rolls back the first, where today the first would have
  stuck), and the cancelled-set publish is delayed by the lock wait when a cascade is
  in flight.
- `job_recovery.rs:1165` `try_retry_job`: the transaction calls
  `cascade_lock_tx(new_job_id)` as its first statement (Decision 24); its three
  updates are otherwise unchanged.
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
- Instance rows start at `retry_attempt = 0` (DB default), not the placeholder's value.
- Which failure paths honour step retry (§3).

### 4.10 Fixed by construction (Decision 2)

All go live in the activation commit (§10); each has its own regression-test commit
(§9.2):

1. **for_each crash hole** — instance insert and placeholder transition commit together
   (§4.6).
2. **Lost cascade work under concurrency** — advisory lock plus verified, row-locked
   apply (§4.7).
3. **Self-healing rollup and advance** — R5/R6 run on every cascade. Covers the case
   where today's sequential failure path skips rows and then checks terminality against
   a stale snapshot (`job_creator.rs:1282`, `:1307`), leaving the rollup for a later
   keyed call that may never come.
4. **Stale reads cannot be applied** — any write to a job's rows between snapshot and
   verification forces a re-run, and none can land between verification and commit
   (§4.7); today's autocommitted statements apply stale decisions.
5. **Placeholder writes guarded** — a cancelled or timed-out placeholder is never
   overwritten by a later rollup or retirement (§4.6).
6. **Adoption of partial expansions** — placeholders stranded by past crash-hole
   incidents are moved to `running` and roll up normally (R0).

## 5. Data Model

No migration. No new columns. The `for_each` instance/placeholder relationship stays a
naming convention plus `loop_source`, and no trigger or foreign key is added. The
verification vector is read from existing columns in the `SELECT`; nothing is stored.

## 6. Server

### 6.1 New

- `crates/stroem-server/src/cascade.rs` — `Change`, `RollupOutcome`, `Snapshot`, `Plan`,
  `Applied`, `run`, `apply`, `execute`, the moved helpers `parse_for_each_items`,
  `render_for_each_template`, `MAX_FOR_EACH_ITEMS` (with their existing unit tests,
  `job_creator.rs:1979-2060`), and an internal snapshot type.
- `stroem-db` transaction primitives (thin, one statement each):
  `CASCADE_LOCK_CLASS`, `JobRepo::cascade_lock_tx`, `JobRepo::get_status_tx`,
  `JobRepo::mark_running_if_pending_tx`, `JobStepRepo::get_snapshot_vector_for_update_tx`
  (`step_name`, `status`, `output::text`, `error_message` for readable statuses,
  `FOR UPDATE`; the readable-status list is one shared const also used by
  `Snapshot::from_rows`),
  `cancel_pending_steps_tx`, `cancel_server_managed_steps_tx`, `promote_steps_tx`,
  `skip_steps_tx`, `fail_pending_step_tx`, `start_placeholder_tx`,
  `rollup_placeholder_tx`. `create_steps_tx` already exists.

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
deleted call), task retry, hooks, cancellation, recovery.

## 7. Documentation

- `CONTEXT.md` (new): step cascade, placeholder, instance, retirement, adoption,
  rollup, plan, snapshot, change, settlement (pointer to the later design). Links back
  to CLAUDE.md.
- `CLAUDE.md`: link to `CONTEXT.md` in the overview; new `### Step Cascade` section
  (interface, phase order, the verification vector and row locks, the advisory lock
  class and the two-integer rule, lock order, "commit before dispatch", callers, the
  rule that new context variables keep `job` first); rewrite the "Placeholder
  resolution lives inside the orchestrator cascade" paragraph under For-Each Loops and
  the failed-dep note to point at R0/R4/R5/R6; remove the "Do NOT call
  `expand_for_each_steps` after `on_step_completed`" warning; correct the truthiness
  summary under Conditional Flow Steps (`null`/`none` are also false, trimmed,
  case-insensitive).
- `crates/stroem-db/README.md:86`: remove the orchestration API it advertises.
- `docs/internal/TODO.md`: close the in-file TODOs from `job_step.rs:678-690` if tracked;
  add: which failure paths honour step retry is inconsistent (§3); recovery's unguarded
  `mark_failed` can overwrite a `completed` row; remove the `None` workspace-config mode
  (settlement branch); move `build_step_render_context` (candidate 7).
- No user-facing docs change except one line under the `for_each` guide's `sequential`
  section stating that a failed instance stops the loop immediately (R5 behavioural
  change).

## 8. Known Limitations

- `run` depends on `job_creator::build_step_render_context`, and `job_creator` calls
  `cascade::execute`. Module-level mutual reference within the crate; resolved when the
  builder moves (candidate 7).
- The `Option<&WorkspaceConfig>` mode is a test-only concept carried through one more
  branch.
- Rendering can run `vals`; the cascade runs it outside any lock but does not bound it.
- Three verification failures in a row surface as an error from `on_step_completed`.
  The writers that can cause a mismatch are cancellation, recovery timeouts, worker
  claims/completions on other steps of the job, approvals and retries; three in a row
  on one job is pathological and the error is logged with the job id.
- While a cascade holds its step-row locks (verification through commit, a handful of
  statements plus at most one wait on an artifact upload for R7), workers skip that
  job's ready steps and single-statement writers on its step rows wait.

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
  not see it until the next pass
- placeholder: failed dep skips it; cof keeps it; all-deps-skipped skips it; `when`
  false skips; `when` error fails; expression error fails; empty array skips; >10 000
  fails with the exact text; literal JSON array vs Tera string; parallel vs sequential
  instance statuses; instances carry `retry_attempt = 0`; placeholder absent from the
  flow is ignored; `[0]` exists → `Adopt`
- sequential advance: promote next; failure skips all pending, including a pending
  successor of a completed later instance (`[failed, completed, pending]`); cof promotes
  past a failure; no-op while `[i]` is `ready`/`running`; a cancelled middle instance
  without cof skips the rest, with cof promotes past it; failure followed by immediate
  rollup in the same `run`
- rollup: completed array in `loop_index` order with `null` for missing output; failed
  list text; only `failed` fails the loop, `cancelled` does not; no rollup while any
  instance is non-terminal; no rollup for a non-running placeholder; no rollup with
  zero instances; missing flow entry → non-sequential, no cof
- downstream `when` sees an expanded placeholder's rollup output within the same `run`
- termination: a large DAG reaches the fixpoint; a snapshot at fixpoint returns an
  empty plan (idempotency); `assumed` equals the input vector including `version` only
  for context-visible rows
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
- `apply` row-count assertion: a plan whose `Promote` names a row that is not `pending`
  returns `Err` and leaves no partial writes.
- **Verification-mismatch handshake** (no code hook). Fixture: a job where a completed
  step `a` has a pending dependent `b` (so the plan is non-empty). Connection A begins a
  transaction and calls `cascade_lock_tx` for the job; the test spawns `execute` on the
  pool and polls `pg_locks` until a row for that advisory key shows `granted = false`
  (the cascade has computed its plan and is blocked); connection C cancels `b` and
  commits; A rolls back; `execute` verifies, sees the mismatch, re-runs, and its
  returned plan carries no change for `b`; final state has `b` cancelled. A second
  variant has C rewrite `a`'s output with same status (`mark_completed` again) and
  asserts the re-run rendered `b`'s `when` against the new output. (fixes 2, 4)
- **Row-lock protection through the real path**: the job has steps named so that
  `step_name` order is known (`a`, `b`, `c`). Connection A takes `FOR UPDATE` on `c`
  (lexically last) and holds it; the test records A's backend pid. It spawns `execute`
  on the pool for a non-empty plan and polls `pg_stat_activity` joined with
  `pg_blocking_pids()` until some backend is blocked by A's pid (the cascade has taken
  the advisory lock, locked `a` and `b` in order, and is waiting on `c`); it records
  that backend's pid. Connection C then issues a recovery-style `mark_failed` on `a`
  and the test polls until C's backend is blocked by the cascade's pid (behind the row
  it already holds). A commits; the cascade verifies, applies and commits; C's write
  then lands on the committed state. Assert the final state is the serial outcome and
  that C's write did not influence the applied plan. Row-lock waits appear in
  `pg_locks` as transaction-id waits, so the test uses `pg_blocking_pids`, not
  `granted = false` on a tuple lock.
- Concurrency: two `execute`s for the same job run concurrently after two sibling steps
  complete; the join step is promoted exactly once and the final state equals a serial
  run. (fix 2)
- **Cancel vs cascade** (Decision 22): a job with many pending steps; `execute` with a
  non-empty plan and `cancel_job` are started concurrently 50 times; both always
  complete without error (no `40P01` surfaces from either), and the final state is one
  of the two serial outcomes. (fix 2)
- Compensation interleaving: the existing injected approval-dispatch failure fixture
  (`integration_test.rs:25130`, a raw-SQL trigger that raises when the approval step is
  marked suspended) is extended so the trigger first calls
  `pg_advisory_lock(TEST_GATE_CLASS, 1)` and releases it, then raises. Sequence: the
  test holds the gate on connection G; creation runs on the pool: init's cascade
  commits, `handle_approval_steps` fires the trigger, which blocks on G; the test
  observes a backend blocked by G's pid via `pg_blocking_pids`; the test now takes
  `cascade_lock_tx(job)` on connection A; G releases the gate; the trigger raises,
  dispatch fails, compensation begins and blocks on A (observed via
  `pg_blocking_pids`); A releases; compensation completes. Assert the final job is
  `failed` with all non-terminal steps failed, and that an `execute` on the job
  afterwards returns an empty plan. No production hook; the gate lives in the test's
  trigger body.
- **Task-level retry vs cascade** (Decision 24): a task with `retry`, a positive
  delay, and an approval as its root step. Its first job fails (a non-approval step
  fails after the approval is approved). Task retry creates the retry job; the test's
  raw-SQL trigger on the approval step's transition to `suspended` (the same gate
  technique as the compensation test, keyed `(TEST_GATE_CLASS, 2)`) blocks the retry
  job's creation-time approval dispatch, which happens after its creation cascade has
  committed and before `try_retry_job`'s transaction begins (`job_recovery.rs:1145-
  1169`). While dispatch is parked on the gate, connection A takes `cascade_lock_tx(new
  job)`; the gate is released; creation returns; `try_retry_job`'s transaction is
  observed blocked by A (`pg_blocking_pids`); a further `execute` on the new job is
  spawned and observed blocked too; A releases; all complete; the new job's `ready`
  steps carry `retry_at`; no `40P01` is reported by any side.
- **NULL vs empty error**: a failed step `a` with `error_message = NULL` and a
  dependent `b` whose `when` renders `{{ a.error is defined }}`; a plan is computed;
  before it applies, `a.error_message` is set to `''` (same status); verification
  fails, the re-run sees the key present, and `b`'s outcome reflects it.
- Timeout vs rollup: recovery fails a running placeholder by timeout; a later instance
  completion does not overwrite it. (fix 5)
- Cancelled placeholder: rollup leaves it `cancelled`. (fix 5)
- Cancelled instance output: a parallel loop where one instance is `cancelled`; a plan
  is computed; before it applies, the cancelled instance receives an output-only write
  (same status); verification fails and the re-run's rollup array carries the new
  output. (fix 4)
- Adoption: instances exist, placeholder `pending`; one `execute` moves it to `running`
  and, once instances are terminal, rolls it up. (fix 6)
- Self-healing: placeholder `running`, all instances terminal, no prior rollup → one
  `execute` rolls it up. (fix 3)
- R5 behavioural change: `[failed, completed, pending]` → `[2]` skipped, placeholder
  failed, never promoted.
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

- Prerequisite: the fail-or-retry branch is merged, released, and running on every
  replica before the release containing commit 5 is rolled out.
- Pure server change, no migration, no config. Ships as a patch release.
- Commit sequence (each green against the oracle; commits 1–3 change no runtime
  behaviour; commit 4 changes cancellation's and task retry's transaction shape,
  preserving successful-path behaviour, with the atomicity and failure consequences
  described in §4.8):
  1. `cascade::run`, `Snapshot`, `Plan`, `Change`, the internal snapshot type, and the
     `run` unit suite (§9.1 minus adoption, which lands with commit 7). The parser
     helpers stay in `job_creator.rs` and are made `pub(crate)` (`parse_for_each_items`
     `:1155`, `render_for_each_template` `:1181`, `MAX_FOR_EACH_ITEMS` `:933`) so `run`
     can call them.
  2. stroem-db transaction primitives, `CASCADE_LOCK_CLASS`, `cascade_lock_tx`, the
     snapshot-vector queries, and `apply` + its integration tests (transactional,
     timestamps, row-count).
  3. `execute` (lock, verify, retry) + the verification-handshake, row-lock and
     concurrency tests, driven directly against `execute` on fixture jobs.
  4. Creation compensation, cancellation's step sweeps (now one transaction that
     commits before the publish steps) and `try_retry_job`'s transaction call
     `cascade_lock_tx` as their first statement (Decisions 22, 24) + the
     compensation-interleaving, cancel-vs-cascade and task-retry-vs-cascade tests. No
     cascade uses the lock in production yet; the three writers can contend only with
     each other, which they could not before (compensation vs cancellation of the same
     job is now serialised rather than interleaved, an improvement).
  5. **Activation**: `on_step_completed` and the creation-time loop switched to
     `execute`; `check_loop_completion` and its two calls, `expand_for_each_steps`,
     `promote_ready_steps`, `skip_unreachable_steps` and their four DB tests deleted;
     parser helpers and their tests moved into `cascade.rs` (the `pub(crate)` grants go
     away with them); `test_convergence_without_continue_on_failure` rewritten. Fixes
     1–5 go live here.
  6. Regression-test commits, one per fix: self-healing (3), timeout-vs-rollup and
     cancelled placeholder (5), R5 behavioural change, `on_step_completed` vs creation
     equivalence, parent/child.
  7. Adoption: R0 in `run`, `Adopt` in `apply`, unit fixtures and the integration test
     (fix 6).
  8. Docs: `CONTEXT.md`, CLAUDE.md, DB README, TODO.md, `for_each` guide line.
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
| Q8 atomicity | one transaction (rev 1: job row lock; superseded by Q15/Q23/Q24/Q30) |
| Q9 rollup | absorbed as global rules; `check_loop_completion` deleted |
| Q10 repo fns | deleted with their four tests |
| Q11 `None` mode | preserved on this branch |
| Q12 vocabulary | `CONTEXT.md`, cross-referenced from CLAUDE.md |
| Q13 lock sharing | accept sharing the job row lock (superseded by Q23) |
| Q14 phase model | reproduce today's phase boundaries; two contexts per pass; P0 is an extension |
| Q15 rendering vs lock | render outside; verify and apply inside; retry ×3 |
| Q16 retry-aware terminality | superseded by Q28 |
| Q17 placeholder guards | keep stronger guards; preserve column behaviour |
| Q18 sequential advance | global rule (policy restated by Q26) |
| Q20 rollup errors | propagate (abort before settlement) |
| Q22 partial expansions | adopt (R0) |
| Q23 lock | advisory xact lock keyed on job id, not the row lock |
| Q24 verification | whole-snapshot vector (refined by Q29) |
| Q25 retry ownership | superseded by Q28 |
| Q26 sequential failure | failure precedence over successor promotion; stated as a behavioural change |
| Q27 commit sequence | land unused, one activation commit, test commits after, adoption later |
| Q28 retry | no retry logic in the cascade; atomic fail-or-retry is a prerequisite branch |
| Q29 verification vector | job status + every step's status + a digest of output/error for rows whose output/error can be read (context statuses + cancelled); refined by Q34 |
| Q30 verify→apply window | `FOR UPDATE` on all step rows at verification; row-count assertions |
| Q31 cancellation | step sweeps take the cascade lock in one transaction that commits before publish; rule: multi-row `job_step` updates hold the lock or go per row |
| Q32 deadlock | `40P01` inside `execute` counts as a verification failure and re-runs; no victim guarantee |
| Q33 task-level retry | `try_retry_job`'s transaction takes the new job's cascade lock first |
| Q34 values not xmin | verification compares `output::text` and `error_message` as nullable values, exact, no hash, no version-token caveats |

## 12. Review Log

- 2026-09-08 — rev 1 written after the design walk.
- 2026-09-08 — Codex review of rev 1 (session `01a07fd5-bf6b-7551-9d70-14f51079e953`),
  verdict "not ready": phase ordering observable; `run` not pure (`vals`); compensation
  lock-order inversion; job lock does not stabilise steps; missed test caller; plus
  should-fixes on R4/R5/R6 semantics, guards, termination, lock inventory, tests, docs.
- 2026-09-08 — rev 2: phase model, render-outside/verify-touched-rows, advisory lock,
  compensation lock, retry-aware terminality for R5/R6, adoption, R5 keyed-equivalent
  form, guard table, test mapping.
- 2026-09-08 — Codex second pass, verdict "not ready": touched-row verification
  insufficient; retry budget ≠ scheduled retry; R5 not equivalent; measure and P0
  wording; fixture; advisory key collision; commit sequence gaps.
- 2026-09-08 — rev 3: whole-snapshot status verification; effective status with
  `action_type != "task"` ownership; R5 failure precedence; two-integer lock key;
  activation-commit sequence.
- 2026-09-08 — Codex third pass, verdict "not ready": status equality does not imply
  output/error equality (approval message, same-status rewrites); verify→apply window
  unprotected; `action_type` is not a retry-ownership discriminator (approval dispatch
  bypasses, task timeouts do not, cascade-generated failures never retry); R5 "same
  eventual state" too strong; Decision 13 vs R7; test staging and `pub(crate)` gaps.
- 2026-09-08 — rev 4 (this document): retry logic removed from the cascade, atomic
  fail-or-retry made a prerequisite branch (Q28, `2026-09-08-fail-or-retry-design.md`);
  verification vector adds `xmin` for context-visible rows (Q29); verification `SELECT`
  takes `FOR UPDATE` on all step rows, row-count assertions, lock order stated (Q30);
  R5 divergence restated as a behavioural change with a user-doc line; Decision 13
  reworded; column coverage enumerated (§4.2); handshake needs a non-empty plan and a
  `pg_locks` observation, compensation test gets a barrier, `pub(crate)` grants and
  commit numbering fixed (§9.2, §10).
- 2026-09-08 — Codex fourth pass (A and B), verdict "not ready" for both: cancelled
  instance outputs unversioned; data and version read by two queries; cancellation's
  multi-row `UPDATE` can cycle with the ordered row scan; `try_retry_job` contradicts
  the "single-statement" inventory; `xmin` wraparound caveat; compensation fixture
  wrong; row-lock test not through `execute`; B: reject guard lost, history built from
  stale row, `Failed` lacks logging data, "only difference" too strong, `FOR SHARE`
  test weak, replica-wide rollout.
- 2026-09-08 — rev 5 (this document): version predicate includes `cancelled` (Q29
  refined); `get_steps_with_version` reads data and version in one `SELECT`;
  cancellation's sweeps take the cascade lock (Q31, Decision 22) and the multi-row
  rule is stated; `40P01` handled as a mismatch (Q32, Decision 23); lock inventory
  rewritten to enumerate every multi-statement transaction and multi-row statement,
  including `try_retry_job`; `xmin` described as a short-lived optimistic token;
  tests: row-lock through `execute`, cancel-vs-cascade, cancelled-output rollup,
  compensation fixture corrected to the approval-dispatch injection; commit 4 adds the
  cancellation lock; replica-wide prerequisite stated. B revised to rev 2 in its own
  file.
- 2026-09-08 — Codex fifth pass: A "not ready" (`try_retry_job` updates the new job's
  steps after creation committed, job → steps order; Decision 23's victim claim
  unsupportable; retention delete and seeding updates missing from the inventory;
  commit 4 not inert; compensation barrier races dispatch; row-lock test relies on
  tuple-lock `granted = false`; `xmin` freezing claim wrong). B "ready with listed
  changes" (helper visibility, `retry_at` sampling, workspace-outage wording and test).
- 2026-09-08 — rev 6 (this document): `try_retry_job` takes the new job's cascade
  lock (Q33, Decision 24) and the inventory describes its real order; retention and
  seeding inventoried; Decision 23 reworded, no victim claim; verification digest
  replaces `xmin` (Q34), removing the version-token caveats entirely; cancellation's
  sweep transaction commits before publish and its behavioural consequences are
  recorded; commit 4 described honestly; row-lock test uses `pg_blocking_pids` with a
  lexically-last held row; compensation test uses a test advisory gate inside the
  trigger; task-retry-vs-cascade test added. B revised to rev 3.
- 2026-09-08 — Codex sixth pass: A and B both "ready with listed changes", changes
  small enough to apply without another round: the digest conflated `NULL` and `''`
  (renderer distinguishes them) → compare the two nullable values directly; qualify
  the "terminal jobs have no cascade" claim and add the retry-chain-root FK lock;
  commit-4 sentence contradicted §4.8; task-retry test needs a dispatch gate; B's
  outage paragraph cited the stored-input range for the stored-`action_spec` path.
- 2026-09-08 — rev 7 (this document): all six applied. Status moved to Reviewed;
  awaiting owner sign-off, then the implementation plan.
