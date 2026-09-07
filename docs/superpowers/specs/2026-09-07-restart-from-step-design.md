# Restart From Step — Design

**Status:** Draft, pending Codex + user review
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

## 2. Decisions (agreed 2026-09-07)

| Question | Decision |
|---|---|
| Which steps can be restart points | **Any** flow step (completed, failed, skipped, cancelled), except `for_each` *instance* rows |
| Job input | **Reused unchanged** from the source job — no form |
| Workspace revision | **Current** live revision (same as Re-run), not the source job's |
| Steps outside the restart set that did not complete | **Carried over as they ended** (failed stays failed) |

Consequences are spelled out in §4 and §8.

## 3. Non-Goals

- Editing input on restart (use Re-run).
- Pinning the source job's revision. The tarball cache only holds revisions still on
  disk (`web/worker_api/workspace.rs:130-136` → 404 otherwise), so pinning cannot be
  made reliable without a git-checkout-by-OID path. Out of scope.
- Resuming *in place* on the same job row. Rejected: overwrites logs/outputs, fires
  hooks and metrics twice, collides on per-job artifact names, and breaks the
  "terminal job is immutable" assumption everywhere.
- Restarting a single `for_each` instance (`step[3]`). Restart from the placeholder.
- Copying artifacts or state snapshots from the source job (see §8).
- Restarting non-terminal jobs.

## 4. Semantics

### 4.1 Restart set

Given source job `S`, its task's **current** flow `F`, and chosen step `x`:

```
restart_set = { x } ∪ transitive_dependents(F, x)
            ∪ { s ∈ F | S has no job_step row named s }        // new steps since S ran
carry_set   = F \ restart_set
```

- Dependents are computed on `F` (current flow), not on the flow as it was when `S`
  ran. `depends_on` is the only edge type; `for_each` instances are not in `F`.
- Steps present in `S` but absent from `F` are dropped silently (the flow changed;
  the current flow is authoritative, exactly as Re-run behaves).
- `x` must be a key of `F`. `x` containing `[` is rejected (loop instance).

### 4.2 Carried-over steps

For every `s ∈ carry_set` the new job's `job_step` row is created by the normal
creator (so `action_spec`, `when_condition`, `for_each_expr`, retry config,
`action_workspace`/`action_revision` reflect the **current** flow), then overwritten
in the same transaction with the source row's terminal state:

| copied from `S.job_step[s]` | value on new row |
|---|---|
| `status` | verbatim: `completed`, `failed`, `skipped`, or `cancelled` |
| `output` | verbatim (for a `for_each` placeholder this is the aggregated array) |
| `error_message` | verbatim |
| `completed_at` | `NOW()` (creation time — the row did not run in this job) |
| `started_at`, `worker_id` | `NULL` |
| `seeded_from_job_id` (new column, §5) | `S.job_id` |

Not copied: `retry_attempt`/`retry_history`/`retry_at` (fresh row), `agent_state`,
`suspended_at`, `loop_*`. `for_each` instance rows of a carried-over placeholder are
**not** recreated — `build_step_render_context` (`job_creator.rs:1344-1346`) skips
instance rows anyway, so downstream templates only ever read the placeholder's
aggregated output. A carried-over `type: task` step keeps the child job's output that
`propagate_to_parent` already stamped onto it (`job_recovery.rs:439-445`); no child
job is recreated and the new step has no `parent_job_id` back-reference from any child.

If a carried-over source step is **non-terminal** (`pending`/`ready`/`running`/
`suspended`) — possible only if `S` was cancelled mid-flight — it is treated as
`cancelled` with `error_message = "carried over from cancelled source job"`. It is
*not* added to the restart set, per the "carry as ended" decision; the user restarts
from an earlier ancestor if they want it to run.

### 4.3 Restart-set steps

Created exactly as by a normal job: `ready` if root with no `when`, otherwise
`pending`. After seeding, the existing post-creation cascade
(`promote_ready_steps` → `skip_unreachable_steps` → `expand_for_each_steps`, see
`job_creator.rs:353-389`) runs **unconditionally** for restart jobs (today it is gated
on `needs_post_creation_loop`), so a restart-set step whose deps are all carried-over
`completed` rows is promoted immediately. `handle_task_steps` / `handle_approval_steps`
then dispatch as usual, and the all-terminal settle (`:417-428`) runs unconditionally
too — a restart set that is entirely cascade-skipped (e.g. `x`'s only dep was carried
over as `failed`) must close the job as `failed`, not leave it `pending`.

