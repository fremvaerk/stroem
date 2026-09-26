# Dependency Gate — the upstream step decides — Design

Status: revision 4, Codex sign-off (round 3); awaiting user review
Ships in: 0.17.0 (breaking behaviour change, see §10)
Supersedes: `2026-09-09-continue-when-skipped-design.md` §2.3, §2.4 and its
Revision 4 (mixed-dependency rule)

## 1. Problem

Whether a step runs after its dependencies is decided by two flags,
`continue_on_failure` and `continue_when_skipped`. After four releases of
changes (0.16.2 skip reasons, 0.16.3 moved `continue_when_skipped` onto the
dependency, 0.16.5 tainted mixed dependencies), they are read from different
steps, and `continue_on_failure` is read from **both** sides:

| Flag | Read from | Decides | Code |
|---|---|---|---|
| `continue_on_failure` | the dependent | it may run after a failed / cancelled / `unreachable` dependency | `cascade.rs:283-285, 500, 537` |
| `continue_on_failure` | the failed step itself | its failure does not fail the job | `settlement/settle.rs:42-51` |
| `continue_on_failure` | a `for_each` placeholder | instance failures do not fail the loop | `cascade.rs:375-424` |
| `continue_when_skipped` | the skipped dependency | the dependent is not cascade-skipped when ALL its dependencies were skipped | `cascade.rs:323-329` |

The all-dependencies-skipped rule mixes both sides:
`bypass = all_deps_cws && (!tainted || S.continue_on_failure)`.

Consequences seen in production and in the docs:

1. **A job goes green while a step silently never ran.** `a` fails with
   `continue_on_failure`, its dependent `b` has no flag → `b` is skipped
   `unreachable`, the job is `completed` (`a`'s failure is tolerated). Prod job
   `4bc71c31` (2026-09-21, `jobs/recalc-pipeline`, `ai_maintain` → `ai_sources`)
   published nothing and reported success.
2. **A step runs on top of a failure its author meant to stop.** Prod job
   `9691df79` (2026-09-25, same task): `ml-prediction-beta` failed;
   `ml-impressions-beta` carries `continue_on_failure` and **ran**, although its
   YAML comment says "a failed prediction still skips impressions".
3. **The docs contradict the server.** `guides/retry.md:238-252` and
   `reference/workflow-yaml.md:666-682` put the flag on the failing step and
   promise that the next step runs; on the server it is skipped.
   `guides/workflow-basics.md:282` calls the flag "similar to GitHub Actions'
   `continue-on-error`", whose meaning is the failing-step one.
4. **The CLI disagrees with the server.** `stroem run` (`run.rs:264-335`) reads
   only the failing step's flag and aborts the whole run on an untolerated
   failure.
5. **Stale model doc.** `FlowStep.continue_when_skipped`
   (`models/workflow.rs:399-402`) still describes the 0.16.2 placement.

The user's own workflows (`jobs/.workflows/recalc/pipeline.yaml`,
`ai-traffic-estimator/.workflows/daily.yaml`) are already written as if the
flag on the failing step decided — the server does not do that.

## 2. Decisions

Decided with the user, 2026-09-26.

### 2.1 Only the dependency's own flags decide

Whether step `S` runs depends on each dependency `d`'s final state and on
**`d`'s** flags. `S`'s own `continue_on_failure` / `continue_when_skipped`
play no part in whether `S` runs.

- `continue_on_failure` on `X`: whatever went wrong at `X` — it failed, was
  cancelled, or was skipped because something upstream failed — `X`'s
  dependents treat `X` as satisfied, and the failure (`X`'s own, or the
  upstream one that reached `X`) does not fail the job (§2.4).
- `continue_when_skipped` on `X`: if `X` was skipped by choice — its own
  `when`, an empty `for_each`, or because its own dependency was skipped by
  choice — `X`'s dependents treat `X` as satisfied.
- Neither flag makes `X` itself run.

### 2.2 Per-dependency verdict

| `d` final state | Verdict |
|---|---|
| `completed` | Pass |
| `skipped` with reason `condition` / `empty` / `cascade` | Pass if `d.continue_when_skipped`, else BlockSkip |
| `failed` / `cancelled` / `skipped` with reason `unreachable` / `skipped` with NULL or unknown reason | Pass if `d.continue_on_failure`, else BlockFail |
| any non-terminal status, or no row | Pending |

A NULL skip reason (rows from before migration 046, restart carries) is read
as `unreachable`, as since 0.16.2.

### 2.3 Combining the verdicts — strict AND

For `S` with dependencies `d1..dn`, in this order:

1. Any BlockFail → `S` is skipped `unreachable` ("upstream failed"). This is
   decided immediately, even while other dependencies are still pending — a
   failure dominates.
2. Otherwise any Pending → wait.
3. Otherwise any BlockSkip → `S` is skipped `cascade` ("upstream skipped").
   Decided only once every dependency is terminal, so a later failure is never
   hidden as a plain skip.
