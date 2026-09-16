# Task-Step Lifecycle Hardening — Design

Status: problem statement, not yet designed
Ships in: unscheduled; after `2026-09-16-cross-workspace-task-actions-design.md`
Builds on: `2026-09-08-cascade-concurrency-hardening-design.md` (deferred; the
per-job advisory lock and row-locked verification vector it designs are the
natural mechanism for the first candidate in § 3 here)

Split out of the cross-workspace task-actions design at its revision 3. That
design's revision 2 pulled in fixes for three inherited gaps in how a
`type: task` step is dispatched, propagated and settled; its review (Codex,
2026-09-16, thread `01a0a91a`) found that each fix needed its own design —
a lock protocol with defined conflict handling, attempt identity for
propagation, real worker-termination semantics, transition-owned hook
delivery. None of the gaps is caused or worsened by a cross-workspace
child; every one applies to same-workspace children today. Line numbers
cite `main` at `f174020`.

## 1. Problem

A `type: task` step is server-managed: `settlement/dispatch.rs::
handle_task_steps_pass` reads the job's ready steps, renders the child's
input, marks the step `running`, creates the child job, and later
`settlement/propagate.rs` writes the child's terminal status back onto the
step. Six things go wrong on that path.

**(i) Dispatch is not exclusive.** Two `advance` calls on one job (a worker
completion and a recovery tick, or two replicas) each read the ready steps
(`dispatch.rs:107-122`) and both pass `status == ready`.
`mark_running_server` is a guarded `UPDATE … WHERE status IN
('pending','ready')` whose `rows_affected` is discarded and which returns
`Ok(())` (`job_step.rs:630-645`); both callers then call
`create_job_for_task_inner` (`dispatch.rs:259-274`) and two children exist
for one step. For a deployment task that is two deployments. The same
stale read lets a dispatcher that loses the race fail the winner's step:
resolution, rendering and default merging happen before any claim, and
the render / default-merge / creation error branches on that stretch go
through `fail_task_step`, whose `mark_failed` has no status guard
(`dispatch.rs:63-92`, `:81`; `job_step.rs:689-710`); the remaining
errors (missing `action_spec`, DB) propagate with `?`.

**(ii) Dispatch is not crash-safe.** The step is marked `running`
(`dispatch.rs:247`) and the job `running` (`:250`) before the child's
transaction commits (`job_creator.rs:411-460`). A crash between the two
leaves a `running` step with no child and no worker. Recovery's phase 2
sees only steps with a `timeout_secs` (`job_step.rs:1083-1093`); the
unmatched-step sweep and worker claiming both exclude server-managed kinds
(`job_step.rs:570`, `:1116`). A crash after the commit but before
`dispatch::init` (`job_creator.rs:486-496`) leaves a child whose root
steps are `type: task` or `type: approval` with no automatic
initialisation recovery — only a job timeout or an operator cancel ends
it.
`init`'s own failure is compensated in a second transaction (`:500-519`)
that fails the committed child; if the compensation errors, the creator
returns `Err` for a child that exists.

**(iii) Cancellation can miss a child.** `Settlement::cancel` stamps the
job (`JobRepo::cancel`, `WHERE status IN ('pending','running')`,
`job.rs:754-767`) and its steps (`job_step.rs:950-964`, `:649-662`), then
enumerates active children (`get_child_jobs`, `job.rs:771-778`, called at
`settlement/mod.rs:748`) and recurses. A dispatcher that read readiness
before the stamp and commits its child after the enumeration's snapshot
produces a running orphan under a cancelled parent. The claim guard does
not look at the job row.

**(iv) A timed-out parent step does not stop its child, and the child's
result overwrites whatever the step became.** Recovery phase 2 fails a
`type: task` step whose `timeout_secs` elapsed via `step_failed` →
`fail_or_retry` with an empty expected-status list (`recovery.rs:117-159`,
`:140`; `job_step.rs:747`) and does not touch the child. When the child
finishes, `propagate` writes the parent step with the unguarded
`mark_completed` / `mark_failed` (`propagate.rs:135-148`,
`job_step.rs:665-710`; only `mark_cancelled` is guarded, `:1042-1057`, and
it discards the row count) and re-advances a job that already settled. With
a step retry configured, `fail_or_retry` resets the same row to `ready`
with `retry_at` (`job_step.rs:754-789`) and dispatch creates a **second**
child for the next attempt (dispatch reads readiness without consulting
`retry_at`, `dispatch.rs:122`); the first child's late completion then
settles the second attempt's step. Status alone cannot tell attempts
apart.

