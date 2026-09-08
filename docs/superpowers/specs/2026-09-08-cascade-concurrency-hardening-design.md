# Cascade Concurrency Hardening — Design

**Status:** Reviewed, deferred (2026-09-08). This content was designed and reviewed as
part of `2026-09-08-step-cascade-design.md` revisions 2–7 (six Codex passes, final
verdict "ready with listed changes", applied) and then split out so the cascade branch
stays the size the architecture review argued for. It is the third branch of the
sequence: fail-or-retry → step cascade → this.
**Date:** 2026-09-08
**Builds on:** `2026-09-08-step-cascade-design.md` (rev 8, the minimal-apply cascade).

## 1. Problem

The step cascade (rev 8) applies its plan in one transaction with today's status guards
and rolls back on any guard miss. That closes the for_each crash hole and keeps a plan
internally consistent, but it leaves two pre-existing races exactly as they are today:

- **Lost cascade work.** Two cascades on one job read stale snapshots and race their
  writes; the guards prevent corrupt transitions but a decision one of them should
  have made can be lost until the next trigger (`job_step.rs:684-690`, in-file TODO).
- **Stale template values.** A plan rendered against a step's `output`/`error_message`
  can be applied after a same-status rewrite of that value (an approval message,
  `job_creator.rs:1487-1491`; an unguarded `mark_completed`/`mark_failed` on an
  already-terminal row, `job_step.rs:480-510`). Today's loop has the same exposure
  within one iteration.

Neither has been observed in production. Both are worth closing once the cascade has a
single apply path to attach the protection to.

## 2. Design

### 2.1 Per-job advisory lock

`execute` takes `pg_advisory_xact_lock(CASCADE_LOCK_CLASS, hashtext(job_id::text))`
as the first statement of its apply transaction. Two-`int4` form: Postgres keys it
separately from the single-`bigint` form (`objsubid` 2 vs 1), so it cannot collide
with leader election's `pg_try_advisory_lock(0x5354524D4C445201)` (`leader.rs:26-35`).
`CASCADE_LOCK_CLASS` is a `pub const i32` in stroem-db (`0x5354_5243`, "STRC"); any
future two-integer advisory lock uses a different class. `hashtext` is deterministic
within a database. Two job ids hashing to the same value serialise against each other;
harmless. One helper, `JobRepo::cascade_lock_tx`, is the only place the expression is
written.

Why an advisory lock and not the job row: artifact upload holds the job row
`FOR UPDATE` across its blob write (`artifacts.rs:165-206`); cancellation, worker start
and log upload update the job row. The advisory lock never waits on any of those.

### 2.2 Verification vector, row-locked

Before applying, inside the transaction and after the advisory lock:

```sql
SELECT step_name, status,
       CASE WHEN status IN ('completed','skipped','failed','suspended','cancelled')
            THEN output::text END      AS output_text,
       CASE WHEN status IN ('completed','skipped','failed','suspended','cancelled')
            THEN error_message END     AS error_message
FROM job_step WHERE job_id = $1
ORDER BY step_name
FOR UPDATE;
SELECT status FROM job WHERE job_id = $1;   -- no lock
```

`Plan.assumed` (a `Snapshot` built from the same `JobStepRow`s `run` read, with the
readable-status predicate as one shared const) is compared field by field, `NULL`
distinct from `''` (the renderer distinguishes them, `job_creator.rs:1643-1652`). A
mismatch rolls back and re-runs from a fresh snapshot, at most three times, then
errors. `jsonb` renders canonically, so `output::text` equals
`serde_json::to_string` of the value `run` saw.

The `FOR UPDATE` on every step row of the job holds until commit, so no writer can
change a step row between verification and apply. A step-row guard that then matches
zero rows is a programming error and rolls back. The job-row update of R7 stays
guarded on `pending` and may match zero rows (cancellation between verification and
R7 makes it a no-op).

### 2.3 Writers that must take the lock

A multi-row `UPDATE` acquires row locks in scan order, not the cascade's `step_name`
order, so it can form a lock-wait cycle with the verification scan. Rule, recorded in
CLAUDE.md: a statement updating more than one `job_step` row of a job holds the
cascade lock or is split per row. Writers changed to comply:

- **Creation compensation** (`job_creator.rs:669-680`): `cascade_lock_tx` as its first
  statement; then `fail_non_terminal_steps_tx`, `mark_failed_tx` as today.
- **Cancellation** (`cancellation.rs:36-70`): `cancel_pending_steps` and
  `cancel_server_managed_steps` move into one transaction that takes `cascade_lock_tx`
  first and **commits before** the running-step inspection, cancelled-set insert,
  event-bus publish, child cancellation and terminal handling (`:76-127`).
  `JobRepo::cancel` stays a separate autocommitted statement before it, so
  cancellation never holds the job row while waiting for the advisory lock.
  Consequences: the two sweeps become atomic (failure of the second rolls back the
  first), and the cancelled-set publish is delayed by the lock wait when a cascade is
  in flight.
- **Task-level retry** (`job_recovery.rs:1145-1200`): the transaction that updates the
  new job row (`:1172`, whose `retry_of_job_id` takes a `KEY SHARE` on the retry-chain
  root job row, FK `027_step_retry.sql:20`), the old job row (`:1182`) and the new
  job's `ready` steps (`:1191`, multi-row) takes `cascade_lock_tx(new_job_id)` first.
  It never touches an old-job or root-job step row; a cascade on those jobs takes its
  job row only for R7, guarded on `pending`; `KEY SHARE` conflicts only with key
  updates; the production caller runs task retry for top-level jobs only
  (`:390-397`). No cycle.

### 2.4 Lock order and inventory

