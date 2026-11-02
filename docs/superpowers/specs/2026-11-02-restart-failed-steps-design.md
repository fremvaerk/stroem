# Restart Failed Steps — Design

Revision 2 (2026-11-02). Extends
`docs/superpowers/specs/2026-09-07-restart-from-step-design.md` ("the base spec").
Everything the base spec says holds unless a section below changes it.

## 1. Problem

Restart From Step takes ONE chosen step. `restart::compute_restart_set`
(`crates/stroem-server/src/restart.rs:38-142`) builds
`roots = {from_step} ∪ {current flow steps with no source row}` and reruns the
roots plus their transitive dependents; every other step is carried over as it
ended — failures included.

Prod 2026-11-02, job `dd4c3a6a` (`jobs/recalc-pipeline`): two workers were
replaced at 03:08–03:10 CET and five steps on parallel branches failed:

| step | `continue_on_failure` | cause |
|---|---|---|
| `ai_maintain` | true | heartbeat timeout |
| `ai_maintain_release` | true | heartbeat timeout |
| `ml-prediction-master` | false | heartbeat timeout / pod not found |
| `ml-prediction-beta` | true | heartbeat timeout / pod not found |
| `ml-prediction-stage` | true | heartbeat timeout / pod not found |

Twelve steps below `ml-prediction-master` were skipped `unreachable`. A restart
from `ai_maintain_release` (job `b0a99ddf`) carried the other four failures
over as `failed`, so the twelve steps were skipped again. The user had to
restart once per branch. The confirm dialog
(`ui/src/components/restart-dialog.tsx:42-75`) also shows only
`carried_failed`, never `carried_failed_tolerated`, so it gave no warning that
four tolerated failures were copied, not rerun.

## 2. Decisions

Made by the user (2026-11-02):

- **D1 Start steps.** An all-failed restart starts at every source step with
  `loop_source IS NULL` (plain steps and `for_each` placeholders) whose status
  is `failed` or `cancelled`. Steps with `continue_on_failure: true` are
  included (`ai_maintain` is the motivating case).
- **D2 Everything else unchanged.** Transitive dependents of the start steps
  rerun; all other steps are carried over (`carried_over = true`); input =
  source `raw_input` replayed; revision = current (a pinned source re-resolves
  its ref as today); top-level jobs only; terminal source only; `Run` ACL.
- **D3 One job per click**, for the job on screen. No bulk restart.

