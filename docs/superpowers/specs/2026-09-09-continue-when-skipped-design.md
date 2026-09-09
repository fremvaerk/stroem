# `continue_when_skipped` and Skip Reasons — Design

Status: revision 2, for user review
Ships in: 0.16.2 (patch; carries one documented behaviour change, §2.4)

## 1. Problem

A flow step whose dependencies are **all** `skipped` is itself skipped by the
cascade (`cascade.rs` R1, mirrored for `for_each` placeholders in R4). This is
what makes a `when: false` at the head of a branch skip the whole branch. It
also means a step that depends only on a conditional step can never run when
the condition is false, even if the author wants it to (a "report" step after
an optional check, a merge after an if/else where both arms may be off).

Today's escape hatches are structural or accidental:

- Add an unconditional dependency so that not all deps are skipped. The extra
  dependency is invisible as intent: nobody can tell it is load-bearing for the
  cascade. The recalc pipeline in the `jobs` workspace relies on this in at
  least two places (`demography-*` and `data-monitoring` survive `skip_agg_4`
  only because `run_params` is also listed).
- `continue_on_failure: true` also bypasses the all-deps-skipped rule
  (`all_deps_skipped(..) && !fs.continue_on_failure`). Undocumented, and it
  tolerates failure as a side effect.

A dedicated flag is the obvious fix, but a flag alone lies in its main use
case. A `skipped` row does not record **why** it was skipped. "Branch not
taken" (`when` false) and "upstream failed" (R3 skip-unreachable) look the
same. A merge step declared "continue when skipped" would therefore also run
after an upstream **failure** skipped its branch, which is exactly the
behaviour `continue_on_failure` exists to opt into explicitly.

## 2. Decisions

### 2.1 New flow-step flag `continue_when_skipped: bool` (default `false`)

When `true`, the step is not cascade-skipped when all of its dependencies
are skipped for a benign reason. Skipped dependencies already count as
satisfied, so the step is promoted and its own `when` is evaluated as usual;
skipped dependencies render as `{ "output": null }` in the template context,
unchanged.

### 2.2 Skip reasons are recorded

Every write of `status = 'skipped'` also writes `job_step.skip_reason`, one
of:

| Reason | Written when |
|---|---|
| `condition` | the step's own `when` rendered falsy |
| `empty` | the step's `for_each` produced zero items |
| `cascade` | all dependencies are skipped and none of them is `unreachable` |
| `unreachable` | a dependency `failed` or `cancelled` without `continue_on_failure` (R3, R4, R5); **or** all dependencies are skipped and at least one of them is `unreachable` |

The last clause is the propagation rule: a failure-induced skip stays
"unreachable" all the way down a chain of cascade skips. A skip caused by a
choice (`condition`, `empty`) becomes `cascade` downstream.

`NULL` (rows written before the migration, or by an older replica in a mixed
fleet) is treated as `unreachable` by the cascade. That reproduces the
pre-change behaviour for such rows: a `continue_when_skipped` step behind
them stays skipped.

### 2.3 The all-deps-skipped rule, restated

For a pending step `S` with non-empty `depends_on`, when every dependency is
`skipped`:

```
tainted  = any dependency has skip_reason ∈ {unreachable, NULL}
bypass   = S.continue_when_skipped && (!tainted || S.continue_on_failure)

if !bypass:
    Skip S with reason = unreachable if tainted else cascade
else:
    fall through to the normal path (deps satisfied → evaluate `when` → promote / skip:condition)
```

Read as: `continue_when_skipped` covers skips by choice; `continue_on_failure`
covers failure; a step that must run no matter what sets both. Mixed
dependencies (at least one `completed`) are promoted exactly as today,
whatever the reasons on the skipped ones. That rule is not touched.

### 2.4 `continue_on_failure` becomes failure-only

`continue_on_failure` alone no longer bypasses the all-deps-skipped rule. It
still (unchanged) lets a step run when a **direct** dependency failed or was
cancelled, and still marks the step's own failure as tolerated for job
settlement, loop rollup and restart.

This is a behaviour change for any existing step that has `continue_on_failure`
and whose dependencies can all be skipped. Audit of the user's workspaces
found no step affected in normal runs (the six `continue_on_failure` steps in
the recalc pipeline all have at least one dependency that always runs). The
release notes call it out with the fix: add `continue_when_skipped: true`.

