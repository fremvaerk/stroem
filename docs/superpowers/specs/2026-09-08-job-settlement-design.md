# Job Settlement — Design

**Status:** Draft, revision 1 (2026-09-08). Awaiting Codex review.
**Date:** 2026-09-08
**Origin:** architecture review 2026-09-07/08, candidate 2 "Job settlement", entered
through candidate 1 (the step cascade, now on `main`).
**Builds on:** `2026-09-08-fail-or-retry-design.md` and `2026-09-08-step-cascade-design.md`
(both merged to `main`, 3cacd67 and 12e4a2c). The cascade is a finished internal seam
this design calls; it is not reopened here.
**Not covered:** `2026-09-08-cascade-concurrency-hardening-design.md` (advisory lock,
verification vector). It stays a separate later branch; nothing here depends on it and
nothing here makes it harder.

## 1. Problem

What a job owes after one of its steps moves is one procedure: run the cascade, settle
the job if every step is terminal, dispatch server-side steps that became ready, pick
up descendants that settled at creation, fire suspended hooks, and, once the job row is
terminal, wait for its workers to drain, take the exactly-once claim, propagate to the
parent, retry the task or fire hooks, notify, close and archive the log.

Today that procedure is written three times in `crates/stroem-server/src/job_recovery.rs`:
in `orchestrate_after_step` (worker completion, recovery, approval), in the parent leg of
`propagate_to_parent`, and in reduced form in `handle_job_terminal` (cancellation,
creation-time terminal, reconcile, worker `complete_job`). The copies already disagree:

- the parent leg aborts on a task-dispatch error (`?`), the child leg logs and
  continues; the parent leg lacks the approval-failure job log line;
- task-level retry exists only in `orchestrate_after_step`; a job that fails through
  `handle_job_terminal` (settled at creation) never retries;
- no creation path writes `job.max_retries`, so task-level retry is dead in production
  and alive only in tests that seed the column with raw SQL.

The pieces are split by dependency, not by meaning. `orchestrator.rs` and
`job_creator.rs` are pool-only; `job_recovery.rs`, `hooks.rs` and `cancellation.rs`
need `AppState`. The pool-only half cannot call the `AppState` half, which is why the
creator returns `CreatedJob { terminal_at_creation }` and asks nine call sites to
remember `finalize_created_job`, and why `reconcile_settled_children` exists (a recursive
CTE) to find the descendants the creator could not finalize. `CreatedJob` is `Copy`,
its flag is a public `bool`, and dropping it silently loses a job's hooks, completion
metric, archive, notification and parent propagation forever.

Nothing below the integration seam is unit-testable: the claim, the drain gate,
terminal actions, finalize, reconcile and task retry are reached only through the
container suites.

## 2. Goal

One module, `crates/stroem-server/src/settlement/`, that owns everything from "a step
moved" (or "a job was created", or "a job was cancelled") to the last terminal side
effect. Seven entry points. One body, `advance`, replacing the three copies. Two pure
internal seams with unit tests. `CreatedJob` that cannot be acted on outside the
module. The container suites unchanged as the oracle.

Behaviour policy is **preserving**, with these exceptions, each fixed by the module's
shape and each carrying its own regression test (§9):

- D1 unified failure policy across the three copies (§6.4);
- D2 settlement write predicated on the row still being non-terminal (§7);
- D3 task-level retry persisted at creation and decided on every terminal path (§8.1);
- D4 single-action hook jobs built through the shared step builder and finalized
  through the module (§8.2).

The `Option<&WorkspaceConfig>` mode of `on_step_completed` is removed (§5.2).

## 3. Vocabulary

Added to `CONTEXT.md` (the two existing entries are sharpened):

- **Settlement** — deciding a job's terminal status from its steps once every step is
  terminal: any untolerated `failed` → `failed`; else any `cancelled` → `cancelled`;
  else `completed` with the aggregated output of the flow steps nothing depends on.
- **Terminal handling** — the once-only side effects after a job row is terminal:
  propagation to the parent step, task-level retry or hooks, completion notification,
  log close and archive.
- **Claim** — the exactly-once gate on terminal handling: the `metrics_recorded_at`
  compare-and-set. Fail-closed.
- **Drain gate** — no terminal handling while a step of the job is `running` or
  `claimed`. Fail-open.