`when` conditions on restart-set steps are re-evaluated (stored `when_condition` from
the current flow). Carried-over outputs render under the usual sanitized names
(`{{ upstream_step.output.x }}`), since the render context keys off `status` +
`output` only.

### 4.4 Job row

| field | value |
|---|---|
| `workspace`, `task_name` | from `S` |
| `input` | `S.input` verbatim (already merged + connection-resolved; the retry path does the same, `job_recovery.rs:917`) |
| `raw_input` | `S.raw_input` verbatim (so a later **Re-run** of the restart job prefills correctly; `NULL` stays `NULL`) |
| `source_type` | `"restart"` |
| `source_id` | the acting user's email, or `"api"` (same rule as execute) |
| `source_job_id` | `S.job_id` (immediate source, not root — matches Re-run chains) |
| `restart_from_step` | `x` |
| `revision` | `WorkspaceManager::get_revision(ws)` — current |
| `retry_*`, `parent_*` | `NULL`/0 |

Restart jobs are top-level for hooks (§6.3) and are excluded from duration statistics
(§6.4).

## 5. Data Model

Migration `044_job_step_seeded_from.sql`:

```sql
ALTER TABLE job_step ADD COLUMN seeded_from_job_id UUID
    REFERENCES job(job_id) ON DELETE SET NULL;
```

Nullable, additive, no backfill. Purpose: the UI must (a) badge carried-over steps and
(b) link to the source job for their logs, which do not exist under the new job id.
The alternative heuristic (`status IN ('completed','failed') AND started_at IS NULL`)
is fragile — server-dispatched `type: task` steps also have `started_at` set by
`mark_running_server`, but future server-side step kinds may not.

`JobStepRow`/`STEP_COLUMNS` (`crates/stroem-db/src/repos/job_step.rs:10-58`) gain the
field; `NewJobStep` does **not** (it is set by the seeding UPDATE, never at insert).
`GET /api/jobs/{id}` step objects (`web/api/jobs.rs:305-345`) expose it as
`seeded_from_job_id: string | null`.

No change to `job`: migration 032 already provides everything.

## 6. Server

### 6.1 Endpoint

```
POST /api/jobs/{id}/restart
Content-Type: application/json
{ "from_step": "publish" }

201 Created
{ "job_id": "<uuid>", "restart_steps": ["publish", "recalc", "agg-sessions"],
  "carried_over": 4 }
```

Handler `restart_job` in `web/api/jobs.rs`, registered next to `cancel`/`approve`
(`web/api/mod.rs:312-313`). Order of checks:

| check | response |
|---|---|
| auth configured but no user | 401 |
| job not found, or ACL `Deny` | 404 `"Job"` (same as cancel) |
| ACL `View` | 403 `"Insufficient permissions to restart this job"` |
| job not terminal (`completed`/`failed`/`cancelled`) | 409 `"Job is still running"` |
| workspace unloaded / task no longer in workspace | 400 `"Task '{t}' no longer exists in workspace '{ws}'"` |
| `from_step` not in current flow | 400 `"Step '{x}' is not in the current flow of task '{t}'"` |
| `from_step` contains `[` | 400 `"Restart from the loop step '{base}', not an instance"` |
| creation error | classified by the existing `classify_execute_error` (400 for user errors, else 500) |

ACL requires **Run** on `(job.workspace, job.task_name)` via `check_job_acl` — the
restart *creates* a job, so it follows the execute precedent, not the "View is enough
to read the source" relaxation Re-run applies to `source_job_id`.

`fire_initial_suspended_hooks` runs after creation as in `execute_task`
(`tasks.rs:532`).

### 6.2 `job_creator::create_restart_job`

```rust
pub async fn create_restart_job(
    workspaces: &WorkspaceManager, pool: &PgPool,
    workspace_config: &WorkspaceConfig, workspace_name: &str,
    source: &JobRow, source_steps: &[JobStepRow],
    from_step: &str, source_id: Option<&str>, revision: Option<&str>,
    defaults: JobDefaults,
) -> Result<RestartOutcome>   // { job_id, restart_steps: Vec<String>, carried_over: usize }
```

Implementation plan, minimal change to the existing creator:

1. `compute_restart_set(&task.flow, source_steps, from_step) -> (BTreeSet<String>, Vec<Seed>)`
   — pure function, unit-tested. `Seed { step_name, status, output, error_message }`
   applies the §4.2 mapping (incl. the non-terminal → `cancelled` rule).