4. Otherwise (all Pass, or no dependencies) → the gate is open: `S`'s own
   `when` is evaluated, then promotion / `for_each` expansion, as today.

There is **no automatic convergence**: a dependency skipped by choice blocks
its dependents even when a sibling completed. An if/else merge needs
`continue_when_skipped` on each branch step. This reverses Revision 4 of the
2026-09-09 spec ("`condition`, `empty` and `cascade` skips still count as
satisfied") — one rule instead of a rule plus an exception.

### 2.4 Job status — a failure fails the job only if nothing catches it

`continue_on_failure` works like a `catch`: a failure travels down the DAG
(its dependents are skipped `unreachable`) until it reaches a step that
carries the flag — the failed step itself or any step below it. There it is
caught: that step's dependents run (§2.2) and the failure no longer counts
against the job.

A failure at step `f` is **caught** when every path from `f` to the end of
the flow passes through a step with `continue_on_failure` (`f` included):

```
caught(s) = s.continue_on_failure
         || (dependents(s) is non-empty && every dependent d: caught(d))
```

`dependents` comes from the flow definition. A step that is not in the flow
has no dependents and no flag, so it is never caught.

**`caught` is a structural policy over the current flow**, not a statement
about which rows ran. In an ordinary run it coincides with "an uncaught
failure reached a step nothing depends on", because every dependent of an
uncaught failure is skipped `unreachable` (BlockFail dominates, §2.3). Restart
carries and R0 adoption can leave dependents that already completed; the
structural answer still decides. This matches 0.16, where the failing step's
own flag — also a property of the flow — decided.

Settlement (`decide`), in order:

1. **Uncaught failure → `failed`.** Any row with status `failed` whose step
   is not in `caught_steps(current flow)`. A loop instance row
   (`loop_source` set, name `p[i]`) is judged by its placeholder `p`. A failed
   row whose step is missing from the current flow is uncaught (as today,
   `settle.rs:42-51`; test `cascade.rs:2164-2183`).
2. Otherwise any `cancelled` row, loop instances included → `cancelled`
   (unchanged).
3. Otherwise → `completed`, output unchanged (`settle.rs:69-96`).

**Skipped rows never decide job status**, whatever their reason. The failure
or cancellation that caused an `unreachable` skip is itself a row and is
judged under rule 1 or 2. So a cancelled child job still makes its parent
job `cancelled`, not `failed` (a cancelled step skips its dependents as
`unreachable`, but no `failed` row exists). And NULL / unknown skip reasons
cannot change job status: the gate reads them as failures, conservatively, so
their dependents do not run, but settlement ignores them.

A caught failure keeps the job `completed` and stays visible: hook
`failed_steps[]` (new field `tolerated`, §5) and the job page banner (§8).

Before this change a failure was tolerated only by the failing step's own
flag (`settle.rs:42-51`). That rule is the special case "caught at `f`
itself".

### 2.5 Cleanup after a failure → hooks

There is no dependent-side "run even if upstream failed". A step that must run
after a failure while the job still fails (cleanup, notify) belongs in
`on_error` / `on_cancel` hooks. No usage of the old pattern was found in the
user's workflows.

### 2.6 Field names unchanged

`continue_on_failure` and `continue_when_skipped` keep their names; their
meaning changes (as 0.16.3 did for `continue_when_skipped`). No alias.

### 2.7 One implementation for server and CLI

A pure function in `stroem-common` decides the gate; the server cascade and
`stroem run` both call it (§4, §6).

## 3. Worked examples

### 3.1 Prod job 9691df79 (`jobs/recalc-pipeline`, 2026-09-25)

`ml-prediction-beta` (no flags) → `ml-impressions-beta`
(`continue_on_failure`) → `merge-ml` (depends on impressions master, beta,
stage) → `agg-sessions` → …

| Step | Before (0.16.x) | After |
|---|---|---|
| `ml-prediction-beta` | failed | failed |
| `ml-impressions-beta` | **ran** (its own flag, dependent-side) | skipped `unreachable` — its own flag does not make it run |
| `merge-ml` | ran | runs — `ml-impressions-beta`'s skip is failure-class and it carries `continue_on_failure` → Pass |
| job | failed | **completed** — `ml-prediction-beta`'s only dependent, `ml-impressions-beta`, catches the failure (§2.4); the failure is listed as tolerated |

`continue_on_failure` on `ml-prediction-beta` itself would instead let
impressions-beta run on the failed prediction — not what the author wants.

### 3.2 Chain `a → b → c`

| `a` | `a` flags | `b` flags | `b` | `c` | job |
|---|---|---|---|---|---|
| failed | — | — | skipped `unreachable` | skipped `unreachable` | failed (uncaught) |
| failed | cof | — | runs | depends on `b` | completed if the rest succeeds (caught at `a`) |
| failed | — | cof | skipped `unreachable` | runs | completed if `c` succeeds (caught at `b`) |

Branching: `a` fails (no flag) with dependents `b` (cof) and `d` (no flag,
nothing depends on `d`) → `b` catches, `d` does not → the job fails: the
failure escaped through `d`.
| skipped `condition` | — | — | skipped `cascade` | skipped `cascade` | completed |
| skipped `condition` | cws | — | runs | depends on `b` | completed |
| skipped `condition` | — | cws | skipped `cascade` | runs | completed |
| skipped `condition` | — | cof | skipped `cascade` | skipped `cascade` | completed |

### 3.3 Multiple dependencies

| `m` depends on | Verdicts | `m` |
|---|---|---|
| `x` completed, `y` skipped `condition` (no cws) | Pass, BlockSkip | skipped `cascade` |
| `x` completed, `y` skipped `condition` (cws) | Pass, Pass | runs |
| `x` failed (no cof), `y` running | BlockFail, Pending | skipped `unreachable` now |
| `x` skipped `condition` (no cws), `y` running | BlockSkip, Pending | waits |
| … then `y` fails (no cof) | BlockSkip, BlockFail | skipped `unreachable` |
| `x` failed (cof), `y` skipped `condition` (cws) | Pass, Pass | runs |

## 4. The shared gate (`stroem-common`)

New module `crates/stroem-common/src/gate.rs`, registered in `lib.rs`.

- `SkipReason` moves from `cascade.rs:36-60` to
  `stroem-common/src/models/job.rs` (beside `StepStatus`; `as_str`,
  `FromStr`, `Display`, lowercase serde). `cascade.rs` re-exports it so
  existing imports (`cascade_apply_test.rs`, `orchestrator_test.rs`) keep
  working.
- Types and functions:

```rust
pub enum DepOutcome { Pending, Completed, Failed, Cancelled, Skipped(Option<SkipReason>) }
impl DepOutcome { pub fn from_row(status: &str, skip_reason: Option<&str>) -> Self }

pub enum Verdict { Pass, BlockSkip, BlockFail, Pending }
pub fn verdict(outcome: DepOutcome, dep: Option<&FlowStep>) -> Verdict

pub enum Gate { Open, Wait, Skip(SkipReason) }   // Skip carries Cascade | Unreachable only
pub fn gate(
    depends_on: &[String],
    flow: &HashMap<String, FlowStep>,
    outcome_of: impl Fn(&str) -> DepOutcome,
) -> Gate
```

- `gate` never receives the dependent's own `FlowStep` — the signature
  enforces §2.1.
- `pub fn caught_steps(flow: &HashMap<String, FlowStep>) -> HashSet<String>`
  — the steps `s` with `caught(s)` (§2.4), one memoized walk over the
  (validated, acyclic) flow; a step whose dependents are missing from the
  flow counts as having none. Used by settlement, hooks, restart preview and
  the CLI exit code.
- `from_row`: every non-terminal status (`pending`, `ready`, `claimed`,
  `running`, `suspended`) and any unknown status → `Pending`; an unknown
  `skip_reason` string → `Skipped(None)` (blocks as a failure).
- A dependency with no row → `Pending` (today's `_ => false`). A dependency
  missing from `flow` → both flags read as `false`.

## 5. Server cascade (`crates/stroem-server/src/cascade.rs`)

- Add `Snapshot::outcome(name) -> DepOutcome`; `Snapshot::skip_reason` is
  routed through it.
- Delete `dep_tainted`, `deps_satisfied`, `all_deps_skipped`, `tainted`,
  `all_skipped_decision`, `any_dep_carries_failure` (:273-348).
- Phases keep their order (P0 → context A → P1 → P2 → context B → P3):

| Phase | Rule | Acts on |
|---|---|---|
| P1 `phase_promote` | R1 | `Skip(Cascade)` → skip `cascade`; `Skip(Unreachable)` **only when every dependency is skipped** → skip `unreachable` (no context needed, as today) |
| P1 | R2 | `Open` → own `when` → promote / skip `condition` / fail |
| P1 | — | `Wait`, other `Skip(Unreachable)` → nothing |
| P2 `phase_skip_unreachable` | R3 | remaining `Skip(Unreachable)` → skip `unreachable` |
| P3 `phase_placeholders` | R0 | adopt first, bypasses the gate (unchanged) |
| P3 | R4 | `Wait` → nothing; `Skip(r)` → skip with `r`; `Open` → `when` → expand |

  **Why this split:** pass timing is observable. Context B (the render
  context for P3's `when` / `for_each`) is built after P2
  (`cascade.rs:711-723`); skipped rows enter the template context and pending
  rows do not (`render_context.rs:272-292`). So the phase in which a skip
  lands decides what a placeholder's `when` can see in the same pass (Codex
  round 1). 0.16's R1 skipped all-dependencies-skipped steps in P1 (cascade or
  unreachable) and its R3 skipped the rest in P2; the split above follows the
  same division.

  **What is and is not preserved.** The phase order and the legacy timings
  pinned in §12.2 are preserved. The changes this design makes — flag
  semantics (the dependent's own flag is no longer read), strict-AND
  convergence, and unknown skip reasons read as failure-class — can move a
  transition to an earlier or later pass and so change what a later
  template sees. Known cases (Codex rounds 2-3), all accepted and pinned:
  - `x` skipped with an unknown non-NULL reason → `a` → `b`, no flags: 0.16
    read the reason as untainted (`cascade.rs:273-278`), skipped `a` as
    `cascade` in P1 and left `b` pending in P2; now `a` is skipped
    `unreachable` in P1 and `b` in P2, so an independent placeholder whose
    `when` tests `b is defined` sees `b` one pass earlier.
  - `x` skipped `unreachable` → `a` → `b` (`b` has `continue_on_failure`):
    0.16 left `b` pending in P2 (its own flag, `cascade.rs:500`) and skipped it
    in the next pass's P1; now `b` is skipped in P2 of the same pass (failure
    dominance). An independent placeholder whose `when` tests `b is defined`
    now sees `b` one pass earlier.
  - A placeholder `L` with `continue_on_failure`, depending on `x` skipped
    `unreachable` and a running `y`: 0.16 waited (`cascade.rs:536-543`); now
    `L` is retired `unreachable` immediately.
  Exact legacy timing would need a compatibility rule that reads the
  dependent's flag again — the thing this design removes — so it is not
  offered. The one new shape — completed + choice-skip without
  `continue_when_skipped` → `cascade` — has no earlier timing and is decided
  in P1 with the other cascade skips.
- P0 (R5 sequential advance, R6 rollup) is unchanged: it already reads the
  placeholder's own `continue_on_failure`.
- Guards, `apply`, `execute` and the three-attempt re-run: unchanged.

Job status and its readers switch from "the failed step's own flag" to
`caught_steps` (§2.4):

- One helper in `stroem-common/src/gate.rs`, used by every reader below:
  `fn flow_step_name(row_name, loop_source) -> &str` (an instance row →
  its placeholder), and `fn failure_caught(caught: &HashSet<String>,
  row) -> bool`.
- `settlement/settle.rs::decide` (:42-67): rule 1 of §2.4 replaces
  `tolerated` with `failure_caught`; rules 2 and 3 unchanged. The `completed
  with {n} tolerable failure(s)` log line is unchanged.
- `settlement/hooks.rs::build_hook_context` (:473-507): `FailedStepInfo` gains
  `tolerated: bool` (= `failure_caught`, instance rows judged by their
  placeholder, so a failed `p[0]` under a completed placeholder `p` with the
  flag is `tolerated: true`). `continue_on_failure` keeps meaning "this row's
  own flow step has the flag". `failed_steps` membership is unchanged (failed
  rows); a `failed` job always has at least one uncaught failed row, so
  `error_message` stays non-null. Wire format: one added field.