- **Reconcile** — finding descendants that settled at creation under a parent step that
  is still `running`, and advancing them. Bounded by task depth.
- **Advance** — the settlement module's one body: move a job as far as its rows allow,
  from cascade through terminal handling.

## 4. Module layout

```
crates/stroem-server/src/settlement/
  mod.rs        Settlement struct, the seven entries, advance()      (state tier)
  settle.rs     decide() pure + settle_if_all_terminal()            (pool tier)
  dispatch.rs   task-step and approval-step dispatch, init()        (pool tier)
  terminal.rs   drain gate, claim, TerminalPlan (pure), run()       (state tier)
  propagate.rs  child → parent step, agent registration barrier     (state tier)
  retry.rs      task-level retry job creation and linking           (state tier)
  hooks.rs      moved from src/hooks.rs; hook job creation          (state tier)
```

Deleted: `src/job_recovery.rs`, `src/orchestrator.rs`, `src/hooks.rs` (moved).
`src/cancellation.rs` keeps only the cancelled-jobs set (`is_cancelled`,
`clear_cancelled`, the insert used by cancel) and its ten unit tests; `cancel_job` moves
to the module. `src/job_creator.rs` keeps job creation and calls the pool tier for its
post-commit init block. `src/cascade.rs` is untouched.

The **pool tier** is free functions taking `&PgPool` and explicit workspace arguments.
It is what the creator and the ninety pool-only tests call. The **state tier** is the
`Settlement` struct. It calls down into the pool tier and adds everything that needs
`AppState`: the claim, hooks, archive, notification, the cancel signal.

## 5. Pool tier

### 5.1 `settle.rs`

```rust
pub struct Settled { pub status: JobStatus, pub output: Option<serde_json::Value> }

/// Pure. `None` while any step is non-terminal.
pub fn decide(task: &TaskDef, steps: &[JobStepRow]) -> Option<Settled>;

/// Reads the steps, calls `decide`, writes with `JobRepo::settle` (§7).
/// Returns the status written, or the status already on the row when the
/// predicated write matched nothing (an explicit cancellation), or `None`
/// while a step is live.
pub async fn settle_if_all_terminal(pool: &PgPool, job_id: Uuid, task: &TaskDef)
    -> Result<Option<JobStatus>>;
```

`decide` is the body of today's `orchestrator::settle_if_all_terminal` after its reads:
`flow_name` strips `[i]`, tolerated failures come from `continue_on_failure`, the output
is aggregated from terminal flow steps nothing depends on. Unit tests cover: all
completed; untolerated failure; tolerated failure with aggregated output; cancelled
without failure; failed beats cancelled; a live step → `None`; instance rows map to
their placeholder's flow step; empty flow.

### 5.2 `cascade_and_settle`

```rust
/// Replaces `orchestrator::on_step_completed`. The workspace config is
/// required: production always has one, and the `None` mode is gone.
pub async fn cascade_and_settle(pool: &PgPool, job_id: Uuid, task: &TaskDef,
    workspace_config: &WorkspaceConfig) -> Result<Option<JobStatus>>;
```