2. Extend `create_job_for_task_inner` with one new parameter
   `seed: Option<&JobSeed>` where
   `JobSeed { source_job_id, restart_from_step, raw_input_override, steps: Vec<Seed> }`.
   All existing callers pass `None`. Inside the creation transaction
   (`job_creator.rs:309-343`), after `create_steps_tx`, call the new
   `JobStepRepo::seed_steps_tx(&mut tx, job_id, &seed.steps)`:
   ```sql
   UPDATE job_step
      SET status = $3, output = $4, error_message = $5,
          completed_at = NOW(), seeded_from_job_id = $6
    WHERE job_id = $1 AND step_name = $2
   ```
   (one statement per seed inside the tx; ≤ flow size, fine.) The job INSERT uses
   `raw_input_override` when present and sets `restart_from_step`.
3. When `seed.is_some()`, force `needs_post_creation_loop = true` so the cascade and the
   all-terminal settle both run (§4.3).
4. Skip the Re-run sentinel branch (`:131-162`) for restart — `input` is already
   resolved; `source_job_id` is set on the row directly, not used for prefill.
   Concretely: the sentinel branch keys off a new `rerun_source: Option<Uuid>`
   parameter, while `seed.source_job_id` only populates the column. (Today the single
   `source_job_id` parameter does both jobs; splitting it is the smallest safe change.)

`create_restart_job` loads nothing itself — the handler passes the source rows it
already fetched for validation, keeping the creator free of HTTP concerns and easy to
drive from tests.

### 6.3 Hooks

`hooks.rs:95-98` (and the two sibling matchers at `:210`, `:552`) list top-level source
types as `api | user | trigger | webhook | mcp | retry`. Add `rerun` **and** `restart`.
`rerun` is a pre-existing omission: a re-run's failure fires task-level hooks but not
workspace-level ones today. Fixing it here is one token per site and is called out in
the release notes.

### 6.4 Duration statistics

`JobRepo::get_task_duration_stats` / `get_recent_durations` (`repos/job.rs:999`, `:1042`)
and `JobStepRepo::get_step_duration_stats_for_task` (`repos/job_step.rs:1146`) select
`status = 'completed'` runs. Add `AND source_type <> 'restart'` to the job-level
queries; for the step-level query add `AND seeded_from_job_id IS NULL` so carried-over
rows (duration 0) never enter per-step p50. Restart-set steps *do* count — they are
real executions.

### 6.5 Concurrency / idempotency

Two simultaneous restarts of the same source create two independent jobs, like two
Re-runs. No lock, no dedup. The source job is never mutated.

## 7. UI

### 7.1 Step detail action

`ui/src/components/step-detail.tsx` renders the only per-step action today
(`ApprovalCard`, `:78-84`). Add a **Restart from here** button in the same slot, shown
when:

- the job is terminal (`completed | failed | cancelled`), and
- `step.loop_source == null` (not an instance).

Click → confirm dialog:

> Restart **{task}** from **{step}**?
> Reruns {n} step(s): {list}. {m} step(s) are carried over from this job unchanged.
> ⚠ {k} carried-over step(s) ended **failed** — the new job will end failed too.
>   Restart from an earlier step to rerun them. *(only when k > 0)*

The list is computed client-side from `depends_on` on the step objects (already
returned by `GET /api/jobs/{id}`, filled from the current flow). The server's
`restart_steps` is authoritative; the client list is a preview. On `201`, `navigate`
to `/jobs/{new_id}`; on `4xx`, toast the server message.

New client function `restartJob(jobId, fromStep)` in `ui/src/lib/api.ts` next to
`executeTask` (`:280-293`).

### 7.2 Lineage

`job-detail.tsx` `InfoGrid` (`:305-381`) gains, when `source_type === "restart"`:
**Restart of** `<source id link>` **from** `<step>` — mirroring the existing "Re-run of"
entry (`:366-380`).

### 7.3 Carried-over steps

`StepRow` badge cluster (`step-timeline.tsx:220-260`) shows a muted **carried over**
badge when `step.seeded_from_job_id` is set. `StepDetail` replaces the Logs tab body for
such a step with "This step was carried over from job `<link>`; logs and artifacts live
there." Duration/p50 badges are suppressed for carried-over rows.

### 7.4 Types

`ui/src/lib/types.ts`: `JobStep.seeded_from_job_id: string | null`; `restart_from_step`
already exists on `JobDetail` (`:129`).

## 8. Known Limitations (documented, not solved)

- **State snapshots** resolve to the *latest* snapshot for the task at claim time
  (`TaskStateRepo::get_latest`, `repos/task_state.rs:24-42`), not the snapshot the source
  job saw. A restart after a later successful run reads the newer state.