Cascade: advisory lock → step rows (`FOR UPDATE`, `step_name` order) → job row (R7).
Compensation, cancellation and task retry: advisory lock first, then as above.

Other multi-statement transactions: job creation and restart seeding
(`create_with_parent_tx`, `create_steps_tx`, `seed_steps_tx`, which inserts then
updates rows it just inserted) — all on rows invisible until commit, no cascade on that
job exists yet; artifact upload — job row then `job_artifact`, never `job_step`;
retention `JobRepo::delete` (`job.rs:859`, `recovery.rs:427`) — job row then step rows
via FK cascade in one statement, on terminal jobs days old; a coinciding cascade is
implausible but not impossible (a late worker report can orchestrate a terminal job),
and if it happens Postgres aborts one side: the cascade re-runs, retention logs and
retries next sweep (`recovery.rs:427-430`). No coordination added.

Single-row writers (worker claim with `SKIP LOCKED`, completion, approval, recovery,
the retry write, agent state, worker start, log upload) skip locked rows or wait on one
row; they hold nothing while waiting.

Legacy multi-row statements (`job_step.rs:775`, `:791`, `:903`, `job_creator.rs:1290`)
were deleted by the cascade branch.

### 2.5 Deadlock as a mismatch

A `40P01` inside `execute` is rolled back and counted as a verification failure.
Postgres chooses the victim; this is defence in depth for a writer the inventory
missed, not a guarantee. The other side's own error path applies.

### 2.6 R7's residual wait

The cascade's one job-row write can wait behind an in-flight artifact upload for the
duration of its blob write, while the cascade holds the step-row locks, so workers
polling that job skip it for the same duration. Accepted.

## 3. Interface changes to the cascade

- `Plan` gains `assumed: Snapshot { job_status, steps: Vec<(String, StepStatus,
  Option<(Option<String>, Option<String>)>)> }`, built by `Snapshot::from_rows`.
- `execute` gains the lock, the verification read and the three-attempt loop; `apply`
  is unchanged except that a step-row guard miss becomes an error rather than a
  re-run trigger (the re-run trigger moves to verification).
- stroem-db: `CASCADE_LOCK_CLASS`, `JobRepo::cascade_lock_tx`, `JobRepo::get_status_tx`,
  `JobStepRepo::get_snapshot_vector_for_update_tx`, `cancel_pending_steps_tx`,
  `cancel_server_managed_steps_tx`.

## 4. Tests

- Verification-mismatch handshake (no code hook): a job where completed `a` has pending
  dependent `b`; connection A begins a transaction and calls `cascade_lock_tx`; the
  test spawns `execute` and polls `pg_locks` until the advisory key shows
  `granted = false`; connection C cancels `b` and commits; A rolls back; `execute`
  verifies, mismatches, re-runs; its plan carries no change for `b`. Variant: C
  rewrites `a`'s output with the same status; the re-run rendered `b`'s `when` against
  the new output. Variant: C sets a failed step's `error_message` from `NULL` to `''`;
  mismatch.
- Row-lock protection through `execute`: steps `a`, `b`, `c`; connection A holds
  `FOR UPDATE` on `c`; `execute` is observed blocked by A's pid via
  `pg_blocking_pids()` (it holds `a`, `b`, waits on `c`); connection C's
  recovery-style `mark_failed` on `a` is observed blocked by the cascade's pid; A
  commits; cascade commits; C lands on the committed state. Row-lock waits show as
  transaction-id waits, so `pg_blocking_pids`, not tuple `granted = false`.
- Concurrency: two `execute`s for one job after two sibling completions; the join
  promoted exactly once; final state equals a serial run.
- Cancel vs cascade: `execute` and `cancel_job` started concurrently 50 times; both
  always complete; no `40P01` surfaces; final state is one of the two serial outcomes.
- Compensation interleaving: the raw-SQL trigger fixture (`integration_test.rs:25130`)
  extended to call `pg_advisory_lock(TEST_GATE_CLASS, 1)` before raising; the test
  holds the gate, observes dispatch blocked on it, takes `cascade_lock_tx`, releases
  the gate, observes compensation blocked on the cascade lock, releases; final job
  `failed`, all non-terminal steps failed; a later `execute` returns an empty plan.
- Task-level retry vs cascade: a retrying task whose root step is an approval; the
  same gate technique parks the retry job's approval dispatch after its creation
  cascade; A takes the new job's cascade lock; the gate releases; `try_retry_job`'s
  transaction and a further `execute` are both observed blocked by A; A releases; all
  complete; `ready` steps carry `retry_at`; no `40P01`.
- Parent/child: a child's cascade and its parent's cascade run concurrently; both
  complete.

## 5. Rollout

One branch after the cascade branch has shipped. Commits: (1) lock helper, snapshot
vector query, `Snapshot`, verification in `execute` + handshake and row-lock tests;
(2) compensation, cancellation and task retry take the lock + their interleaving
tests; (3) CLAUDE.md `### Step Cascade` gains the lock class, the multi-row rule and
the lock order; CONTEXT.md gains "snapshot". Patch release.

## 6. Review Log

- 2026-09-08 — designed as §4.7/§4.8/§9.2 of the cascade spec, revisions 2–7, through
  six Codex passes (advisory lock vs row lock, render-outside, touched-row vs
  whole-snapshot vs value verification, `xmin` rejected twice, cancellation and task
  retry lock order, deadlock victim claim removed, test barriers made deterministic).
  Final Codex verdict on that content: "ready with listed changes", all applied.
- 2026-09-08 — split into this document (cascade rev 8) so the cascade branch matches
  the architecture review's scope. No design change in the split.