Body: `cascade::execute(pool, job_id, task, Some(workspace_config))` then
`settle_if_all_terminal`. (`cascade::execute` keeps its `Option` parameter; that is the
cascade's own interface and out of scope. The module always passes `Some`.)

Tests that passed `None` (19 in `orchestrator_test.rs`, 39 in `integration_test.rs`)
pass a config built by a test helper `workspace_with(&task) -> WorkspaceConfig`
(default config with the task inserted under its name). The helper is added to each
test file that needs it; there is no shared test module in `crates/stroem-server/tests/`.

### 5.3 `dispatch.rs`

`handle_task_steps`, `handle_task_steps_pass`, `fail_task_step`,
`handle_approval_steps` and `fire_initial_suspended_hooks` move here from the creator
with their signatures unchanged, except that `orchestrate_after_server_step_failure`
becomes a call to `cascade_and_settle`. The approval-render failure branch goes through
`fail_task_step` too (today it calls `mark_failed` + re-orchestrate inline; same effect,
one site).

```rust
/// The creator's post-commit init block. Returns the status if the job
/// settled during initialisation.
pub async fn init(pool: &PgPool, workspaces: &WorkspaceManager,
    workspace_config: &WorkspaceConfig, workspace_name: &str, job_id: Uuid,
    task: &TaskDef, defaults: JobDefaults) -> Result<Option<JobStatus>>;
```

Body, in this order: `cascade::execute` → `handle_task_steps` →
`handle_approval_steps` → `settle_if_all_terminal`. The creator calls `init` and keeps
its compensation on `Err` exactly as today (fail live steps and mark the job failed in
one transaction; `terminal_at_creation: true`). Compensation stays in the creator
because it belongs to the creation transaction's contract, not to settlement.

## 6. State tier

### 6.1 The struct

```rust
pub struct Settlement {
    pool: PgPool,
    workspaces: Arc<WorkspaceManager>,
    defaults: JobDefaults,
    log_storage: Arc<LogStorage>,
    log_broadcast: Arc<LogBroadcast>,
    job_completion: Arc<JobCompletionNotifier>,
    cancelled_jobs: Arc<RwLock<HashSet<Uuid>>>,
    event_bus: EventBus,
}

impl AppState {
    /// Cheap: clones of `Arc`s and a `PgPool` handle.
    pub fn settlement(&self) -> Settlement;
}
```

Seven of `AppState`'s eighteen fields; nothing else in the cluster reads the rest
(`leader`, `acl`, `tarball_cache`, state and artifact storage). Built on demand from
`&AppState` rather than stored as a field: storing it would make every `AppState`
literal in five test files construct it first, and the module's server-log helper
(`append_server_log`'s two calls, log storage plus broadcast) would otherwise need
`AppState` back. `Settlement::server_log(job_id, msg)` is the module's private copy of
that helper; `AppState::append_server_log` stays for the other callers.

No trait. One adapter would make the seam hypothetical; tests go through Postgres like
the rest of the server.

### 6.2 The seven entries

```rust
impl Settlement {
    /// A step of `job_id` reached a terminal status (or was reset by an agent
    /// tool result). Replaces `job_recovery::orchestrate_after_step`.
    pub async fn step_settled(&self, job_id: Uuid, step_name: &str) -> Result<()>;

    /// Fail-or-retry the step, then `step_settled` when the outcome is `Failed`.
    /// Replaces the `fail_step` + `orchestrate_after_step` pair at the seven
    /// failure sites. `RetryScheduled` and `NotApplied` do not advance.
    pub async fn step_failed(&self, job_id: Uuid, step_name: &str, error: &str,
        expected: &[StepStatus]) -> Result<FailOutcome>;

    /// Consume the creation result. Replaces `finalize_created_job`.
    pub async fn job_created(&self, created: CreatedJob);

    /// Agent tool child: reconcile its subtree, then reject it if it is
    /// terminal. Never finalizes (the registration barrier, §6.5).
    pub async fn agent_child_created(&self, created: CreatedJob)
        -> Result<Uuid, BornTerminal>;

    /// The worker persisted `agent_state` for `step_name`: replay propagation
    /// for every pending tool child that is already terminal.
    pub async fn agent_children_registered(&self, job_id: Uuid, step_name: &str);

    /// Replaces `cancellation::cancel_job`.
    pub async fn cancel(&self, job_id: Uuid) -> Result<CancelResult>;

    /// Local-mode worker reported the whole job complete. Predicated settle
    /// write (§7), then advance.
    pub async fn worker_completed_job(&self, job_id: Uuid,
        output: Option<serde_json::Value>) -> Result<()>;
}

pub struct BornTerminal { pub job_id: Uuid, pub status: String }
```

Callers after the switch:

| entry | replaces | call sites |
|---|---|---|
| `step_settled` | `orchestrate_after_step` | worker `complete_step` (success path), approve; recovery phases where the outcome is not a failure write |
| `step_failed` | `fail_step` + `orchestrate_after_step` | worker `complete_step` (failure), claim-time render failure, four recovery phases, approval reject |
| `job_created` | `finalize_created_job` | `web/api/tasks.rs`, `web/api/jobs.rs` (restart, rerun), `web/hooks.rs`, `web/worker_api/event_source.rs`, `mcp/tools.rs`, `scheduler.rs`, `event_source.rs`, `settlement/hooks.rs`, `settlement/retry.rs` |
| `agent_child_created` | `reconcile_settled_children` + inline check | `web/worker_api/jobs.rs::agent_task_tool` |
| `agent_children_registered` | inline loop over `propagate_to_parent` | `agent_save_state`, `agent_suspend_step` |
| `cancel` | `cancel_job` | `web/api/jobs.rs`, `mcp/tools.rs`, `recovery.rs`, `scheduler.rs`, `event_source.rs` (three) |
| `worker_completed_job` | `mark_completed` + `handle_job_terminal` | `web/worker_api/jobs.rs::complete_job` |

Missing job or missing workspace: `step_settled` logs and returns `Ok(())` as
`orchestrate_after_step` does today. `job_created` and `agent_children_registered` are
best-effort and return `()`. `handle_job_terminal`, `propagate_to_parent`,
`reconcile_settled_children`, `claim_terminal_handling`, `drained_for_terminal_handling`
and `run_terminal_job_actions` are no longer public; nothing outside the module can
reach a terminal side effect without the claim.

### 6.3 `advance`

```rust
/// Move `job_id` as far as its rows allow. Idempotent: every effect is
/// guarded (cascade guards, predicated settle, drain gate, claim).
async fn advance(&self, job_id: Uuid) -> Result<()>;
```

1. Read the job row; missing → warn, `Ok(())`.
2. Resolve workspace and task: `workspaces.get_config(job.workspace)`; task from
   `config.tasks`, else `build_minimal_task_def` for `hook` and `event_source`
   source types (moved from `job_recovery.rs`), else warn and `Ok(())`. A missing
   workspace logs `[orchestration] workspace '…' not loaded` to the job and returns
   `Ok(())`.
3. **While the job row is non-terminal** (`pending` or `running`):
   a. `cascade_and_settle(pool, job_id, task, config)`.
   b. `dispatch::handle_task_steps(...)`; on `Err` log at `error!` and write
      `[orchestration] Failed to handle task steps: …` to the job; continue (D1).
   c. reconcile: `JobRepo::get_settled_descendants_with_running_parent_step(job_id)`,
      deepest first, `Box::pin(self.advance(descendant))` for each.
   d. snapshot suspended steps; `dispatch::handle_approval_steps(...)`; on `Err` log
      and write `[orchestration] Failed to handle approval steps: …`; continue (D1);
      re-read; `fire_suspended_hooks` for every newly suspended step and write
      `[approval] Step '…' waiting for approval`.
   e. re-read the job row.
4. **If the job row is terminal** (`completed`, `failed`, `cancelled`, `skipped`):
   a. drain gate: `JobStepRepo::has_live_steps`; live → `Ok(())`. Query error →
      proceed (fail-open, as today).
   b. `cancellation::clear_cancelled(job_id)`.
   c. claim: the `metrics_recorded_at` CAS; on win increment
      `STROEM_JOBS_COMPLETED_TOTAL{status}`; not won → `Ok(())`; error → `error!`
      plus `[orchestration] terminal handling claim failed: …` and `Ok(())` (fail-closed,
      as today).
   d. `terminal::plan(&job, task, retry_budget)` (§6.6) and run it in order:
      i.  propagate (§6.5) when `parent_job_id` is set; errors are logged and never abort
          the rest (the claim is consumed).
      ii. retry when the plan says so: `retry::create_retry_job` (§8.1); on success write
          the task-retry log line, `upload_to_archive` and `job_completion.notify`, then
          return (hooks fire only after retries are exhausted, as today). On failure fall
          through to hooks (as today).
      iii. `hooks::fire_hooks`; if this job is itself a hook job that failed, write the
          failure summary to the originating job parsed from `source_id`.
      iv. `job_completion.notify(job_id, status)`.
      v.  `log_storage.close_log`, then spawn `upload_to_archive` (after hooks, so server
          events are in the archive).
5. `Ok(())`.

Step 3 is skipped by the status check for a job that is already terminal when
`advance` is entered, which is exactly today's `handle_job_terminal`. Cancel calls
`advance` unconditionally: for a cancelled job with live workers the drain gate returns
early, which is today's "only if no running steps" condition made structural.

Recursion: `advance` → `propagate` → `advance(parent)` and `advance` → reconcile →
`advance(descendant)` are boxed and bounded by `MAX_TASK_DEPTH` in each direction, as
today.

### 6.4 D1: one failure policy

Inside step 3, a task-dispatch or approval-dispatch error is logged, written to the
job's log, and does not abort. Today the parent leg of `propagate_to_parent` returns the
task-dispatch error with `?`, which skips the parent's drain, claim and terminal
actions; since the claim would then never be taken by anyone, those actions are lost.
Regression test: a parent whose `type: task` step dispatch fails (unknown task name)
still settles, fires its `on_error` hook and increments the completion counter once.

### 6.5 `propagate.rs`

```rust
/// Child `child` settled; mark parent step `(parent_job_id, parent_step)` and
/// advance the parent. `agent_tool` children are subject to the registration
/// barrier: `agent_state` absent, or not listing this child, → return without
/// touching the step.
async fn propagate(&self, child: &JobRow, parent_job_id: Uuid, parent_step: &str)
    -> Result<()>;
```

Body: today's `propagate_to_parent` up to and including the parent-step write
(`agent_tool` branch: resolve the tool call, `update_agent_state`, and the
`status='ready'` reset when all calls resolved; otherwise `mark_completed` /
`mark_cancelled` / `mark_failed` on the parent step), then `Box::pin(self.advance
(parent_job_id))`. The parent-side copy of the twelve steps is gone.

`agent_children_registered` re-reads the step's `agent_state`, and for every pending
child that is already terminal calls `propagate` directly (not `advance(child)`: the
child's claim was consumed by its own settlement, as today).

`agent_child_created` runs reconcile on the child's subtree (step 3c against
`created.job_id`), re-reads the child, and returns `Err(BornTerminal)` when the flag is
set or the child row is terminal; otherwise `Ok(created.job_id)`. It never calls
`job_created`.

### 6.6 `terminal.rs` — the plan

```rust
pub enum HookKind { Success, Error, Cancel, None }

pub struct TerminalPlan {
    pub propagate: bool,          // parent_job_id.is_some()
    pub retry: bool,              // failed && top-level && attempt < max
    pub hooks: HookKind,          // by status, None when retry is true
}

/// Pure.
pub fn plan(job: &JobRow) -> TerminalPlan;
```

Unit tests: failed top-level with budget → retry, no hooks; failed top-level exhausted
→ error hooks; failed child with budget → propagate, no retry (children never retry,
as today); cancelled → cancel hooks; completed → success hooks; `max_retries` NULL →
no retry. `HookKind::None` on a retry plan is what makes "hooks only after retries are
exhausted" a property of the plan rather than of control flow. The hook chain-depth
guard and the hook selection stay inside `hooks.rs`, already pure and tested.

### 6.7 `cancel`

Today's `cancellation::cancel_job` body, unchanged in order: exists check →
`JobRepo::cancel` (`false` → `AlreadyTerminal`) → `cancel_pending_steps` →
`cancel_server_managed_steps` → if any step is `running`, insert into `cancelled_jobs`
and `event_bus.publish_job_cancelled` → `Job cancelled by user` log line (wording unchanged) →
`Box::pin(self.cancel(child))` for every child job → `self.advance(job_id)`. The last
call is unconditional (today: only when no step is running); the drain gate inside
`advance` returns early in exactly that case, so the observable behaviour is the same and
the condition lives in one place.

## 7. D2: predicated settlement write

```rust
impl JobRepo {
    /// `UPDATE job SET status=$2, output=COALESCE($3, output), completed_at=NOW()
    ///  WHERE job_id=$1 AND status IN ('pending','running')`.
    /// Returns whether the row was written.
    pub async fn settle(pool: &PgPool, job_id: Uuid, status: JobStatus,
        output: Option<serde_json::Value>) -> Result<bool>;
}
```

`settle_if_all_terminal` and `worker_completed_job` use it. The "never overwrite an
explicit cancellation" re-read in today's decider goes away; a `false` return means the
row was already terminal and the caller reports the status it re-reads. `mark_completed`,
`mark_failed`, `mark_cancelled` stay for their other callers (compensation inside the
creation transaction via `mark_failed_tx`, and tests). `JobRepo::cancel` keeps its own
predicate. Regression test: `settle` against a `cancelled` row returns `false` and the
row stays `cancelled` with its original `completed_at`.

Migration: none. `metrics_recorded_at` and the retry columns already exist.

## 8. The other construction-fixed defects

### 8.1 D3: task-level retry

**Persistence.** `create_job_for_task_inner` writes `job.max_retries = task.retry
.max_attempts - 1` in the creation transaction, for every `CreationMode`, whenever the
task defines `retry`. `JobRepo::create_with_parent_tx` gains a `max_retries: Option<i32>`
parameter (its wrappers pass `None`; the creator passes the value). Retry jobs get their
`retry_of_job_id`, `retry_attempt`, and the original's `retry_job_id` back-link from
`retry::create_retry_job`'s linking transaction, as today; that transaction no longer
needs to write `max_retries` because creation did.

**Coverage.** The retry decision is `TerminalPlan.retry`, computed in step 4d for every
path into terminal handling: worker completion, propagation, creation-time terminal,
reconcile, cancellation (never `failed`, so never retries), worker `complete_job`.

`retry::create_retry_job(&self, job, workspace, task) -> Result<CreatedJob>` is
today's `try_retry_job` minus the finalize call; `advance` passes the result to
`job_created`.

Regression tests: (1) a task with `retry: { max_attempts: 2 }` whose only step fails
through the worker path produces a retry job with `source_type = "retry"` and
`retry_of_job_id` set, with **no** raw-SQL seeding of `max_retries`; (2) the same task
with its only root step failing at creation (a `type: task` step naming an unknown task)
also produces a retry job. The five existing task-retry tests keep passing unchanged.

### 8.2 D4: hook jobs through the shared step builder

Today `fire_single_hook` builds a hook job with `JobRepo::create` plus a hand-written
`NewJobStep` (timeout `None`, retry fields `None`, `action_workspace` `None`), and only
`type: task` hooks go through the creator.

The creator's step construction (the `NewJobStep` literal in `create_job_for_task_inner`,
`job_creator.rs:499`) is extracted to `pub(crate) fn build_step(job_id, flow_step,
action, action_spec, input, status, defaults, action_workspace, action_revision) ->
NewJobStep`, used by the creator's loop and by the hook path. `fire_single_hook`
synthesizes `FlowStep { name: "hook", action: hook.action, .. }` and calls
`build_step` with the already-rendered hook input as the step's literal input and
`Ready` status, inserts the job and step in one transaction, and passes
`CreatedJob::new(job_id, false)` to `job_created`. The hook payload is **not**
re-rendered: it goes in as the step's input verbatim, which is why the hook path does not
run the creator's template loop or the cascade (a one-step flow with a `Ready` root has
nothing for the cascade to do).

This differs from the "synthesized task through the full creator" wording in the design
walk: the full creator renders step inputs against the job input, and hook payloads are
already rendered and may contain braces (error messages), so re-rendering them is
wrong. The shared builder achieves the goal — one step-construction site, hook jobs on
the same lifecycle — without that risk.

Regression test: a hook whose action defines `retry: { max_attempts: 3 }` on a server
with `default_step_timeout: 30s` produces a hook job step with `max_retries = 2` and
`timeout_secs = 30`.

## 9. `CreatedJob`

```rust
#[must_use = "pass to Settlement::job_created or agent_child_created"]
pub struct CreatedJob { pub job_id: Uuid, terminal_at_creation: bool }

impl CreatedJob {
    pub(crate) fn new(job_id: Uuid, terminal_at_creation: bool) -> Self;
}
```

Not `Copy`, not `Clone`. The struct is defined in `settlement/mod.rs`; the creator
constructs it through `new` and cannot read the flag. Only `job_created` and
`agent_child_created` read it, and both take the struct by value. `create_job_for_task` and
`create_child_job_for_task` (the `_id`-returning wrappers "kept for tests") are deleted;
their test callers use the `_detailed` variants and ignore the flag with `let _ =`
explicitly, or are converted to `job_created`.

Residual hole: `create_job_for_task_detailed(..).await?.job_id` moves the id out and
drops the struct. `#[must_use]` does not catch field access. This is documented on the
struct and is the one place reviewer discipline still applies.

## 10. Documentation

- `CONTEXT.md`: the six terms in §3.
- `CLAUDE.md`: one `### Settlement` section (module layout, the seven entries, the
  `advance` order, drain-before-claim, the reconcile predicate, `CreatedJob`
  obligation, task retry now functional, hook jobs via `build_step`, the `None` mode
  gone). The settlement paragraphs now inside `### Task Actions`, `### Prometheus
  Metrics` and `### Agent Actions` are replaced by one-line pointers to it. The
  `### Retry Mechanism` "task-level retry is non-functional" bullet is replaced by the
  persistence rule.
- `crates/stroem-db/README.md`: `JobRepo::settle`, the `max_retries` creation parameter.
- `docs/internal/TODO.md`: the task-retry entry marked done; the hardening spec entry
  kept; add the residual `CreatedJob` hole.
- `docs/src/content/docs/guides/retry.md` already documents task-level retry as
  working, with no caveat; it needs no change. D3 makes the guide true.

## 11. Tests

Policy: the container suites (`integration_test.rs` 340, `orchestrator_test.rs` 37,
`propagate_to_parent_test.rs` 5, `metrics_test.rs` 18, `restart_integration_test.rs`
22, `rerun_integration_test.rs` 6, `ha_test.rs` 22, `cascade_apply_test.rs` 11) stay
untouched as the oracle, except for the mechanical call-site changes in §5.2 and §6.2
and the deletion of the two `_id`-returning creation wrappers. A final pruning commit
may delete container tests made redundant by the unit tests below; each deletion names
the unit test that replaces it.

New unit tests (in-module):

- `settle::decide` — the eight cases in §5.1.
- `terminal::plan` — the six cases in §6.6.

New regression tests (container):

- D1 parent dispatch failure still fires hooks and counts once.
- D2 `settle` against a cancelled row is a no-op.
- D3 two retry-job tests without raw-SQL seeding.
- D4 hook job step carries action retry and default timeout.
- `CreatedJob` privatization: compile-level; no test.
- `agent_child_created` and `agent_children_registered`: covered by the existing
  `test_agent_task_tool_rejects_*` and `test_agent_save_state_replays_*` tests through
  the worker API, unchanged.

Full verification before merge: `cargo fmt --check --all`, `cargo clippy --workspace
-- -D warnings`, `cargo test --workspace` under `DOCKER_HOST=unix:///Users/ala/.orbstack
/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true CARGO_INCREMENTAL=0`.

## 12. Order of work

One branch (`worktree-refactor-settlement`), structure before defects, container suites
green after every task:

1. Pool tier: `settlement/settle.rs` (`decide` + tests, `settle_if_all_terminal`),
   `cascade_and_settle`, the `workspace_with` test helper, sixty call sites. Delete
   `orchestrator.rs`.
2. `settlement/dispatch.rs`: move dispatch and `init`; creator calls `init`.
3. `settlement/terminal.rs` (`plan` + tests, drain, claim, run), `propagate.rs`,
   `retry.rs`, `hooks.rs` moved; `Settlement` struct, `advance`, the seven entries;
   switch every caller; delete `job_recovery.rs`; `cancellation.rs` trimmed.
4. `CreatedJob` privatization; delete the `_id` wrappers.
5. D1 with its test.
6. D2: `JobRepo::settle` with its test; decider and `worker_completed_job` switched.
7. D3: `max_retries` at creation, `create_with_parent_tx` parameter, two tests.
8. D4: `build_step` extraction, hook path, test.
9. Docs (§10), TODO, pruning commit.

Process: subagent-driven development with a task review after every task and a
whole-branch review at the end, then a Codex implementation review; findings from both
are fixed and re-reviewed before the branch is offered for merge. No AI co-author
trailers in commits.

## 13. Out of scope

- The cascade's `Option<&WorkspaceConfig>` parameter (cascade interface).
- The hardening branch (advisory lock, verification vector, `try_retry_job` lock order).
- Approval-retry wedge, late-report guard (TODO.md).
- Merging `hooks::is_top_level_source` (allow-list of eight) with
  `web/api/jobs.rs::is_top_level_job` (deny-list plus parent check). They differ on
  purpose: restart eligibility is a wider set than workspace-hook fallback. Both stay,
  each keeps its doc comment.
- The scheduler's `create_skipped` rows (never a `CreatedJob`, never fire hooks; unchanged).

## 14. Risks

- **Scope of the caller switch.** About sixty test call sites and around thirty
  production call sites change in tasks 1 and 3. Mechanical, but the two tasks are the
  largest diffs; the reviewer gets the full package, not `HEAD~1`.
- **Behaviour drift hidden by D1.** Making the parent leg log-and-continue changes
  when a parent's terminal actions run in a case that today loses them. The regression
  test pins the new behaviour; the old was a defect, not a contract.
- **Hook job creation.** D4 changes the insert path for every hook job. The eleven hook
  tests are the oracle; `test_hook_job_completes_through_orchestrator` in particular
  exercises the created step end to end.
- **`#[must_use]` residual hole** (§9). Accepted and documented.