Made in this spec (the brief's recommendations, confirmed or refined):

- **D4 API.** `POST /api/jobs/{id}/restart` keeps `from_step` and gains
  `all_failed: bool` (default `false`). Exactly one of `from_step` /
  `all_failed: true` (§5.1).
- **D5 D1 reads the status the restart gives the row.** A source row that is
  still `pending`/`ready`/`claimed`/`running`/`suspended` on a terminal job
  can exist only when the job row went terminal before its steps did: an
  explicit cancel (`JobRepo::cancel` stamps the job at once, the drain gate
  waits for workers) or a job timeout. Restart never copies such a status: the
  base spec (§4.2, `restart.rs:102-117`) turns it into a carried `cancelled`
  row with `carried over from cancelled source job`. For the restart, the row
  IS a cancelled step, so D1 makes it a start step.
  The alternative (select only rows whose stored status is `failed` or
  `cancelled`) carries the row as `cancelled`, and the decider then ends the
  new job `cancelled` even when every rerun step succeeds
  (`settlement/settle.rs:53-60`) — the "had to restart again" problem this
  feature exists to remove. Codex round 1 (finding 1) read D5 as a widening of
  D1; rev 2 keeps D5 and states this reasoning. It is reported to the user as
  a decision of this spec.
- **D6 New column, not a comma-joined `restart_from_step`** (§6). Migration
  `055_job_restart_from_steps.sql` adds `job.restart_from_steps TEXT[]`.
- **D7 UI**: one **Restart failed steps** button in the job header; the
  restart dialog shows `carried_failed_tolerated` in both modes (§7).
- **D8 No MCP tool and no `stroem-api` command** (neither has restart today).

## 3. Non-Goals

- Instance-level restart of a `for_each` loop: a loop with a failed instance
  reruns as a whole (the placeholder is the start step).
- Changing the cancelled-only loop rollup: a `for_each` placeholder whose
  instances are all `cancelled` (none `failed`) rolls up `completed`
  (CLAUDE.md § For-Each Loops, TODO.md). It is therefore NOT a start step.
  Documented, not fixed.
- Restarting `skipped` steps whose upstream failure is gone from the flow
  (see §4.3, last row).
- The stale-plan gap (TODO.md "Restart confirm dialog can act on a stale
  plan") applies to both modes unchanged; not fixed here.
- Bulk restart over many jobs (D3).

## 4. Semantics

### 4.1 Start steps

`compute_restart_set` becomes mode-aware. Let `F` be the task's current flow
(the pinned flow for a pinned source, unchanged) and `S` the source's step
rows with `loop_source IS NULL` (`restart.rs:53-57`, unchanged).

```
carried_status(row) = row.status            if row.status is terminal
                    = "cancelled"           otherwise        // base spec §4.2

start_steps(Step(x))    = [x]                                // validation unchanged (§4.2)
start_steps(AllFailed)  = sorted { s ∈ F | S has row r named s
                                          and carried_status(r) ∈ {failed, cancelled} }

roots       = start_steps ∪ { s ∈ F | S has no row named s }  // added steps, unchanged
restart_set = roots ∪ transitive_dependents(F, roots)          // unchanged
carry_set   = F \ restart_set                                  // unchanged
```

- `AllFailed` with an empty `start_steps` is an error,
  `RestartError::NoFailedSteps` → 400 `No failed or cancelled steps to restart
  in the current flow of task '<task>'`. The check is on `start_steps`, NOT on
  `roots`: a source with no failures but with a step added to the flow since
  is still a 400 (the user asked to rerun failures; there are none).
- A failed or cancelled source row whose name is not a key of `F` is ignored
  (it is not a flow step; the base spec drops such rows, §4.1).
- Loop instances (`loop_source` set) are never start steps. A placeholder
  whose rollup is `failed` is one.
- The selection reads only the source row's status. `continue_on_failure`
  is not consulted (D1).

### 4.2 Step mode is unchanged

`Step(x)` keeps today's validation (`x` contains `[` → `LoopInstance`; `x`
not in `F` → `UnknownStep`) and today's result, byte for byte. The status of
`x` in the source is not checked (restarting from a completed step stays
allowed).

### 4.3 Invariants and consequences

- **I1** In `AllFailed` mode `carried_failed` and `carried_failed_tolerated`
  are always empty: every row of `S` in `F` with carried status `failed` is a
  start step, so it is in `restart_set`, never in `carry_set`. Asserted by a
  unit test and a `debug_assert!` in the planner.
- **I2** In `AllFailed` mode no carried row has status `cancelled`: a row
  stored `cancelled` is a start step, and a non-terminal row — the only other
  source of a carried `cancelled` (`restart.rs:110-117`) — is a start step by
  D5. So the new job's status is decided by the rerun steps alone (and by
  carried `failed` rows, of which there are none by I1). Asserted with I1.
- **I3** `start_steps ⊆ restart_steps` in both modes.

| source row (in `F`) | `AllFailed` |
|---|---|
| `failed`, any `continue_on_failure` | start step |
| `cancelled` | start step |
| `pending` / `ready` / `claimed` / `running` / `suspended` (D5) | start step |
| `for_each` placeholder rolled up `failed` | start step (whole loop reruns) |
| `for_each` placeholder rolled up `completed` with only `cancelled` instances | carried `completed` (§3) |
| `skipped` (`condition`, `empty`) | carried, unless downstream of a start step |
| `skipped` (`unreachable`/`cascade`) below a start step | rerun (dependent) |
| `skipped` (`unreachable`) whose failed upstream is no longer in `F` | carried `skipped` (§3) |
| `completed` | carried, unless downstream of a start step |

For job `dd4c3a6a`: start steps = the five failed steps; restart set = those
five plus the twelve `unreachable` steps below `ml-prediction-master` plus any
other dependents; everything else (completed upstream, e.g. `build-sessions`)
is carried.

### 4.4 Job row

As the base spec §4.4, except the lineage columns:

| mode | `restart_from_step` | `restart_from_steps` |
|---|---|---|
| `Step(x)` | `x` | `NULL` |
| `AllFailed` | `NULL` | `start_steps` (sorted, non-empty) |

`restart_from_steps IS NOT NULL` therefore identifies an all-failed restart.
Single-step rows are written exactly as today, so every existing reader keeps
working. `source_type = 'restart'` and `source_job_id` as today.

## 5. Server

### 5.1 Endpoint

`RestartJobRequest` (`crates/stroem-server/src/web/api/jobs.rs:681-690`):

```rust
pub struct RestartJobRequest {
    #[serde(default)] pub from_step: Option<String>,
    #[serde(default)] pub all_failed: bool,
    #[serde(default)] pub dry_run: bool,
}
```

Shape rule, checked FIRST in the handler (before the 401), because it replaces
today's extractor rejection, which also runs before the handler:

| body | result |
|---|---|
| `from_step` set, `all_failed` false/absent | Step mode |
| `all_failed: true`, no `from_step` | AllFailed mode |
| both, or neither | 400 `Specify exactly one of from_step or all_failed` |

This changes the status of a body without `from_step` (e.g. `{"nope": 1}`)
from 422 to 400; `restart_endpoint_rejections`
(`crates/stroem-server/tests/restart_integration_test.rs:1459-1466`) is
updated. A wrong JSON type (`"all_failed": "yes"`) is still the extractor's
422. `from_step: ""` is Step mode and fails `UnknownStep` as today.

The rest of the order is unchanged (`jobs.rs:731-830`), and both modes share
it: 401 → 404 / 403 on the SOURCE job (`check_job_acl`) → 409 (not terminal)
→ 400 (not top-level, no `raw_input`) → then

- unpinned source: 400 workspace not loaded / task gone;
- pinned source (`resolve_source_pin`, `jobs.rs:770-790`): 400 `RefNotFound` /
  withheld `PinLoadFailed` / task missing at the re-resolved commit, **500**
  `PinUnavailable` (`jobs.rs:722-728`), then `require_task_run` on the folder
  the task declares at that commit — 404 "Task not found" / 403 "View-only
  access" (`jobs.rs:788-803`);

→ 400 plan error. `NoFailedSteps` is a plan error, so it comes after every
check above. The plan is computed on the pinned flow for a pinned source, as
today.

Responses (both modes; additive keys in **bold**):

- dry run, 200: `restart_steps`, `carried_over`, `carried_failed`,
  `carried_failed_tolerated`, **`start_steps`**.
- create, 201: `job_id`, `restart_steps`, `carried_over`, `carried_failed`,
  **`carried_failed_tolerated`**, **`start_steps`**.

`start_steps` is `[x]` in Step mode and the sorted start steps in AllFailed
mode. Added-step roots are not in it (they appear in `restart_steps`).

### 5.2 Planner

`crates/stroem-server/src/restart.rs`:

```rust
pub enum RestartFrom { Step(String), AllFailed }
pub struct RestartPlan {
    pub from: RestartFrom,          // new
    pub start_steps: Vec<String>,   // new, sorted
    pub restart_steps: Vec<String>,
    pub carried: Vec<Seed>,
    pub carried_failed: Vec<String>,
    pub carried_failed_tolerated: Vec<String>,
}
pub fn plan_restart(flow, source_steps, from: &RestartFrom) -> Result<RestartPlan, RestartError>;
pub fn compute_restart_set(flow, source_steps, from_step: &str)   // kept: = plan_restart(.., Step)
```

`RestartError` gains `NoFailedSteps`. Pure, no I/O, as today.

### 5.3 Creator

`job_creator::CreationMode::Restart { source, from_step, plan }`
(`crates/stroem-server/src/job_creator.rs:54-61`) drops `from_step`: the plan
carries its mode. `create_restart_job` (`job_creator.rs:255-297`) drops its
`from_step` parameter; its `debug_assert!` (`:266-271`) becomes I3. The
lineage match (`job_creator.rs:523-577`) derives the two columns of §4.4 from
`plan.from`. Seeding, settlement, file grants (A2 from the source) and pinning
are unchanged.

### 5.4 Repository

`stroem-db` `repos/job.rs`:

- New `pub enum RestartLineage<'a> { FromStep(&'a str), FromSteps(&'a [String]) }`.
  The `restart_from_step: Option<&str>` parameter of `create_with_parent`,
  `create_with_parent_tx` and `create_with_parent_tx_id` (`job.rs:326-460`)
  becomes `restart: Option<RestartLineage<'_>>`; the INSERT binds
  `restart_from_step` and `restart_from_steps` from it. Every caller that
  passes `None` compiles unchanged; the type makes "both set" unrepresentable,
  so no CHECK constraint is added (it would need a `NOT VALID` + `VALIDATE`
  pair on `job` for no gain).
- `JobRow.restart_from_steps: Option<Vec<String>>`; `job_columns!()`
  (`job.rs:15-19`) gains the column.
- `DerivedRunRow.restart_from_steps` and the `list_derived_runs` SELECT
  (`job.rs:1100-1115`).
- Hand-written column lists mapped to `JobRow` in tests
  (`crates/stroem-server/tests/integration_test.rs:20104, 20272, 22408`) gain
  the column; the stroem-db helper at
  `crates/stroem-db/tests/integration_test.rs:5881-5914` keeps its explicit
  INSERT list (the new column defaults to NULL).

### 5.5 API fields and redaction