- `restart.rs:113-119`: `carried_failed_tolerated` / `carried_failed` split by
  `failure_caught` against the current flow instead of the carried step's own
  flag. Carried skipped rows stay out of both lists — consistent with §2.4
  (skipped rows never decide job status). API (`web/api/jobs.rs:724-730`) and
  the dialog warning (`ui/src/components/restart-dialog.tsx:60-72`) keep
  their contract.
- Terminal side effects follow the status: hook kind and task-level retry
  (`terminal.rs:166-191`) and the completion metric's `status` label
  (`terminal.rs:42-57`). No code change there; §10 and §12 cover the
  behaviour change.

## 6. CLI local runner (`crates/stroem-cli/src/local/run.rs`)

- `run_dag` keeps `outcomes: HashMap<String, DepOutcome>` instead of the
  `completed` / `skipped` sets; each round it processes the undecided steps
  whose `gate` is not `Wait`, sorted for determinism.
- Per step: gate → own `when` → `for_each` → execute. Today the CLI evaluates
  `when` before the dependency check (:165 before :184) — aligned with the
  server.
- An untolerated failure no longer aborts the run (`failed = true; break` at
  :285, :322, :333): independent branches continue, dependents are skipped
  `unreachable`. The CLI now produces `unreachable`.
- A `when` or `for_each` evaluation error fails that step instead of aborting
  the run (server behaviour).