### 2.5 The CLI local runner mirrors the server

`stroem run` implements its own all-deps-skipped check. It gains the same
flag. It has no `unreachable` reason because it aborts the run on the first
untolerated failure instead of skipping dependents; its reasons are
`condition`, `empty`, `cascade`.

## 3. Non-Goals

- Per-dependency status conditions (`depends_on: [{step: a, on: [..]}]`).
  The flag can be described as sugar for that later; not now.
- Exposing `skip_reason` in the Tera template context. Possible follow-up
  (`when: "{{ merge.skip_reason != 'unreachable' }}"`); not needed for the
  feature. Listed in TODO.md.
- Skip reasons in hook payloads. `hook.failed_steps` is about failures; there
  is no `skipped_steps` list today and this change does not add one.
- A task- or action-level default for the flag.
- A `CHECK` constraint on `skip_reason`. The Rust enum is the authority; the
  column is free text so a future reason needs no migration.

## 4. Semantics in the cascade

### 4.1 `Change::Skip` carries the reason

```rust
pub enum SkipReason { Condition, Empty, Cascade, Unreachable }
Change::Skip { step: String, reason: SkipReason }
```

`Snapshot::apply` records the reason on the in-memory row, so a later pass
of the same `run` sees it (a chain `a → b → c` all cascading in one run must
propagate `unreachable` from `b` to `c` without a DB round trip).

**Pass-boundary invariant (unchanged, now explicit).** A reason is visible
exactly where the status it accompanies is visible: a `Skip` decided in P1
is seen by P3 of the same pass and by P1 of the next pass, never by P1 of
the same pass. The `tainted` predicate reads the snapshot the same way
`all_deps_skipped` does and must not be given a private, earlier view.
Test: a chain of three cascade skips takes three passes and the reasons
match the statuses at every pass.

### 4.2 Sites, with their reason

| Site | Today | After |
|---|---|---|
| R1 `phase_promote`, all deps skipped | `&& !cof` → Skip | §2.3 formula, reason `cascade` / `unreachable` |
| R2 `phase_promote`, `when` falsy | Skip | Skip `condition` |
| R3 `phase_skip_unreachable` | Skip | Skip `unreachable` (guard unchanged: `!cof && any_dep_failed_or_cancelled`) |
| R4 `phase_placeholders`, dep failed/cancelled | Skip | Skip `unreachable` |
| R4 `phase_placeholders`, all deps skipped | `&& !cof` → Skip | §2.3 formula |
| R4 `phase_placeholders`, `when` falsy | Skip | Skip `condition` |
| R4 `phase_placeholders`, zero items | Skip | Skip `empty` |
| R5 `phase_rollup`, pending instances after a failed/cancelled instance | Skip | Skip `unreachable` |

`deps_satisfied`, `any_dep_failed_or_cancelled`, R0, R6, R7 and the phase
order are unchanged. The `tainted` predicate is a new sibling of
`all_deps_skipped` reading `snap.skip_reason(d)`.

### 4.3 `apply`

Consecutive `Skip`s are still batched into one statement, now per reason:
a run of `Skip`s is split into buckets by reason, one
`skip_steps_tx(job_id, names, reason)` per bucket, each with its own
row-count guard. Order within the plan is preserved.

## 5. Data Model

Migration `046_job_step_skip_reason.sql`:

```sql
ALTER TABLE job_step ADD COLUMN skip_reason TEXT;
```

Nullable, no default, no constraint. Written by:

- `JobStepRepo::skip_steps_tx` — gains a `reason: &str` parameter.
- `JobStepRepo::mark_skipped` — only used by a stroem-db guard test; gains
  the parameter rather than being deleted.
- `JobStepRepo::seed_steps_tx` — `Seed` gains `skip_reason: Option<String>`;
  `restart::compute_restart_set` copies it from the source row so a carried
  skipped step keeps its reason in the restart job.

Read by `JobStepRow.skip_reason` (all `SELECT` lists that enumerate columns
follow the `carried_over` precedent from migration 045).

## 6. Model and validation (`stroem-common`)