- `JobDetailResponse.restart_from_steps: Option<Vec<String>>` and
  `DerivedJobRef.restart_from_steps` (`jobs.rs:250-320`).
- `redaction::JOB_IDENTIFIER_KEYS` (`crates/stroem-server/src/redaction.rs:127-150`)
  gains `restart_from_steps`: step names are identifiers, like
  `restart_from_step`. `walk_job_content` (`redaction.rs:598-626`) skips the
  whole value of an identifier key, so an array is safe. `derived_jobs` is
  skipped whole already.
- Hooks: no hook context carries `restart_from_step` today; none gains the new
  field.

## 6. Data Model

`crates/stroem-db/migrations/055_job_restart_from_steps.sql`:

```sql
-- Restart failed steps (spec 2026-11-02): the start steps of an all-failed
-- restart. NULL for every other job, including a single-step restart, which
-- keeps using restart_from_step.
ALTER TABLE job ADD COLUMN restart_from_steps TEXT[];
```

A nullable column with no default is a catalog-only change: no table rewrite,
no scan, no index, so no manual pre-run and no upgrade note
(`docs/src/content/docs/operations/`) is needed.

**Rolling deploy.** Migrations run at server start
(`crates/stroem-db/src/pool.rs:18-21`).

- Old replica, migrated schema: it lists columns explicitly (`job_columns!()`)
  and never writes the new one; it reads an all-failed restart row as
  `restart_from_step = NULL` and shows "restart of `<id>`" with no step.
- New UI against an old replica: `{"all_failed": true}` has no `from_step`, so
  the old extractor answers 422 and the dialog shows the error. The button
  works once every replica runs the release. Documented in the guide's
  rollout note; no flag.
- New replica before its own migration: impossible, it migrates at start.
- **An old binary cannot START after 055 is applied** — a pod restart of an
  old replica mid-rollout, or a rollback. `run_migrations`
  (`pool.rs:18-21`, called from `main.rs:66-71` before serving) uses the
  default sqlx migrator, which refuses an applied migration it does not know
  (`VersionMissing`). This is true of every migration since 001, not new here
  (054 behaves the same); it is not documented anywhere today. This change
  documents it once, in the guide's rollout note and a TODO.md entry
  ("rollback after a migration"), and does not change the migrator: a
  rollback of this release needs `DELETE FROM _sqlx_migrations WHERE version =
  55` (the column can stay; the old binary never reads it). No startup check
  is added (out of scope; the fix belongs to the migrator for all
  migrations).

## 7. UI

### 7.1 Button

Job page header (`ui/src/pages/job-detail.tsx:420-447`, next to **Re-run**):
**Restart failed steps**, shown when

```
canExecute === true && isTopLevelJob(job) && isTerminalJobStatus(job.status)
  && restartableFailedSteps(job.steps).length > 0
```

the same gate as the per-step **Restart from here**
(`job-detail.tsx:625`, `step-detail.tsx:65-67`) plus at least one step.
`restartableFailedSteps(steps)` (new, pure, `ui/src/lib/job-status.ts`):
names of steps with `loop_source === null` whose status is not `completed`
and not `skipped` — i.e. `failed`, `cancelled`, or non-terminal (D5), the same
rule as the server — sorted. It does not know the current flow (a step
removed from it), so the server's dry run stays authoritative and a 400
`NoFailedSteps` is shown as the error.

Click → dry run (`{all_failed: true, dry_run: true}`) → dialog → confirm →
create → navigate to the new job, exactly like the per-step flow.

### 7.2 Shared restart state

A hook `ui/src/hooks/use-restart.ts` owns the request / plan / pending /
confirming state that today lives in `step-timeline.tsx:509-550`, keyed by a
`RestartTarget = { kind: "step"; step: string } | { kind: "failed" }`.
`StepTimeline` and the job page each use it and each render one
`RestartDialog`. `restartJob(jobId, target, dryRun)` in `ui/src/lib/api.ts`
(`:466-476`) sends `{from_step, dry_run}` or `{all_failed: true, dry_run}`.
`RestartPlanResponse` gains `start_steps?: string[]` (optional: an older
server omits it).

### 7.3 Dialog

`RestartDialog` takes the target instead of `stepName`:

- Step: title `Restart {task} from {step}?` (unchanged).
- Failed: title `Restart the failed steps of {task}?`; description starts
  `Starts at {n} failed or cancelled step(s): {start_steps}.` then the
  existing rerun / carried-over sentence.
