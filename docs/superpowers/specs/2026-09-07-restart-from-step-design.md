# Restart From Step — Design

**Status:** Revised after Codex review (rev 2, 2026-09-07) — pending user approval
**Date:** 2026-09-07
**Builds on:** `docs/internal/2026-04-28-rerun-prefill-design.md` (Re-run prefill & job
lineage — "feature A"; this document is the deferred "feature B")

## 1. Problem

A long pipeline fails at step 6 of 9. Today the only recovery is **Re-run**, which
starts a brand-new job from step 1 and repeats every upstream step, however expensive
and however unrelated to the failure. On 2026-09-07 the `jobs/recalc-pipeline` task was
re-run four times because one late `type: task` step could not be dispatched; every
re-run repeated ~2 minutes of upstream AI work that had already succeeded.

The April 2026 lineage design reserved the data model for this (`source_type = 'restart'`,
`job.source_job_id`, `job.restart_from_step`) but explicitly deferred the executor and UI
behaviour. Nothing writes `restart_from_step` today
(`crates/stroem-server/src/job_creator.rs:333` passes `None`).

## 2. Decisions

| Question | Decision (2026-09-07) |
|---|---|
| Which steps can be restart points | **Any** flow step (completed, failed, skipped, cancelled), except `for_each` *instance* rows |
| Job input | **Same values as the source job, no form** — replayed from `raw_input` through the normal pipeline so connections/defaults are re-resolved (rev 2, see §4.4) |
| Workspace revision | **Current** live revision (same as Re-run), not the source job's |
| Steps outside the restart set that did not complete | **Carried over as they ended** (failed stays failed) |

## 3. Non-Goals

- Editing input on restart (use Re-run).
- Pinning the source job's revision. The tarball cache only holds revisions still on
  disk (`web/worker_api/workspace.rs:130-136` → 404 otherwise), so pinning cannot be
  made reliable without a git-checkout-by-OID path.
- Resuming *in place* on the same job row. Rejected: overwrites logs/outputs, fires
  hooks and metrics twice, collides on per-job artifact names, and breaks the
  "terminal job is immutable" assumption everywhere.
- Restarting a single `for_each` instance (`step[3]`). Restart from the placeholder.
- Copying artifacts or state snapshots from the source job (see §8).
- Restarting non-terminal jobs.
- Restarting legacy jobs with `raw_input = NULL` (same 400 as Re-run; see §4.4).

## 4. Semantics

### 4.1 Restart set

Given source job `S`, its task's **current** flow `F`, and chosen step `x`:

```
roots       = { x } ∪ { s ∈ F | S has no job_step row named s }   // steps added since S ran
restart_set = roots ∪ transitive_dependents(F, roots)
carry_set   = F \ restart_set
```

The closure is over **all roots**, not just `x` (rev 2). A step inserted upstream of an
existing step must pull that existing step into the restart set; otherwise the new step
reruns but its dependent is carried over as terminal and `promote_ready_steps` never
reconsiders it (`crates/stroem-db/src/repos/job_step.rs:633-672` only touches `pending`
rows).

- Dependents are computed on `F`; `depends_on` is the only edge type; `for_each`
  instances are not in `F`.
- Steps present in `S` but absent from `F` are dropped silently (current flow is
  authoritative, as with Re-run).
- `x` must be a key of `F`; `x` containing `[` is rejected (loop instance).
- The whole current flow is resolved and pre-checked exactly as for a normal job
  (`job_creator.rs:183-243`), **including carried-over rows**. An unavailable owner
  workspace or removed action for a carried-over cross-workspace step therefore blocks
  the restart with the usual 400. Chosen deliberately (rev 2): strict whole-flow
  validation keeps one code path and matches Re-run; the cost is that a broken action you
  were not going to run can still block you. Carried rows keep the *current* action
  metadata (`action_spec`, `action_workspace`, `action_revision`); provenance of their
  copied output is the source job, reachable via `job.source_job_id`.

### 4.2 Carried-over steps