- Delete `cascade_skip` (:569) and its calls (:175, :200).
  `dag::ready_steps` stays (examples, README and common tests use it).
- **Template context becomes outcome-aware** (`build_render_context`,
  :430-442). Today it inserts every stored output whatever the step's status;
  that was harmless while a failure aborted the run, but once the run
  continues a dependent could read a failed step's output (a partial loop
  array, a failed command's `OUTPUT:` lines) that the server masks. Mirror
  `render_context.rs:272-292`: `completed` → its output; `skipped` → `output:
  null`; `failed` → `output: null` plus `error` when a message exists. A
  failed loop's partial array is never exposed.
- **Exit code = job outcome, not a failure count.** Today success means
  `summary.failed == 0` (`run.rs:85`, `local/mod.rs:56-58`). New:
  `RunSummary` gets an `outcome` computed with rule 1 and rule 3 of §2.4
  (`failure_caught` over failed steps); exit 0 only for `completed`, 1 for
  `failed`. A run whose failures were all caught exits 0 and prints them as
  tolerated. The CLI has no step-level cancellation: Ctrl-C stays the existing
  error path outside `RunSummary` (`run.rs:145-157, 229-230`) and exits
  non-zero, unchanged.
- **Summary counts stay diagnostic and are not fixed here.** Their existing
  quirk is left as is: successful loop iterations count as one completed step
  while failed iterations count individually (`run.rs:252-254, 282,
  346-349`). Recorded in `docs/internal/TODO.md` instead.