**(v) A job whose task is removed mid-run is never settled.** `Settlement::
resolve` returns `Ok(None)` both when the workspace config is unavailable
and when the config is loaded but the task is gone (`settlement/mod.rs:
136-152`, `:160-167`); for a non-terminal job `advance` returns early
(`:209-211`). No sweep re-enters `advance` for a job with no further step
activity (recovery phases: `recovery.rs:62`, `:117`, `:161`, `:205`,
`:219`). The job stays `running` with all steps terminal until a job
timeout cancels it (selected without the task definition,
`job.rs:884-889`, `recovery.rs:205-214`) or an operator does; its parent
step waits as long. Failing the job is not a one-line fix: the drain gate reads
statuses, not process termination (`has_live_steps`, `job_step.rs:1012-1024`);
`advance`'s terminal branch clears the cancel signal before claiming
(`settlement/mod.rs:261-270`, `:266`) while workers poll only that cache
(`worker_api/jobs.rs:1045-1050`; self-originated NOTIFY is dropped,
`events.rs:375-376`); a worker's later success or failure write is
unguarded (`worker_api/jobs.rs:950`, `:956-963`) and a failure with retries
left re-readies the step under a terminal job, which another worker can
claim (`job_step.rs:561-574`); descendants are not cancelled unless the
`cancel` traversal runs (`settlement/mod.rs:748-753`); `JobRepo::settle`
is pool-only (`job.rs:532-548`); and the unresolvable terminal branch
returns before the log is closed (`settlement/mod.rs:303-305` vs
`terminal.rs:245`).

**(vi) A child suspended at creation fires no `on_suspended` hook.**
`dispatch::init` (pool tier, `dispatch.rs:600-648`) suspends root approval
steps through `handle_approval_steps` (`:315`, `:452`), which cannot fire
hooks; the sweep that does, `fire_initial_suspended_hooks` (state tier,
`:489-565`), is called by the six top-level creation entry points
(`scheduler.rs:439`, `web/hooks.rs:135`, `worker_api/event_source.rs:121`,
`web/api/tasks.rs:546`, `web/api/jobs.rs:806`, `mcp/tools.rs:530`) and by no
child path (`dispatch.rs:277-285` drops `CreatedJob`). Moving the sweep to
`job_created` is routing, not ownership: the sweep fires for **every
currently suspended** step (`:536-539`) while `advance`'s
`dispatch_approvals` fires from a before/after snapshot diff
(`settlement/mod.rs:369-404`, `:418`) — the two can both fire for one step,
and a step approved before the sweep runs is missed. `init` returns
`Result<Option<JobStatus>>` (`dispatch.rs:609`), so descendant ids created
during a partially failed `init` are lost (`job_creator.rs:519` builds a
fresh `CreatedJob`); retry creation commits and initialises before its
linkage transaction (`settlement/retry.rs:140-193`), so a linkage failure
drops the `CreatedJob` before `job_created`; and retry delay applies only
to `ready` steps (`:181-188`), so a retry job's root approval is already
suspended when the delay is decided. Agent-tool children go through
`agent_child_created` with its `BornTerminal` barrier
(`settlement/mod.rs:534-555`) and must keep it.

**(vii) Terminal delivery is not atomic with the claim.** A child's
terminal handling takes the one-shot claim (`metrics_recorded_at`,
`terminal.rs:41-49`, taken at `settlement/mod.rs:270`) and only then writes
the parent step (`:282`). A crash between the two consumes the claim; the
next `advance` on the child loses the claim and never propagates, so the
caller's step stays `running` with a terminal child under it. Reconcile
(`job.rs:814-845`) does not cover it: it looks for a terminal descendant
under a `running` parent step, which this is, but the child's claim is
already spent, so its `advance` skips the terminal actions.

## 2. What the fix must provide

- **One owner per step transition** on the server-managed path: exactly one
  dispatcher moves a step `ready → running`, and only that dispatcher may
  fail it for a pre-creation error; a loser observes that it lost and does
  nothing. Dispatch respects `retry_at` (today it does not,
  `dispatch.rs:122`). Crash between claim and child commit leaves the step
  re-dispatchable, or a sweep repairs it.
- **Defined outcomes for every creation fault boundary**: a child
  committed whose `init` never ran is initialised by a sweep or failed
  with the `[creation]` line; an `init` failure whose compensation also
  fails is retried or surfaced as a stranded-job alarm — in no case does
  the creator return `Err` for a child that exists without the caller's
  step reflecting it.
- **Terminal delivery survives a crash after the claim**: propagation is
  either inside the claim's transaction, or idempotent and re-driven by
  reconcile (paragraph (vii)).
- **Cancellation and dispatch serialise** without a lock-upgrade deadlock
  (`FOR SHARE` on the parent job row followed by an `UPDATE` of it is
  exactly that; Postgres aborts a victim with `40P01`, which the creator
  does not retry — `cascade.rs:938-941` does).
- **Attempt identity** on propagation: a child's result is applied to the
  step only if the step is still on the attempt that created that child;
  a timed-out or retried step's previous child is cancelled.