For every `s ∈ carry_set` the new row is created by the normal creator, then overwritten
in the same transaction:

| new row field | value |
|---|---|
| `status` | source status if terminal (`completed`/`failed`/`skipped`/`cancelled`); **any non-terminal source status** (`pending`, `ready`, `claimed`, `running`, `suspended` — possible when `S` was cancelled mid-flight) → `cancelled` with `error_message = "carried over from cancelled source job"` |
| `output` | source `output` verbatim (aggregated array for a `for_each` placeholder) |
| `error_message` | source value verbatim |
| `completed_at` | `NOW()` |
| `carried_over` | `true` (new column, §5) |
| `ready_at`, `retry_at`, `started_at`, `worker_id`, `agent_state`, `suspended_at` | `NULL` (explicitly cleared — the creator may have set `ready_at` on a root row) |

Every seeding UPDATE must affect exactly one row; anything else aborts the transaction.

**Output reuse is promised only for `completed` carried rows.** The server render
context exposes `skipped` as `{output: null}`, `failed` as `{output: null, error}` and
omits `cancelled` (`job_creator.rs:1341-1378`), and the worker claim path builds its
context from `completed` rows only (`web/worker_api/jobs.rs:509-518`). That is today's
behaviour for steps that actually ran; carried rows inherit it unchanged.

`for_each` instance rows are **not** recreated (templates only read the placeholder,
`job_creator.rs:1344-1346`). A carried `type: task` step keeps the output
`propagate_to_parent` stamped on it (`job_recovery.rs:439-445`); no child job is recreated.

### 4.3 Restart-set steps and settlement

Restart-set rows are created exactly as for a normal job (`ready` if root with no `when`,
else `pending`). Then, unconditionally for restart jobs, the existing post-creation
cascade runs (`promote_ready_steps` → `skip_unreachable_steps` → `expand_for_each_steps`,
`job_creator.rs:353-389`), followed by server-side dispatch (`handle_task_steps`,
`handle_approval_steps`) and settlement.