## 7. Validation (`crates/stroem-common/src/validation.rs`, ~:298)

- New warning: for a step with two or more `depends_on`, each dependency that
  has `when` or `for_each` and no `continue_when_skipped` →
  "Task 't' step 'm' will be skipped whenever 'd' is skipped (add
  continue_when_skipped: true to 'd' to let 'm' run)". Catches the if/else
  merges §2.3 changes; otherwise they would silently skip and the job would
  complete (and fire `on_success`).
- The existing "`continue_when_skipped` with no dependents" warning stays.
- No warning for `continue_on_failure` on a step with no dependents — the flag
  still matters for job status.

## 8. UI

- `ui/src/lib/skip-reason.ts`: `cascade` → "Skipped: a dependency was skipped
  and does not let its dependents run."; `unreachable` → "Skipped: an upstream
  step failed or was cancelled." (keep the pre-0.16.2 NULL note). Badges
  ("upstream skipped", "upstream failed") unchanged. Update
  `__tests__/skip-reason.test.ts`.
- `ui/src/pages/task-detail.tsx:434`: "continue on failure" → "catches
  failures (dependents run)"; `continue_when_skipped` label stays
  ("dependents continue when skipped").
- `ui/src/pages/job-detail.tsx:324-336` banner: "N step(s) failed with
  continue_on_failure: …" → "N step(s) failed; the failures were caught by
  continue_on_failure: …". On a `completed` job every failed step is caught by
  definition, so no new API field is needed.

## 9. Edge cases

- **`for_each` placeholder as a dependency.** Running → Pending. Failed
  (rollup, `when` error, expression error, too many items, timeout) or
  cancelled → Pass only with the placeholder's own `continue_on_failure`; a
  loop with the flag never rolls up as failed, so its dependents pass.
  Skipped `empty` → needs its own `continue_when_skipped`.
- **`for_each` placeholder as a dependent.** R0 adopt precedes the gate; R4
  still needs a render context even to retire (test :1636).
- **Loop instances** are neither gate inputs nor gate subjects (not in `flow`).
- **`type: task` steps.** Dispatch failure (`fail_task_step`) or a cancelled
  child (`propagate.rs:144`) → the task step's own flag decides.
- **Approval steps.** `suspended` is Pending; reject / timeout → the approval
  step's own flag decides.
- **Retry.** A step being retried is `ready`, never observable as `failed`
  (`fail_or_retry`), so failure-dominance cannot fire on a retrying step.
- **Recovery sweeps / timeouts** go through `step_failed`; same gate.
- **Job cancel** cancels pending steps and the cascade does not run on a
  terminal job, so a flag on a cancelled step never revives dependents after a
  job cancel.
- **Restart carries.** A carried cancelled row has a NULL reason → blocks as a
  failure unless the current flow gives that step `continue_on_failure`.
  Flags are read from the live config.
- **Missing dependency row after a config change** → `Wait` forever
  (pre-existing, `docs/internal/TODO.md`).
- **Hook jobs** (`build_step`, single step, no dependencies) are unaffected.

## 10. Behaviour changes (release notes / upgrade page)

| Workflow shape | 0.16.x | 0.17 | Fix |
|---|---|---|---|
| `a` fails (cof), `b` depends on `a` (no flag) | `b` skipped, job green | `b` runs, job green | — (was the bug) |
| `a` fails (no flag), `b` depends on `a` (cof) | `b` runs, job failed | `b` skipped `unreachable`, `b`'s dependents run, job **completed** (caught at `b`) | cof on `a` if `b` must run; cleanup that must not hide the failure → `on_error` |
| `a` fails (no flag), nothing depends on `a` | job failed | job failed | — |
| if/else merge, branches without cws | merge runs | merge skipped `cascade` | cws on each branch step |
| merge after `x` completed + `y` skipped by choice (no cws) | merge runs | merge skipped `cascade` | cws on `y` |
| `stroem run`, untolerated failure | run aborts | independent branches continue | — |