- **Artifacts** are per job (`job_artifact.job_id`, `ON DELETE RESTRICT`). The new job
  starts with none; a restart-set step that reads `/artifacts/` from an upstream
  carried-over step will not find them. Upstream artifacts stay downloadable from the
  source job.
- **Revision drift**: carried-over outputs were produced by the source revision; the
  restart set runs the current one. Normally harmless (outputs are data). If the
  current flow renamed or re-shaped an upstream output, templates fail at render time
  with the usual step-failure path — visible, not silent.
- **Retention**: if the source job is deleted by retention, `seeded_from_job_id` and
  `source_job_id` become `NULL` (`ON DELETE SET NULL`); the badge stays, the link
  disappears.
- **Child jobs** of carried-over `type: task` steps are not re-linked; walking
  `parent_job_id` from the new job finds only children created by the restart set.

## 9. Tests

### Unit (`job_creator.rs`)
- `compute_restart_set`: linear middle; diamond (restart from one branch keeps the
  other); root step (everything reruns, carry set empty); leaf step; step new in
  current flow lands in restart set; step removed from flow is dropped; loop instance
  name rejected; non-terminal source step → `cancelled` seed; `for_each` placeholder
  carried with array output.

### Integration (`tests/restart_integration_test.rs`, harness like
`rerun_integration_test.rs`)
- Linear `a → b → c`: `a` completed, `b` failed. Restart from `b` → new job has `a`
  `completed` with `seeded_from_job_id`, `b` `ready` immediately, `c` `pending`; worker
  completes `b`,`c`; job `completed`; `{{ a.output }}` visible to `b` at claim time.
- Parallel failure carried over: `a → b`, `a → c`, both `b`,`c` failed. Restart from
  `b` → `c` carried as `failed`; after `b` completes the job ends `failed`.
- Restart from a step whose only dep is carried over as `failed` → step cascade-skipped
  at creation, job settles `failed` (exercises the unconditional settle).
- `for_each` placeholder carried over: downstream template reads the aggregated array;
  no instance rows exist in the new job.
- `type: task` step carried over: output present, no child job created.
- Restart from a `type: task` step: child job created under the new job.
- Restart from an `approval` step: new job suspends at that step.
- Flow changed: task gained a new step downstream of `x` → it runs; lost a step → absent.
- Rejections: 409 on running source; 400 on unknown step; 400 on `step[0]`; 404/403 per
  ACL with `setup_with_auth_and_acl`.
- `raw_input` preserved: Re-run of the restart job prefills (reuse the rerun harness).
- Hooks: workspace `on_error` fires for a failed restart job (and for a `rerun` job —
  regression for §6.3).
- Duration stats exclude restart jobs and seeded steps.

### Frontend (`ui/src/components/__tests__`)
- Button visibility: hidden on running job, hidden on loop instance, shown otherwise.
- Confirm dialog lists dependents computed from `depends_on`; failure warning appears
  when a non-restart step is `failed`.
- Carried-over badge and logs-elsewhere message render from `seeded_from_job_id`.

## 10. Documentation

- `docs/src/content/docs/guides/jobs.md` (or the page that documents Re-run): new
  "Restart from a step" section — what reruns, what is carried over, the four
  limitations in §8, and the "restart from an earlier step to rerun other failures"
  guidance.
- `docs/src/content/docs/reference/api.md`: `POST /api/jobs/{id}/restart`.
- `CLAUDE.md` § *Job Lineage*: replace "*reserved*" with the shipped semantics; add the
  `seeded_from_job_id` column and the rule "restart jobs excluded from duration stats".
- `docs/internal/TODO.md`: close the rerun-hooks omission; add the §8 items that could
  become features (artifact carry-over, state pinning).
- Release notes: call out the `rerun` hook fix as a behaviour change.

## 11. Rollout

Additive migration, new endpoint, new UI action. No config. Safe to ship in a patch
release; no data backfill. Mixed-version note: an old UI against a new server simply
lacks the button; a new UI against an old server gets 404 from `/restart` and shows the
toast.

## 12. Open Questions for Review

1. Should `restart_steps` in the 201 body be authoritative for the dialog (two-phase:
   `POST …/restart?dry_run=true` first), or is the client-side `depends_on` preview
   acceptable? Current choice: client preview, single POST.
2. Is the `seeded_from_job_id` column worth a migration versus deriving "carried over"
   from `job.restart_from_step` + a client-side dependents computation? Current choice:
   column (explicit, survives flow changes).
3. Should restart require the task to still exist in the workspace? Re-run does (it
   goes through `/execute`). Current choice: yes, 400.