**Settlement is unified (rev 2).** Today the creator has its own terminal check
(`job_creator.rs:414-428`: any failed → `failed`, else `completed` with `output = None`)
that disagrees with the orchestrator's (`orchestrator.rs:132-203`: tolerates
`continue_on_failure` failures, aggregates terminal-step outputs). This design extracts
the orchestrator's terminal block into `orchestrator::settle_if_all_terminal(pool, job_id,
task) -> Option<JobStatus>` and makes **both** the creator and `on_step_completed` call
it. Consequences that must hold:

- A carried `failed` row whose *current* flow step has `continue_on_failure: true` does
  **not** fail the job. The UI warning (§7.1) is computed with the same rule.
- A carried `cancelled` row with no failures does not produce `completed`; the shared
  routine maps it to `cancelled` (today's orchestrator only checks `failed` — the
  extraction adds the cancelled case, which is also the correct behaviour for
  non-restart jobs).
- A restart job that is terminal immediately after creation gets a real aggregated
  `output`.

**Terminal-at-creation side effects (rev 2).** The creator never runs terminal handling;
only the worker `complete_job` path and cancellation call `handle_job_terminal`
(`job_recovery.rs`, callers at `web/worker_api/jobs.rs:1029`, `cancellation.rs:127`).
A job that settles at creation (all skipped, or a restart whose set is entirely
cascade-skipped) therefore fires no hooks, archives no logs and records no metrics — a
pre-existing gap that Re-run shares. `create_job_for_task_inner` returns
`CreatedJob { job_id, terminal_at_creation: bool }`; the HTTP handlers (`execute_task`
and `restart_job`) call `handle_job_terminal` exactly once when the flag is set. The
metrics counter already has a CAS guard (`job_recovery.rs:13-45`); hooks do not, so the
single call site is the guard.

**Prerequisite fix — `for_each` placeholder behind a failed dependency (rev 2).**
`skip_unreachable_steps` deliberately ignores placeholders (`job_step.rs:819-822`) and
`expand_for_each_steps` just `continue`s when a dependency is `failed`/`cancelled`
(`job_creator.rs:722-737`). The placeholder stays `pending` forever and the job never
settles. This is a **live bug today** for any flow with a `for_each` step directly
downstream of a step that fails (the 2026-09-02 fix covered only the *skipped*-dependency
case). Fix in `expand_for_each_steps`: when deps are not met because a dependency is
`failed`/`cancelled` and the placeholder's flow step is not `continue_on_failure`, mark
the placeholder `skipped`. Ship this independently of restart (see §13).

### 4.4 Job row and input

| field | value |
|---|---|
| `workspace`, `task_name` | from `S` |
| `input` | **result of replaying `S.raw_input`** through the normal pipeline: `merge_defaults` → `resolve_connection_inputs` against the *current* workspace (rev 2) |
| `raw_input` | `S.raw_input` verbatim |
| `source_type` | `"restart"` |
| `source_id` | acting user's email, or `"api"` (same rule as execute) |
| `source_job_id` | `S.job_id` (immediate source, matches Re-run chains) |
| `restart_from_step` | `x` |
| `revision` | `WorkspaceManager::get_revision(ws)` — current |
| `retry_*`, `parent_*` | `NULL`/0 |

Why replay `raw_input` rather than copy `S.input` (rev 2): `S.input` already holds
resolved connection **objects**, and `resolve_connection_inputs` passes objects through
without any lookup (`stroem-common/src/template.rs:593-603`), so a copied `input` would
revive credentials of a connection that has since been rotated, deleted or un-shared.
The retry path does copy `input` (`job_recovery.rs:917`) — that is precedent, not proof
of safety, and is noted in §13. Replaying `raw_input` gives identical *user* values with
current resolution; a legacy job with `raw_input = NULL` is rejected with the same 400
Re-run uses ("Source job predates Re-run prefill"). Schema drift (a new required input
without default) surfaces as the normal 400 from `merge_defaults`.

The Re-run **sentinel** branch is not involved: restart never receives `••••••` values
from a form, it reads `S.raw_input` directly.

## 5. Data Model

Migration `044_job_step_carried_over.sql`:

```sql
ALTER TABLE job_step ADD COLUMN carried_over BOOLEAN NOT NULL DEFAULT FALSE;
```

Rev 2 replaced the proposed `seeded_from_job_id UUID` column: the source job is already
recorded once on `job.source_job_id` (migration 032), and an `ON DELETE SET NULL` UUID
cannot keep the "carried over" badge alive after the source is retained away. The boolean
survives retention; the link uses `job.source_job_id` and disappears with it.

Wiring: add `carried_over` to `STEP_COLUMNS`/`JobStepRow` (`job_step.rs:10-59`) and to
the explicitly assembled step JSON in `GET /api/jobs/{id}` (`web/api/jobs.rs:309-352`).
`NewJobStep` is unchanged (the INSERT lets the column default; seeding sets it).

## 6. Server

### 6.1 Endpoint

```
POST /api/jobs/{id}/restart
{ "from_step": "publish", "dry_run": false }

200 OK (dry_run: true)
{ "restart_steps": ["publish","recalc","agg-sessions"],
  "carried_over": ["dates","recalc-sessions","ai-sources"],
  "carried_failed": [],          // carried rows that will fail the job under the current flow's continue_on_failure
  "carried_failed_tolerated": [] // carried failed rows the current flow tolerates
}