A merge that is now skipped also loses its entry in the job output (job
output = outputs of the flow's terminal steps).

A job whose failure is now caught downstream (row 2) ends `completed`
instead of `failed`, and every terminal side effect follows the status
(`terminal.rs:166-191`, `:42-57`): `on_success` fires instead of `on_error`,
no task-level retry job is created even when the task has `retry`, and
`stroem_jobs_completed_total` counts it under `status="completed"`.

Unchanged on purpose: a cancelled step (e.g. a cancelled child job under a
`type: task` step) still makes the job `cancelled` — `on_cancel`, no retry —
even though its dependents are skipped `unreachable` (§2.4: skipped rows
never decide job status).

## 11. Documentation

Rewrite (examples marked wrong under the new rule in the inventory):

- `guides/conditionals.md`: overview bullets (:14-15), YAML example (:29-38),
  convergence pattern (:100-129), "Running after a skipped branch" (:133),
  cleanup example (:155-169), skip-reason section (:177-186, "Changed in
  0.17" note), if/else (:241-264), optional step (:300-326), example workflow
  (:359-415), :417.
- `guides/workflow-basics.md`: :157, :227, :266-287 (one meaning; cleanup →
  `on_error`).
- `reference/workflow-yaml.md`: flag rows :406-407, :442, :444, :532;
  :666-682 becomes correct.
- `guides/retry.md`: :234-252 becomes correct; :515-538 move the flag to the
  `process` loop.
- `guides/templating.md:150-166`, `examples/ci-pipeline.md` (notify → hooks),
  `guides/event-sources.md:582, :605` (→ `on_error`; putting the flag on the
  consumer would change `restart_policy: on_failure`), `guides/action-types.md:635,
  :649`, `guides/loops.md:169-173` (note), `guides/hooks.md` (cleanup
  pointer; `failed_steps[].tolerated` in the field table at :58, also
  `reference/workflow-yaml.md:793`), `reference/cli.md` (continues independent branches),
  `reference/worker-api.md:159`, `operations/migration-046.md` (:25-32).
- New `operations/upgrade-0-17-dependency-flags.md` (sidebar in
  `docs/astro.config.mjs`): §10's table, §3.1 as the example.
- `models/workflow.rs:397-404`: doc comments for both flags.
- `CLAUDE.md` § Step Cascade (name the gate) and § Conditional Flow Steps
  (replace the formula and the "CLI never produces `unreachable`" note);
  `CONTEXT.md` glossary: *Dependency gate*, *Verdict*.
- `docs/scripts/generate-llms-txt.ts:55` → regenerate `docs/public/llms*.txt`
  and `llms/*.md`.
- `settlement/dispatch.rs:42` comment; `docs/internal/TODO.md`; a
  "superseded by 2026-09-26-dependency-gate-design.md" line on the
  2026-09-09 spec.

## 12. Tests

### 12.1 `stroem-common` (new)

- Table-driven `verdict` over outcome × (`continue_on_failure`,
  `continue_when_skipped`).
- `gate`: failure dominates pending; cascade waits for pending, becomes
  unreachable if the pending one fails; no auto-convergence; empty
  `depends_on` → Open; missing row → Wait; dependency missing from flow; NULL
  and unknown skip reason → BlockFail.
- Validation: merge warning fires / does not fire with cws / single-dependency.
- `caught_steps`: leaf with / without the flag; chain caught mid-way; branch
  where one dependent catches and another does not; diamond; dependency
  missing from the flow.

### 12.1a Settlement / hooks / restart (unit)

- `settle.rs` (`decide`): failure caught downstream → `completed`; failure
  escaping through one branch → `failed`; cancelled step + its `unreachable`
  dependents, no failed row → `cancelled`; skipped leaf with NULL or unknown
  reason and no failed row → `completed`; failed instance `p[0]` under a
  flagged placeholder → `completed`; failed row whose step is missing from the
  flow → `failed`; cancelled instance under a completed placeholder →
  `cancelled` (unchanged); the existing :238/:246/:294 cases still hold.
- `hooks.rs`: `tolerated` true for a failure caught downstream and for a
  failed instance under a flagged placeholder; false for the uncaught one.
- `restart.rs:213`: split follows `failure_caught`; carried skipped rows
  (`unreachable`, NULL, unknown) appear in neither list.

### 12.2 `cascade.rs` unit tests

Move the flag to the failed dependency: :1244, :2684, :2915. Flip
expectations: :1455, :2699, :2749 (second half), :2902. Extend :2377 (merge
with and without cws). Fix the comment of :2583. :1286/:1301 still pass (NULL
reason, all dependencies skipped → P1, as today). New: failure dominates while
a sibling runs; a cascade-type block waits for a running sibling and becomes
`unreachable` if it fails; a failed placeholder with its own flag lets
dependents promote; a placeholder with mixed dependencies is skipped
`cascade`.

Pass timing (§5), each asserting the full plan of one `run`:
- Codex counterexample: `x` skipped `unreachable` → `a` → `b`, plus an
  independent placeholder `p` with `when: "{% if b is defined %}true{% else
  %}false{% endif %}"` → `a` skipped in P1, `b` in P2, `p` expands (as 0.16).
- Same shape with `x` **failed** → `a` skipped in P2, `b` still pending at P3,
  `p` skipped `condition` (as 0.16).
- Accepted timing changes (§5), expected behaviour pinned: `b` with
  `continue_on_failure` in the first shape → `b` skipped in P2 of the same
  pass, `p` expands; placeholder `L` (flag) with deps `x` `unreachable` + `y`
  running → `L` retired `unreachable` in the first pass; `x` skipped with an
  unknown reason, no flags → `a` `unreachable` in P1, `b` in P2, `p` expands.

### 12.3 `orchestrator_test.rs` (container)

Move flag: :449, :1877. Keep :1501 unchanged (it proves a dependent's own
flag cannot bypass a choice skip) and add a variant with the flag on the
skipped dependency. Flip and add a flagged sibling: :906, :1165, :1326, :1387,
:1618. Add variants: :1457 (flag on the cancelled dependency), :2299 (flag on
the unreachable dependency → dependent ready).

New, each driven through full settlement (`Settlement::advance`, repeated
advancement) and asserting job status, hook kind fired, whether a task-retry
job was created (task with `retry`), and `stroem_jobs_completed_total` status
label counted exactly once:
- Replay of §3.1: prediction fails → impressions `unreachable` → merge runs
  → job **`completed`**, `on_success`, no retry job, `failed_steps[0].tolerated`.
- Same with a second, unflagged dependent of the prediction step that nothing
  depends on → job `failed`. With `retry` budget left: a retry job is created
  and no terminal hook fires (`terminal.rs:185-191`, `settlement/mod.rs:310-325`);
  on the exhausted final attempt: `on_error`, no retry job. If retry creation
  itself fails: the existing hook fallback fires `on_error`
  (`settlement/mod.rs:338-346`). Metric counted once per job (attempt).
- Mixed cancelled + failed: a cancelled step and a failed step in independent
  branches — failure caught → `cancelled`; failure uncaught → `failed`
  (pins rule 1 before rule 2).
- Child job cancelled under a `type: task` step with an unflagged dependent →
  parent job `cancelled`, `on_cancel`, no retry job (Codex high finding).
- Approval rejected, caught by a flagged dependent → `completed`.
- Step retries exhausted, caught downstream → `completed`.
- Failed sequential `for_each` caught by a flagged dependent → `completed`;
  a cancelled instance under a completed placeholder → `cancelled`.
- R0 adoption beside an uncaught failure → `failed`.
- Restart counterexample: carried `a` failed (no flag) → `b` completed, flag
  removed from `b` in the current flow, restart an independent `z` → `failed`
  (structural policy, §2.4).
- Failed placeholder absent from the current flow → `failed`.

### 12.4 `integration_test.rs` (container)

:10521 (flag → `step1`; job becomes `completed`), :25679 (flag → `step-a`),
:31336 (flag → `spawn`). Others unchanged.

### 12.5 CLI (`run.rs`)

Delete the `cascade_skip` unit tests (:947, :970, :992, :1308). New: an
independent branch runs after an untolerated failure and the dependent is
`unreachable`; completed + condition-skip without cws → `cascade`; if/else
merge with cws on both branches runs; a `when` error fails the step, not the
run. Output parity with the server: a failed loop `L` → `B` (flag) → `C`
reading `{{ L.output }}` renders `null` (not the partial array); a failed
step exposes `error`; contrast with a loop that has its OWN flag, which
completes and exposes its aggregate (nulls for failed iterations), as the
server's rollup does (`cascade.rs:418-435`). Exit status through the real binary (subprocess test):
0 when every failure is caught, 1 for an uncaught one; a run interrupted with
Ctrl-C still exits non-zero (unchanged).

### 12.6 UI / E2E

- `skip-reason.test.ts` updated; `step-timeline.test.tsx:352` and
  `step-detail.test.tsx:159` stay green if the badges are kept.
- `workspace/.workflows/failing.yaml:36-47` (`fail-continue`): flag moves to
  `step-fail`; `ui/e2e/jobs.spec.ts:212-241` expects `completed`.
- `tests/e2e-workspace/conditional.yaml` + `tests/e2e.sh`: add `plain-merge`
  (→ skipped `cascade`) and `cws-merge` (→ `completed`).

## 13. Rollout

- 0.17.0 (minor bump: breaking). Release notes lead with §10.
- No migration: no schema change; `skip_reason` values are unchanged.
- Existing jobs in flight at upgrade are re-evaluated under the new gate on
  their next cascade (the cascade is re-run on every step transition). A job
  mid-flight can therefore see a merge skipped that 0.16.x would have run;
  acceptable, noted in the release notes.
- User repos to review after upgrade (not part of this change):
  `jobs/.workflows/recalc/pipeline.yaml` (most flags already assume the new
  meaning; `ai_sources`' comment becomes stale), and
  `ai-traffic-estimator/.workflows/*.yaml`.

## 14. Non-goals

- A dependent-side "run always" / trigger-rule field (§2.5).
- Tekton-style result-aware skipping (skip only dependents that read the
  skipped step's output) — needs template analysis.
- A separate "completed with warnings" job status.
- Renaming the flags or adding aliases.

## Appendix A. Other systems (official docs, 2026-09-26)

| System | Failure knob (side) | Skipped upstream, default | Tolerated failure → run | Cleanup |
|---|---|---|---|---|
| Tekton | `onError: continue` (upstream only) | `runAfter` dependents run; result consumers skipped | Succeeded, "Failed: 1 (Ignored: 1)" | `finally` |
| Buildkite | `soft_fail` (up) + `allow_dependency_failure` (down) | satisfied | build not failed | — |
| GitLab CI | `allow_failure` (up) + `when: on_failure/always` (down) | skipped earlier stage = successful | passed, orange job | `when: always` |
| GitHub Actions | `continue-on-error` (up) + `if: failure()/always()` (down) | dependents skipped unless `if:` | green (`outcome` ≠ `conclusion`) | `if: always()` |
| Azure Pipelines | `continueOnError` (up) + `condition:` (down) | dependents skipped (`succeeded()`) | `SucceededWithIssues` | `always()` |
| Argo | `continueOn` (up) or `depends: A.Failed` (down) | satisfied (`A` = Succeeded ‖ Skipped) | unverified | `onExit` |
| Airflow | `trigger_rule` (downstream only) | cascades (`all_success`) | green if leaves succeed | setup / teardown |

Failure handling follows Tekton's shape: an upstream flag plus a separate
cleanup construct (our `on_error` hooks). Skip handling is the cascading family
(Airflow, GitHub, Azure), but the merge fix sits on the upstream branch step
(`continue_when_skipped`) instead of a downstream condition — consistent with
§2.1, unusual among peers. Tolerated failures stay green and visible (hook
`failed_steps[].continue_on_failure`, UI badge), like GitHub and Tekton.

Sources: airflow.apache.org (core-concepts/dags, setup-and-teardown,
dag-run), argo-workflows.readthedocs.io (enhanced-depends-logic, fields,
exit-handlers), docs.github.com (workflow-syntax, use-jobs, expressions,
contexts), docs.gitlab.com/ci/yaml (allow_failure, when, needs),
buildkite.com/docs/pipelines/configure (dependencies, command-step),
learn.microsoft.com/azure/devops/pipelines/process (tasks, conditions,
expressions), tekton.dev/docs/pipelines/pipelines.

## Review Log

- 2026-09-26: draft written from the design session; decisions §2.1-§2.7 by
  the user.
- 2026-09-26: §2.4 changed at the user's request ("I do not like that the job
  is marked as failed", on §3.1): a failure is tolerated when it is caught by
  `continue_on_failure` anywhere downstream on every path, not only by the
  failing step's own flag. Settlement, hooks, restart preview, CLI exit code
  and UI banner follow.
- 2026-09-26, Codex round 1 (thread 01a0dcb7), all findings verified against
  HEAD 84ed688 and addressed in revision 2:
  - High, cancellation became failure: settlement now judges only `failed`
    rows; skipped rows never decide job status (§2.4). A cancelled child keeps
    the parent `cancelled`.
  - P2 timing: P1 keeps 0.16's all-dependencies-skipped unreachable skips;
    P2 takes the rest (§5); counterexample tests in §12.2.
  - `caught` proof: restated as a structural policy over the current flow;
    missing-flow rows and instance normalization specified (§2.4, §5).
  - Restart preview / unknown reasons / hooks with skip-only failures:
    resolved by the same "failed rows only" rule; hooks judge instances by
    their placeholder (§5).
  - CLI: outcome-aware template context, exit code = job outcome, summary
    count quirk left as is and tracked (§6).
  - §12.3 contradiction fixed; full-settlement side-effect tests added
    (§12.3); `orchestrator_test.rs:1501` kept unchanged with a new variant.
- 2026-09-26, Codex round 2 (same thread): 7 of 9 round-1 findings resolved,
  2 partially. Revision 3:
  - §5 timing claim narrowed: preserved where 0.16 did not read the
    dependent's own flag; the two flag-sensitive cases Codex found are
    documented as accepted and pinned in §12.2.
  - Retry/hook test split into retry-planned (no hook) vs exhausted
    (`on_error`) vs retry-creation-failure fallback; mixed cancelled + failed
    settlement test added.
  - CLI exit code: 0 only for `completed`; Ctrl-C stays an error path.
  - CLI test contrasting an own-flagged loop with a loop caught downstream.
- 2026-09-26, Codex round 3 (same thread): **sign-off, ready for an
  implementation plan.** One low wording finding — the §5 guarantee still
  overlooked the unknown-reason classification change. Revision 4 rewords §5
  as Codex suggested and pins the unknown-reason observer case in §12.2.