- Both modes: the existing `carried_failed` warning (unchanged), plus a new
  muted note when `carried_failed_tolerated` is non-empty:
  `{k} carried-over step(s) ended failed but continue_on_failure lets the job
  pass ({names}). They are not rerun.` In Step mode the note adds `Use Restart
  failed steps to rerun them.` (I1 means it never shows in Failed mode; the
  component does not special-case that.)

### 7.4 Lineage labels

- `jobLineage` (`ui/src/lib/job-status.ts:52-80`): `JobLineage` gains
  `steps: string[] | null` (`restart_from_steps` for a restart, else `null`).
- Header and Source card (`LineageLink`, `job-detail.tsx:45-63`): Step mode
  unchanged (`restart of <id> from <step>`); Failed mode `restart of <id> of
  failed steps <list>`.
- Later runs (`LaterRunsList`, `job-detail.tsx:69-97`): Step mode unchanged
  (`from <step>`); Failed mode `failed steps <list>`.
- `<list>` = `formatStepList(names, 3)` (new, pure): up to three names, then
  `+N more`; the full list goes in the element's `title`.
- `types.ts` `JobDetail` and `DerivedJobRef` gain
  `restart_from_steps?: string[] | null` (optional for an older server).

## 8. Tests

Unit (`restart.rs`):

- multi-root closure: two independent failed branches, both rerun with all
  their dependents; a shared completed upstream is carried;
- the `ai_maintain` shape: a failed `continue_on_failure` step with no
  dependents and a failed non-tolerant step with `unreachable` dependents;
  both are start steps, I1 holds;
- a failed `for_each` placeholder is a start step, its instances are not;
- a placeholder with only cancelled instances rolled up `completed` is carried;
- a `cancelled` row and each non-terminal status (D5) are start steps; an
  independent `failed` + `suspended` pair both rerun, and no carried row is
  `cancelled` (I2);
- zero start steps → `NoFailedSteps`, also when an added step exists;
- a failed source row no longer in the flow is ignored (and alone → error);
- Step mode output unchanged (existing tests stay as they are).

Integration (`crates/stroem-server/tests/restart_integration_test.rs`):

- all-failed dry run then real run on a two-branch failure: `start_steps`,
  `restart_steps`, empty carried failures, job row `restart_from_step` NULL and
  `restart_from_steps` set, `derived_jobs[].restart_from_steps` on the source;
- the new job runs to `completed` once the start steps pass;
- request shape: both / neither → 400; zero start steps → 400;
- existing `restart_endpoint_rejections` updated (422 → 400 for a body
  without `from_step`);
- a cancelled-mid-flight source (job row `cancelled`, a step row left
  `running`): the all-failed restart reruns it and the new job ends
  `completed`;
- pinned source in AllFailed mode (`pinned_rerun_restart_test.rs`): dry run
  and create use the flow at the re-resolved commit, and a View-only user on
  the commit's folder gets 403.

stroem-db: `migration_test::test_055_job_restart_from_steps` (column exists,
`text[]`, nullable) and a round trip of `RestartLineage::FromSteps` through
`create_with_parent_tx_id` → `JobRepo::get` / `list_derived_runs`.

Redaction unit test: `restart_from_steps` stays intact under a secret equal to
a step name.

UI (Vitest): `restartableFailedSteps`, `formatStepList`, `jobLineage` with
`restart_from_steps`; `RestartDialog` in both modes incl. the tolerated note;
the job page shows/hides the button by the gate; Later runs and lineage labels
for an all-failed restart.

Playwright: no restart spec exists in `ui/e2e/`; none is added.

## 9. Documentation

- `docs/src/content/docs/guides/rerun-and-restart.md`: new section
  "Restart failed steps", rollout note (§6).
- `docs/src/content/docs/reference/api.md` (`:500-555`): request/response
  fields.
- CLAUDE.md § Job Lineage; `CONTEXT.md` term **Start step**.
- `docs/internal/TODO.md`: tick "surface `carried_failed_tolerated`" in
  "Restart From Step follow-ups"; add instance-level restart and the
  cancelled-only loop as known limits of this feature.

## 10. Review Log

- Rev 1 (2026-11-02): initial.
- Rev 2 (2026-11-02), Codex round 1: (1) D5 kept, reasoning stated (a
  non-terminal row is a carried `cancelled` row, so D1 selects it; the
  alternative ends the new job `cancelled`); I2 argued from both sources of a
  carried `cancelled`; UI selector mirrors the server; tests for
  failed + suspended and a cancelled-mid-flight source. (2) Old binary cannot
  start after 055 / rollback: pre-existing for every migration, documented with
  the manual recovery, TODO entry, no migrator change. (3) §5.1 order now
  spells out the pinned branch (500 `PinUnavailable`, second `Run` check);
  pinned AllFailed test added.