201 Created (dry_run: false)
{ "job_id": "<uuid>", "restart_steps": [...], "carried_over": [...], "carried_failed": [...] }
```

`dry_run` exists so the confirm dialog is authoritative (rev 2): the job-detail response
lists only rows that exist in `S` — it cannot show steps added to the flow since, and it
still shows removed ones — so a client-side preview from `depends_on` would be wrong
exactly when it matters.

Handler `restart_job` in `web/api/jobs.rs`, registered next to `cancel`/`approve`
(`web/api/mod.rs:312-313`). Checks, in order:

| check | response |
|---|---|
| auth configured, no user | 401 (explicit — `check_job_acl` returns `Run` when no user is supplied, `jobs.rs:941-952`) |
| job not found, or ACL `Deny` | 404 `"Job"` |
| ACL `View` | 403 `"Insufficient permissions to restart this job"` |
| job not terminal | 409 `"Job is still running"` |
| `raw_input` is NULL | 400 `"Source job predates Re-run prefill"` |
| workspace unloaded / task gone | 400 `"Task '{t}' no longer exists in workspace '{ws}'"` |
| `from_step` not in current flow | 400 `"Step '{x}' is not in the current flow of task '{t}'"` |
| `from_step` contains `[` | 400 `"Restart from the loop step '{base}', not an instance"` |
| creation error | `classify_execute_error` (made `pub(crate)` and moved to `web/api/mod.rs`; it is private in `tasks.rs:544-597` today) |

ACL is **Run** on `(job.workspace, job.task_name)` — restart creates a job, so it
follows execute (`tasks.rs:458-475`) and cancel (`jobs.rs:618-630`), not the
View-is-enough relaxation Re-run applies to reading `source_job_id`.

After a non-dry-run creation: `fire_initial_suspended_hooks` (as `tasks.rs:532`), then
`handle_job_terminal` iff `terminal_at_creation` (§4.3).

### 6.2 Creator changes

Replace the positional `source_job_id` parameter of `create_job_for_task_inner` (which
today both drives the sentinel lookup and populates the column) with a typed mode
(rev 2):

```rust
pub enum CreationMode<'a> {
    Normal,
    Rerun   { source_job_id: Uuid },                       // sentinel replay, as today
    Restart { source: &'a JobRow, from_step: &'a str, seeds: &'a [Seed] },
}
pub struct Seed { step_name: String, status: StepStatus, output: Option<Value>, error_message: Option<String> }
pub struct CreatedJob { job_id: Uuid, terminal_at_creation: bool }
```

- `compute_restart_set(&task.flow, &source_steps, from_step) -> RestartPlan
  { restart_steps, carried: Vec<Seed>, carried_failed, carried_failed_tolerated }` — pure,
  unit-tested, shared by dry-run and real runs.
- Inside the creation transaction (`job_creator.rs:309-343`), after `create_steps_tx`,
  `JobStepRepo::seed_steps_tx(&mut tx, job_id, &seeds)` runs the §4.2 UPDATE per seed
  and asserts `rows_affected == 1`.
- `Restart` forces the post-creation cascade and settlement (§4.3).
- **Post-commit failure handling (rev 2).** The job/step INSERT commits, then promotion,
  expansion and dispatch run with `?` (`:352-401`) — an error there returns 500 while a
  committed `pending`/partially initialised job stays behind (pre-existing for execute
  too). The post-commit phase is wrapped: on error, the job is `mark_failed` with a
  `_server` log line `[creation] initialisation failed: {e}`, `handle_job_terminal` runs,
  and the handler still returns the job id (201 with the job in `failed` state) so the
  user lands on a job page that explains itself instead of a 500.
- **Server-dispatched roots.** A restart root may be a `type: task` or `approval` step,
  dispatched at creation by the existing `handle_task_steps`/`handle_approval_steps`. Two
  pre-existing gaps in that path are listed in §13 and must be fixed first: a child job
  that settles synchronously at creation never propagates to its parent step, and a
  `type: agent` step cannot be claimed at all. Both are independent of restart.

### 6.3 Hooks

The top-level classifier (`hooks.rs:95-98`, duplicated at `:210` for `on_suspended`;
`:552` is a test helper) is centralised into one `fn is_top_level_source(&str) -> bool`
and gains `rerun` and `restart`. This enables workspace-level `on_success`, `on_error`,
`on_cancel` **and** `on_suspended` for both — `rerun` is a pre-existing omission (a
re-run's failure fires task-level hooks but not workspace-level ones), called out in the
release notes.

Hook context (`hooks.rs:319-362`) lists every `failed` row in `failed_steps`. Carried
failures are **included, flagged** `carried_over: true`, so an `on_error` fired because
of a carried failure is not silent, but a notification template can distinguish "failed
again" from "failed in the source job". Documented in the hooks guide.

### 6.4 Duration statistics and ETA

- Whole-job stats (`repos/job.rs:999`, `:1042`): `AND source_type <> 'restart'`. A restart
  from a root step with nothing carried is a full execution, but distinguishing it needs a
  subquery; excluded for simplicity, documented.
- Per-step stats (`repos/job_step.rs:1146`): the CTE that picks recent completed jobs
  (`:1158-1164`) also excludes `source_type = 'restart'`, so restart jobs cannot consume
  the sample window. The existing `started_at IS NOT NULL` already drops carried rows.
- ETA (`ui/src/lib/eta.ts:119-149`) falls back to whole-job p50 when a step stat is
  missing. For `source_type === 'restart'` jobs the fallback is suppressed: step-weighted
  ETA over restart-set steps when every needed stat exists, otherwise no ETA.

### 6.5 Concurrency

Two simultaneous restarts of the same source create two independent jobs, like two
Re-runs. No lock, no dedup. The source job is never mutated.

## 7. UI

### 7.1 Action placement and confirm dialog

- **Normal steps**: a **Restart from here** button in `StepDetail`
  (`ui/src/components/step-detail.tsx`, next to `ApprovalCard`, `:78-84`).
- **`for_each` placeholders**: the placeholder header only toggles its instances and never
  opens `StepDetail` (`step-timeline.tsx:215-226`, `:397-449`), so the button goes on the
  `LoopGroup` header; instance rows get no button.
- Shown only when the job is terminal **and** the task detail's `can_execute` is true
  (`tasks.rs:331-350`; fetched once by the job page and passed down — `StepDetail` has no
  job/permission context today, `step-detail.tsx:15-68`).

Click → `POST …/restart {dry_run: true}` → dialog:

> Restart **{task}** from **{step}**?
> Reruns {n} step(s): {restart_steps}. {m} step(s) are carried over unchanged.
> ⚠ {k} carried-over step(s) ended failed and are not tolerated by the current flow — the
>   new job will end failed. Restart from an earlier step to rerun them.
>   *(only when `carried_failed` is non-empty)*

Confirm → `POST …/restart {dry_run: false}` → `navigate("/jobs/{job_id}")`. Errors toast
the server message. New `restartJob(jobId, fromStep, dryRun)` in `ui/src/lib/api.ts`.

### 7.2 Lineage

`job-detail.tsx` `InfoGrid` (`:305-381`): when `source_type === "restart"`, **Restart of**
`<source link>` **from** `<step>`, mirroring "Re-run of" (`:366-380`).

### 7.3 Carried-over steps

`StepRow` badge cluster (`step-timeline.tsx:220-260`): muted **carried over** badge when
`step.carried_over`. `StepDetail` for such a row does **not** fetch logs for the current
job (it does unconditionally today, `step-detail.tsx:15-68`); it shows "Carried over from
job `<source link>` — logs and artifacts live there", or just "Carried over from an
earlier job" when `source_job_id` is null (retention). Duration and p50 badges are
suppressed for carried rows.

### 7.4 Types

`ui/src/lib/types.ts`: `JobStep.carried_over: boolean`; `restart_from_step` already exists
on `JobDetail` (`:129`).

## 8. Known Limitations (documented, not solved)

- **State snapshots** resolve to the *latest* snapshot for the task at claim time
  (`repos/task_state.rs:24-42`), not what the source job saw.
- **Artifacts** are per job (`job_artifact.job_id`). The new job starts with none; a
  restart-set step reading `/artifacts/` from a carried upstream step finds nothing.
- **Revision drift**: carried outputs were produced by the source revision; the restart
  set runs the current one. Template mismatches fail visibly at render time.
- **Carried cross-workspace rows** carry *current* action metadata with *old* output; the
  only provenance is `job.source_job_id`.
- **Redaction** of carried `output`/`error_message` in API responses uses the currently
  configured secret values (`web/api/jobs.rs:462-528`). Values from removed/rotated
  connections or user-typed secrets are not redacted — the same limitation
  `job.input` has today.
- **Child jobs** of carried `type: task` steps are not re-linked to the new job.
- **Whole-job duration stats** exclude every restart job, including root restarts.

## 9. Tests

### Unit (`job_creator.rs`)
`compute_restart_set`: linear middle; diamond (other branch carried); root (empty carry
set); leaf; **step inserted upstream of an existing step pulls it into the restart set**;
step removed from flow dropped; loop-instance name rejected; every non-terminal source
status incl. `claimed` → `cancelled` seed; `for_each` placeholder carried with its array;
`carried_failed` vs `carried_failed_tolerated` split by current `continue_on_failure`.
`settle_if_all_terminal`: failed / tolerated-failed / cancelled / completed-with-output.

### Integration (`tests/restart_integration_test.rs`, harness like `rerun_integration_test.rs`)
- Linear `a → b → c`, `b` failed: restart from `b` → `a` carried (`carried_over`,
  `completed`, `started_at NULL`), `b` `ready` at once, `c` `pending`; worker completes
  `b`,`c`; job `completed` with aggregated output; `{{ a.output }}` renders at claim time.
- Parallel failure carried: `a→b`, `a→c`, both failed: restart from `b` → `c` carried
  `failed`; job ends `failed`. Same with `c: continue_on_failure: true` → job `completed`.
- Restart set entirely cascade-skipped at creation → job settles `failed` **and**
  workspace `on_error` fires exactly once (terminal-at-creation path).
- Carried `cancelled` row, no failures → job ends `cancelled`, not `completed`.
- `for_each` placeholder carried: downstream template reads the array; no instance rows.
- `for_each` placeholder **in the restart set** behind a carried `failed` dep → skipped,
  job settles (prerequisite fix regression).
- `type: task` step carried: output present, no child job. Restart **from** a `type: task`
  step: child created; child that settles at creation propagates to the parent.
- Restart from an `approval` step: new job suspends there; `on_suspended` fires.
- Flow changed: step added downstream of `x` runs; step added upstream of a carried step
  pulls it into the restart set; removed step absent.
- Input replay: connection renamed/unshared since `S` → 400 from resolution, no job
  created; connection value rotated → new job carries the *new* resolved value.
- Rejections: 401 with auth and no user; 409 running source; 400 unknown step; 400
  `step[0]`; 400 legacy `raw_input NULL`; 404/403 per ACL (`setup_with_auth_and_acl`).
- `dry_run: true` creates nothing and matches the real run's `restart_steps`.
- Post-commit failure injection (after seeding, after expansion, after child creation) →
  job `failed`, `_server` log line present, `handle_job_terminal` ran once.
- `raw_input` preserved: Re-run of the restart job prefills.
- Hooks: workspace `on_error` fires for a failed restart **and** for a failed `rerun`;
  hook context marks carried failures `carried_over: true`.
- Stats: restart jobs absent from whole-job and per-step samples.
- Redaction: configured local and cross-workspace secret values in carried
  `output`/`error_message` come back as `••••••`.

### Frontend (`ui/src/components/__tests__`)
Button visibility (running job, instance row, placeholder header, `can_execute` false);
dialog renders from the dry-run response incl. the tolerated/untolerated split;
carried-over badge and logs-elsewhere message; no log fetch for carried rows; ETA
suppressed for restart jobs without full step stats.

## 10. Documentation

- `docs/src/content/docs/guides/` job/re-run page: "Restart from a step" — what reruns,
  what is carried over, §8 limitations, and the "restart from an earlier step to rerun
  other failures" guidance; hooks guide: `carried_over` on `failed_steps`, and the new
  top-level status of `rerun`/`restart`.
- `docs/src/content/docs/reference/api.md`: `POST /api/jobs/{id}/restart` incl. `dry_run`.
- `CLAUDE.md` § *Job Lineage*: replace "*reserved*" with shipped semantics; add
  `carried_over`; note the shared `settle_if_all_terminal`, the `CreationMode` enum, and
  "restart jobs excluded from duration stats".
- `docs/internal/TODO.md`: close the §13 items as they land.
- Release notes: `rerun` hook change; `cancelled` settlement change; the two pre-existing
  stuck-job fixes.

## 11. Rollout

Additive migration, new endpoint, new UI action, no config. Mixed versions: old UI lacks
the button; new UI against an old server gets 404 and toasts it. The §13 prerequisites
ship first as their own patch release(s) — two of them are live stuck-job bugs.

## 12. Resolved Questions

1. Preview authority → server `dry_run` (rev 2; the job-detail step list cannot represent
   flow changes).
2. Marker column → `carried_over BOOLEAN` + `job.source_job_id` for the link (rev 2).
3. Task must still exist → yes, 400, matching Re-run.
4. Input → replay `raw_input`, reject legacy NULL (rev 2; copying resolved `input` revives
   revoked connection values).

## 13. Prerequisites and Pre-existing Defects Surfaced by Review

Independent of restart; fix first, each with its own regression test:

| # | Defect | Evidence | Severity |
|---|---|---|---|
| P1 | `for_each` placeholder directly downstream of a **failed** step is never skipped → job stuck `running` | `job_step.rs:819-822`, `job_creator.rs:722-737` | **Live stuck-job bug**, hotfix |
| P2 | `type: agent` steps are excluded from the worker claim query, so no worker can ever claim one | `job_step.rs:319` and `:967`; regression introduced by retry commit `ed67c76` (2026-04-08) re-adding `'agent'` to `NOT IN`; no test claims an agent step; prod has never run one | Latent feature breakage, hotfix |
| P3 | Jobs that settle at creation (all skipped, or dispatch failure at creation) get no `handle_job_terminal`: no hooks, log archive, metrics | `job_creator.rs:414-428` vs callers of `handle_job_terminal` | Should-fix, needed by §4.3 |
| P4 | Creator's terminal check ignores `continue_on_failure` and `cancelled`, and drops output | `job_creator.rs:414-428` vs `orchestrator.rs:132-203` | Should-fix, needed by §4.3 |
| P5 | A child job that settles synchronously at creation never propagates to its parent `type: task` step (parent stays `running`) | `job_creator.rs:616-654`; propagation only from terminal handling `job_recovery.rs:645-665` | Stuck-job bug (rare), fix with P3 |
| P6 | `rerun` jobs are not top-level for workspace hooks | `hooks.rs:95-98` | Behaviour gap |
| P7 | Retry copies resolved `job.input`, reviving revoked connection values | `job_recovery.rs:912-927`, `template.rs:593-603` | Security hygiene; consider replaying `raw_input` there too |
| P8 | Post-commit initialisation errors leave a committed job behind and return 500 | `job_creator.rs:352-401` | Robustness |

## 14. Review Log

Codex adversarial review, 2026-09-07 (session `01a07c4e-85c8-7f63-8235-cd48e3ed8146`):
7 blockers, 7 should-fix, 3 nits; verdict "not safe to implement as written". All items
verified against the code and folded in above: restart-set closure (§4.1), placeholder
skip (§4.3/P1), unified settlement + terminal side effects (§4.3/P3/P4), server-dispatch
gaps (§6.2/P2/P5), input replay policy (§4.4), post-commit failure handling (§6.2/P8),
output reuse promise (§4.2), hooks scope and carried failures (§6.3), stats/ETA (§6.4),
UI preview via dry-run and placeholder placement (§7.1), `carried_over` boolean (§5), ACL
401 + redaction note (§6.1/§8), cross-workspace carried rows (§4.1/§8), seed hygiene
(§4.2), `CreationMode` (§6.2), `classify_execute_error` visibility (§6.1).