- `FlowStep.continue_when_skipped: bool`, `#[serde(default)]`, added to the
  inline-step `step_field_keys` list and to the manual parser next to
  `continue_on_failure` in `models/workflow.rs`. Every `FlowStep { .. }`
  literal in the workspace (test helpers in `dag.rs`, `library.rs`, the CLI,
  server tests, `restart.rs`) gains the field; the compiler enumerates them.
- `validation.rs`: a **warning** (same tier as `sequential` without
  `for_each`) when `continue_when_skipped` is set on a step with no
  `depends_on`: "`continue_when_skipped` has no effect without `depends_on`".
- `stroem inspect` / `stroem tasks` print the flag wherever they print
  `continue_on_failure`.

## 7. Server surfaces

- `web/api/jobs.rs` step DTO: `"skip_reason": step.skip_reason`.
- `mcp/tools.rs` `get_job_status` step DTO: `"skip_reason"` added (it already
  exposes `status` and `error_message`).
- Settlement, propagation, hooks, retry, recovery: no change. None of them
  reads skipped rows' reasons.

## 8. UI

- `types.ts`: `JobStep.skip_reason: "condition" | "empty" | "cascade" | "unreachable" | null`;
  `FlowStep.continue_when_skipped?: boolean`.
- `step-timeline.tsx` skipped badge: today it shows "condition" when
  `when_condition` is set. After: keyed on `skip_reason` — `condition`,
  `empty loop`, `upstream skipped`, `upstream failed`. `NULL` reason keeps the
  current fallback (`when_condition` set → "condition", else no badge).
  Same for the placeholder row.
- `step-detail.tsx`: one line under the status for skipped steps, e.g.
  "Skipped: an upstream step failed".
- Task Detail / DAG: a `continue_when_skipped` step gets the same kind of
  marker the DAG uses for `when` and `continue_on_failure`, if any; no new
  affordance otherwise.

## 9. CLI local runner (`stroem-cli/src/local/run.rs`)

- The all-deps-skipped check before a step runs and the `cascade_skip` helper
  both skip `continue_when_skipped` steps; the helper needs no reason
  tracking because the CLI never produces `unreachable`.
- Log lines already state the reason (`condition false`, `all dependencies
  skipped`, `empty for_each`); unchanged.

## 10. Documentation