- **Real termination before terminal handling** when a job is failed out
  from under its workers: the cancel signal stays visible until every
  worker acknowledges (the drain gate's intent, `CONTEXT.md` "Drain gate"),
  descendants are cancelled, worker writes after the job is terminal cannot
  re-ready a step, the log is closed. `Unavailable` (transient reload
  failure) must remain a no-op; `TaskGone` (config loaded, task absent) is
  the case to settle.
- **Transition-owned initial hooks**: `on_suspended` fires exactly once per
  suspension, whether the step suspended during `init` or later, for a
  top-level job, a child, a grandchild, a retry job (respecting retry
  delay) and an agent-tool child; partial-init and retry-linkage failures do
  not lose it. Or: best-effort delivery, stated as such.
- **The agent registration barrier is preserved**: agent-tool children keep
  `agent_child_created` and its `BornTerminal` refusal
  (`settlement/mod.rs:534-555`); nothing here routes them through
  `job_created`.
- Same-workspace and cross-workspace children behave identically.

## 3. Candidate directions (to be designed, not decided)

- **Serialise dispatch under the cascade's per-job advisory lock.** The
  deferred cascade-concurrency design already takes
  `pg_advisory_xact_lock(CASCADE_LOCK_CLASS, hashtext(job_id))` as the
  first statement of `execute`'s apply transaction and chose an advisory
  lock over the job row precisely because artifact upload, cancel, worker
  start and log upload all touch that row
  (`2026-09-08-cascade-concurrency-hardening-design.md` § 2.1). Taking the
  same lock at the start of the child-creation transaction and having
  `cancel` take it before stamping would make dispatch, cascade and cancel
  mutually exclusive per job with no upgrade path and one lock order.
  **Caveat:** the cascade spec deliberately leaves `JobRepo::cancel` as a
  separate autocommitted write *before* the lock is taken
  (`2026-09-08-cascade-concurrency-hardening-design.md:85-90`); moving the
  stamp under the lock changes that contract and must be reconciled with
  the cascade design, not assumed from it. TODO.md #9 ("one write standard
  on `job_step`") concentrates the write surface this attaches to.
- **Claim + child insert in one transaction**, returning a typed
  `DispatchLost` on a zero-row claim; pre-claim failures become
  `fail_if_still_ready` writes (guarded on `status = 'ready'` and the
  attempt), not unconditional `mark_failed`.
- **Attempt column or child id on the step.** `retry_attempt` already
  exists on `job_step` (migration for § Retry Mechanism); propagation can
  guard on `(status = 'running', retry_attempt = $attempt_at_dispatch)`
  and `create_child_job` can stamp the child with the attempt it belongs
  to, so `get_child_jobs_for_step` can distinguish current from stale.
- **`TaskGone` as a cancellation variant**, not a fabricated drain: reuse
  `Settlement::cancel`'s machinery (signal, step stamps, descendant
  traversal, then `advance` and its real drain gate) with a `failed`
  outcome and the `[settlement] task '…' no longer exists` line, plus a
  transaction-capable `settle_tx` and a guard on worker writes when the job
  row is terminal.
- **Suspension as a claimed transition**: the writer that moves a step to
  `suspended` (`mark_suspended`, `dispatch.rs:452`) returns whether it
  transitioned, and the hook fires from that return only — in `init` via
  ids carried on a restructured `CreatedJob`, in `advance` from the
  transition rather than the snapshot diff. Removes the six explicit
  `fire_initial_suspended_hooks` calls.
- **A "re-advance on reload" sweep** for jobs under a workspace that just
  became available, so an `Unavailable` stall ends when the workspace does.

## 4. Tests the design must include

From the cross-workspace review's list: dispatch ownership under barriers
(pending and running parents, same and different steps, a loser failing
during rendering); fault boundaries separated (pre-commit insert failure,
post-commit `init` failure, compensation failure, process loss before
server-managed root init); timeout with step retries (old child completes
after the replacement starts; retry delay; crash between the timeout
transition and child cancellation); `TaskGone` with work executing (two
workers, a nested child, a late failing worker with retries, cancellation
polling on the originating replica, concurrent cancellation); hooks
(`advance` racing the initial sweep, approval resolved before the sweep,
partial-init failure after descendant creation, retry-linkage failure,
retry-job root approvals, the agent barrier); terminal claim taken then
crash before propagation, with the reconcile that must redeliver; a
cross-workspace and a same-workspace variant of each.

## 5. Relationship to other work

- `2026-09-08-cascade-concurrency-hardening-design.md` — deferred; supplies
  the lock. This design should be scheduled with or after it.
- `2026-09-16-cross-workspace-task-actions-design.md` § 4 — lists these gaps
  as carried risks and documents them for cross-team callers.
- TODO.md #9 (one write standard on `job_step`) — makes both cheaper.
- CLAUDE.md § Settlement's "known, unchanged" list and § Task Actions.