- `docs/src/content/docs/guides/conditionals.md`: new section "Running after
  a skipped branch" with the report-after-optional-check and merge-after-
  if/else examples; the root-step note (which today says "remove the
  dependency or add a non-conditional dep") points at the flag; a "Skip
  reasons" table (§2.2) and the propagation rule; the `continue_on_failure`
  split with the "cleanup that must always run" example using both flags.
- `guides/workflow-basics.md` §"Error handling": the paragraph claiming
  skipped dependencies never need `continue_on_failure` is rewritten around
  the two flags.
- `reference/workflow-yaml.md`: step-fields table row for
  `continue_when_skipped`; the "Dependencies" paragraph gains the skip
  cascade sentence; `continue_on_failure` row loses any implication about
  skips.
- `reference/api.md`: `skip_reason` on the job-detail step object.
- `CLAUDE.md` § Conditional Flow Steps: the flag, the reasons, the `tainted`
  rule, the cof split. § Step Cascade: `Change::Skip` carries a reason;
  `apply` batches per reason.
- `docs/internal/TODO.md`: template-context `skip_reason` follow-up; note
  that the CLI runner still aborts on failure rather than skipping dependents
  (pre-existing divergence).
- Release notes for 0.16.2: the `continue_on_failure` change (§2.4) with the
  one-line fix.

## 11. Tests

### 11.1 Cascade unit tests (`cascade.rs`, no DB)

- `cws` with all deps skipped `condition` → `promote`.
- `cws` with all deps skipped and a falsy own `when` → `skip:condition`.
- `cof` alone with all deps skipped → `skip:cascade` (regression for §2.4;
  replaces the current `all_deps_skipped_cascade_skips_even_with_truthy_when`
  expectation on `cof`, which stays for the no-flag case).
- `cws` with one dep skipped `unreachable` → `skip:unreachable`.
- `cws` + `cof` with a dep skipped `unreachable` → `promote`.
- Chain `a fails → b (no cof) → c (cws)` in one `run`: `b` gets
  `unreachable`, `c` is skipped `unreachable` (propagation within one run).
- Chain `a when-false → b → c (cws)`: `b` `cascade`, `c` promoted.
- `NULL` reason on a dep behaves as `unreachable`.
- Placeholder with `cws` and all deps skipped `condition` → `expand`;
  with an `unreachable` dep → `skip:unreachable`.
- Reasons on R2/R3/R4/R5 sites: one assertion each.
- `names()` test helper keeps its `skip:<step>` format; a second helper
  `skips_with_reason()` lists `(step, reason)`.

### 11.2 Apply / DB (container)

- `cascade_apply_test.rs`: a plan with `[Skip cascade, Skip cascade, Skip
  unreachable, Skip condition]` writes the four reasons and passes the
  row-count guards; a stale row in one bucket is a `GuardMiss`.
- `stroem-db` integration: `seed_steps_tx` writes `skip_reason`;
  `mark_skipped` guard test updated for the parameter.
- `restart_integration_test.rs`: a carried skipped step keeps its reason.
- Writer contract: after a cascade that produces skips of every kind
  (condition, empty, cascade, unreachable) plus a restart that carries one,
  `SELECT count(*) FROM job_step WHERE status = 'skipped' AND skip_reason IS NULL`
  is zero.

### 11.3 Model / validation / CLI

- `workflow.rs`: flag parses on a normal step and an inline step; defaults
  false; round-trips.
- `validation.rs`: warning for `continue_when_skipped` without `depends_on`;
  no warning with `depends_on`.
- `run.rs`: local run where a `cws` step behind a false condition executes,
  and where a non-`cws` sibling is skipped.

### 11.4 Server integration (container)

- `orchestrator_test.rs` (or its settlement successor): through the real
  completion path, `a` completes with `when` false on `b`, `c` (`cws`,
  depends on `b`) is claimed and completed, job settles `completed`; then
  the same shape with `a` failing: `b` and `c` skipped `unreachable`, job
  `failed`.

### 11.5 UI

- `step-timeline.test.tsx`: badge text per reason and the `NULL` fallback.

### 11.6 E2E

`tests/e2e.sh` has no conditional scenario. Add one task to
`tests/e2e-workspace`: `optional-check` (`when` on an input, default off) →
`report` (`continue_when_skipped: true`). Trigger with the default input,
assert `optional-check` is `skipped` with `skip_reason = condition` and
`report` is `completed`.

## 12. Rollout

- Migration is additive and nullable; it can run before or after the binary
  rollout.
- Mixed fleet: an old replica writes skips without a reason; a new replica
  treats those as `unreachable`, which is the old behaviour (skip). No job
  can run a step it would not have run before the change until every
  replica is new.
- Patch release 0.16.2. Release notes: the `continue_on_failure` split
  (§2.4) and how to get the old behaviour back (`continue_when_skipped: true`
  on the affected step).

## 13. Resolved Questions

- **Should `continue_on_failure` keep bypassing the skip cascade?** No; it is
  failure-only (§2.4). Decided 2026-09-09.
- **Why record a reason instead of shipping the flag alone?** Without it the
  flag runs a merge after an upstream failure (§1). Decided 2026-09-09.
- **Should `unreachable` propagate through a chain of cascade skips?** Yes,
  "any tainted dependency" (§2.2). "All" would let one benign branch launder
  a failure.
- **Does `unreachable` on a skipped dependency affect a step that also has a
  completed dependency?** No. Mixed dependencies promote as today; the reason
  only matters when every dependency is skipped (§2.3).
- **`empty` as its own reason or folded into `condition`?** Its own, for the
  UI label; the cascade treats it exactly like `condition`.
- **Version.** Patch (0.16.2), per the user, with the behaviour change in
  the release notes.

## 14. Review Log

- Revision 1 → 2 (Codex pass 1, 2026-09-09): added the pass-boundary
  invariant to §4.1 and the writer-contract test to §11.2. The remaining
  findings restated the current code's lack of the feature or were already
  covered in §5, §9 and §11; three proposed sequential-loop and CLI-phase
  tests do not apply (instances have no `depends_on`; the CLI has no
  passes).
