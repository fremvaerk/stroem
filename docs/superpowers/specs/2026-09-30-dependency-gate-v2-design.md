# Dependency Gate v2 — per-edge optional dependencies, self-scoped `continue_on_failure` — Design

Status: draft, awaiting user review
Ships in: 0.18.0 (breaking, on top of the already-breaking 0.17.0)
Supersedes: `2026-09-26-dependency-gate-design.md` §2.1, §2.4, §14 (explicitly
reverses that spec's non-goal of a dependent-side flag)

## 1. Problem

0.17.0 (released 2026-09-27) made `continue_on_failure` and
`continue_when_skipped` readable only from the dependency, never the
dependent, and made a failure "caught" for job-status purposes if
`continue_on_failure` sits anywhere on every path below it
(`stroem_common::gate::caught_steps`). Two days later, working prod job
`a45096f8` (`jobs/recalc-pipeline`, ClickHouse disk-full on `build-sessions`)
against the actual desired behaviour of that pipeline showed the design still
conflates two things that need to vary independently:

1. **Does this step's own failure fail the job?** — a property of one step.
2. **Do this step's dependents still run when it fails or is blocked?** — a
   property `continue_on_failure` currently broadcasts identically to *every*
   dependent, but the pipeline needs to answer differently per dependent, and
   sometimes differently per *dependency* of the same dependent.

Concretely, `ml-impressions-beta` depends on both `ml-prediction-beta`
(should block it — a failed prediction must not run impressions on empty
data) and `dwell-time` (should never block it — "a failed dwell does not
block them", already documented intent). One step, two dependencies, two
different tolerances — no flag placement under 0.17.0 expresses this, because
0.17.0 only lets a *dependency* broadcast one blanket answer to all its
dependents, never lets a *dependent* answer differently per edge. The
0.17.0 spec called this out and explicitly declared it a non-goal (§14,
"a dependent-side 'run always' / trigger-rule field") on the assumption it
wasn't needed. It is.

The user supplied the target behaviour for `recalc-pipeline` as seven
concrete rules (§3 below reproduces and verifies them), and the two-flag
model was rebuilt from those rules rather than patched again.

### 1.1 Prior art (see Appendix A for the full table)

- **Tekton**'s `onError: continue` only changes whether the *PipelineRun* is
  marked failed; the TaskRun itself still reports Failed and its
  result-consumers are still blocked. `runAfter` is a separate field for
  pure ordering with no success requirement at all — a different verb, not a
  second list of the same shape.
- **GitHub Actions**' `continue-on-error` is likewise job-status-only;
  dependents inspect the real `needs.<job>.result` if they want to react.
- **GitLab CI** ships `needs: [{job: X, optional: true}]` — the per-edge
  annotation sits *inside* the same list, on the specific entry it modifies.
- **GitLab's `allow_failure: true`**, by contrast, bundles both effects (like
  Strøm's current `continue_on_failure`) — this is the one pattern the
  research turned up that nobody else uses, i.e. the thing being fixed here.
- **Argo Workflows**' `Skipped` (own condition false) vs. `Omitted` (blocked
  because a dependency was skipped) split matches Strøm's existing
  `SkipReason::Condition/Empty` vs. `Cascade/Unreachable` split — not
  changed by this spec, cited here only to confirm that split is
  precedented, not overengineering.
- **GitHub Actions**' `if:` silently drops the implicit `success()` check
  when a custom condition is written — a widely-documented footgun — which
  is why this design does not reuse Strøm's `when` (input-conditional) field
  for dependency-status gating; it stays a separate mechanism.

## 2. Decisions

### 2.1 `continue_on_failure` becomes self-scoped only

> `continue_on_failure: true` on step `X`: if `X` itself fails or is
> cancelled, that failure does not fail the job. It has no effect on `X`'s
> dependents — they gate on `X` exactly as if the flag were absent.

This drops the "dependents treat `X` as satisfied" half of the 0.17.0
definition entirely. `X`'s own outcome (Failed/Cancelled) is unchanged; only
what it does to job status and to the gate changes.

### 2.2 `continue_when_skipped` is unchanged

Kept exactly as shipped in 0.17.0: `continue_when_skipped: true` on `X`
means a dependent that requires `X` still runs when `X` is skipped by choice
(`condition`, `empty`, or a `cascade` skip of a choice-class skip). Every use
of it in `recalc-pipeline` (§3) stayed a clean one-to-one broadcast with no
per-edge conflict, so there is no motivation to touch it.

### 2.3 `depends_on` gains a per-entry `optional` marker

```yaml
depends_on: [step1, step2, {step: step3, optional: true}]
```

An entry is either a bare string (today's shape, unchanged meaning: this
dependency must `complete`, or be excused by *its own*
`continue_when_skipped`) or an object `{step: <name>, optional: <bool>}`.
`optional` defaults to `false`, so `{step: step3}` alone is identical to the
bare string `step3` — the object form exists purely to carry the marker,
not as an alternate spelling.

An `optional: true` entry means: wait for `step3` to reach *any* terminal
state (`completed`, `failed`, `cancelled`, or `skipped` for any reason), then
proceed regardless of which one it was. It is a strict superset of both
`continue_on_failure` and `continue_when_skipped`'s tolerance, evaluated
per-edge, declared by the dependent, not broadcast from the dependency.

This was chosen over three other shapes considered and rejected in
discussion:

- A second field naming the same dependencies again (`optional_depends_on:
  [...]`) — rejected: two lists that can name the same step invite a
  dependency ending up in the wrong one, or in neither.
- A separate ordering-only field with a different verb (`after: [...]`,
  Tekton's `runAfter` shape) — workable, but the user preferred keeping every
  dependency in the one list they already look at, with GitLab's inline
  per-entry precedent as the deciding factor.
- Reusing `when` to express dependency-status conditions — rejected per
  §1.1's GitHub Actions footgun; `when` stays input-conditional only.

### 2.4 Job status: a simple per-row check, no graph walk

0.17.0's `caught_steps()` (`stroem_common::gate`) computes, for every step,
whether a hypothetical failure there would be "caught" by
`continue_on_failure` on itself *or, recursively, on every one of its
dependents* — a structural walk over the whole flow graph, with cycle
guarding. Once `continue_on_failure` no longer propagates (§2.1), that
recursive fallback can never fire: nothing downstream can catch a failure it
didn't declare tolerance for itself. Job-status determination collapses to:

```
job fails ⟺ some job_step row has status Failed or Cancelled
             AND that row's flow step does not have continue_on_failure = true
```

(Skipped rows still never decide job status, unchanged from every prior
revision.) `caught_steps()`, `failure_caught()`, and their cycle-detection
machinery are deleted; the check becomes a direct lookup keyed by the failed
row's flow-step name (loop instances still normalize to their placeholder's
name, exactly as `flow_step_name` does today).

### 2.5 Verdict table

| Dependency outcome | Edge `optional: false` (default) | Edge `optional: true` |
|---|---|---|
| Pending (not yet terminal) | Wait | Wait |
| `completed` | Pass | Pass |
| `skipped` (`condition`/`empty`/`cascade`) | Pass if dependency has `continue_when_skipped`, else BlockSkip | Pass |
| `failed` / `cancelled` / `skipped` (`unreachable` or NULL/unknown reason) | **BlockFail, unconditionally** — the dependency's own `continue_on_failure` is no longer consulted here | Pass |

The one behavioural change to the required-edge column versus 0.17.0:
`continue_on_failure` drops out of the failure row entirely. Everything else
in that column — the choice-skip row, the pending row — is unchanged from
the 0.17.0 table.

## 3. Worked example: `jobs/.workflows/recalc/pipeline.yaml`

The seven target rules, and the config that satisfies each:

| # | Rule | Config |
|---|---|---|
| 1 | `build-sessions` fails → job fails, nothing depending on it runs | No `continue_on_failure` on `build-sessions`. Its (required, unannotated) dependents block. |
| 2, 3 | `ai_maintain` fails or is skipped → `ai_sources` still runs | `ai_sources`: `depends_on: [{step: ai_maintain, optional: true}]` — one mechanism for both outcomes. |
| 4 | `ml-prediction-master` fails → job fails | No `continue_on_failure` on it. |
| 5, 6 | `ml-prediction-beta`/`-stage` fail → job survives, but their impressions steps skip (not run on empty data) | `continue_on_failure: true` on the prediction steps (job-status only); the impressions steps keep them as required (unannotated) dependencies. |
| 7 | `merge-ml` requires `ml-impressions-master`, tolerates `-beta`/`-stage` in any state | `merge-ml`'s `depends_on` marks only the beta/stage entries `optional: true`. |

Full diff:

```yaml
  ai_maintain:
    depends_on: [build-sessions]         # unchanged — genuinely needs today's session data (confirmed
                                          # with the user; ai_maintain's action reads it directly, not
                                          # through a templated input field)
    continue_on_failure: true            # unchanged spelling, narrowed meaning: only protects job status now
-   continue_when_skipped: true          # redundant — ai_sources now marks the edge optional below

  ai_sources:
    action: ai_traffic_model.sources
-   depends_on: [ai_maintain]
+   depends_on: [{step: ai_maintain, optional: true}]
-   continue_on_failure: true            # did nothing for ai_sources itself under either model — remove

  ml-prediction-beta:
    depends_on: [build-sessions]
+   continue_on_failure: true            # NEW: a failed beta prediction doesn't fail the job
    ...
  ml-prediction-stage:
    depends_on: [build-sessions]
+   continue_on_failure: true            # NEW: same for stage
    ...

  ml-impressions-master:
-   depends_on: [ml-prediction-master, dwell-time]
+   depends_on: [ml-prediction-master, {step: dwell-time, optional: true}]
    # dwell-time must be marked optional HERE too, not just on beta/stage below:
    # dwell-time's own continue_on_failure used to broadcast to every dependent,
    # including this one. Under §2.1 it no longer does, so without this the
    # existing documented intent ("a failed dwell does not block impressions")
    # would regress for the master variant specifically.

  ml-impressions-beta:
-   depends_on: [ml-prediction-beta, dwell-time]
+   depends_on: [ml-prediction-beta, {step: dwell-time, optional: true}]
    continue_on_failure: true            # kept — now correctly self-scoped (protects the job if
                                          # impressions-beta itself crashes, e.g. the empty-array
                                          # ValueError seen in job a45096f8)
    ...
  ml-impressions-stage:   # mirror of beta
    ...

  merge-ml:
-   depends_on: [ml-impressions-master, ml-impressions-beta, ml-impressions-stage]
+   depends_on: [ml-impressions-master, {step: ml-impressions-beta, optional: true}, {step: ml-impressions-stage, optional: true}]
```

`build-sessions`, `ml-prediction-master`, `demography-*`, `monitor-*`,
`agg-sessions`, `realtime-factor`, `data-monitoring`, `upload-jcdecaux`: all
unchanged, all already correct under this model (no flags, so a failure
blocks everything below and fails the job — exactly rule 1's shape).

## 4. Data model (`stroem-common`)

`FlowStep.depends_on` changes from `Vec<String>` to `Vec<DependsOnEntry>`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DependsOnEntry {
    Name(String),
    Detailed {
        step: String,
        #[serde(default)]
        optional: bool,
    },
}

impl DependsOnEntry {
    pub fn name(&self) -> &str { .. }       // both variants
    pub fn is_optional(&self) -> bool { .. } // false for Name
}
```

Every current call site that treats `depends_on` as `&[String]` (cycle
detection, validation, the render-context dependency graph, the docs
generator, the UI's DAG payload) moves to `.iter().map(DependsOnEntry::name)`
or gets a small adapter. Cycle detection is unaffected by `optional` — an
optional edge is still an edge for graph-shape purposes; only the gate's
runtime verdict differs.

## 5. Shared gate (`stroem-common::gate`)

- `verdict()` gains an `edge_optional: bool` parameter (or an `Verdict`
  computation branches on it before the existing match). The optional branch:
  `Pending → Pending`, everything else `→ Pass`. `continue_on_failure` is
  removed from the failure-class arm of the non-optional branch (§2.5).
- `gate()`'s signature changes from `depends_on: &[String]` to
  `depends_on: &[DependsOnEntry]`, passing each entry's `is_optional()` into
  `verdict()` alongside the looked-up `DepOutcome` and the dependency's
  `FlowStep` (still needed for `continue_when_skipped`, no longer needed for
  `continue_on_failure`).
- `caught_steps()` and `failure_caught()` are deleted. Callers (`settle.rs`'s
  job-status decision, the hooks `tolerated` field) switch to the direct
  per-row check in §2.4: `flow.get(flow_step_name(step_name,
  loop_source)).is_some_and(|fs| fs.continue_on_failure)`.

## 6. Server cascade (`crates/stroem-server/src/cascade.rs`)

Phase structure (P0–P3) is unchanged — this spec touches only what `gate_for`
computes, not when it's called. `phase_rollup`'s existing use of a `for_each`
placeholder's own `continue_on_failure` (deciding whether a failed loop
instance fails the *placeholder*) is unaffected: that was always a
self-scoped question ("does this loop's own outcome count as a failure"),
never a propagation one, so §2.1 doesn't change it. It does mean a
placeholder that currently also relies on its `continue_on_failure` to
*unblock its dependents* needs the same audit as any other step — see §9.

## 7. Validation (`stroem-common::validation`)

The existing "step will be skipped whenever its dependency is skipped, add
`continue_when_skipped`" warning must skip entries already marked
`optional: true` (an optional edge needs no such flag on the dependency —
it's unconditionally satisfied). No new validation is required for the
`optional` marker itself; a bad `step:` name inside a `Detailed` entry is
caught by the same "unknown dependency" check as a bad bare-string entry.

## 8. Hooks

`hook.failed_steps[].tolerated` is redefined from "caught anywhere
downstream" to "this step's own `continue_on_failure` is set" — a direct
flag lookup instead of a graph walk. This is a narrower, more literal
reading of "tolerated" and needs its own line in the upgrade guide (§11):
a failure that used to show `tolerated: true` only because something further
downstream had the flag will now show `tolerated: false` on the failing step
itself (the job may still be `completed` if that downstream step's
`optional` edge let everything past it — but the *hook payload's* per-step
flag now reflects that step alone).

## 9. Migration guidance (for the upgrade doc, §11)

Any workflow relying on 0.17.0's `continue_on_failure`-on-the-dependency
propagation (the exact case this spec removes) needs each such dependent
edge re-expressed as `{step: <name>, optional: true}` on the dependent's own
`depends_on`. This is mechanical but not automatable in general — the
existing 0.17.0 upgrade guide's checklist item ("search for
`continue_on_failure` on a step with a `depends_on`...") gets a second pass:
search for `continue_on_failure` on any step that has *dependents*, and for
each one, either accept it becomes self-scoped-only (if the workflow never
actually needed downstream propagation, e.g. it degenerately worked before
because that node had no dependents), or add `optional: true` on each
dependent's edge to it.

## 10. Documentation

- CLAUDE.md § Conditional Flow Steps: rewrite the dependency-gate paragraph
  for the third time; this revision should be written to last (state the
  final model plainly, keep the superseded-revisions history in the spec
  files, not in CLAUDE.md).
- `docs/src/content/docs/guides/conditionals.md`: full rewrite of the
  flag-reference section; add the `depends_on` object-entry syntax next to
  the existing plain-list syntax.
- `docs/src/content/docs/reference/workflow-yaml.md`: `depends_on` schema
  entry gains the `{step, optional}` shape.
- New upgrade page (`docs/src/content/docs/operations/upgrade-0-18-*.md`),
  parallel to the 0.17 one, covering §9's migration guidance and the hooks
  change (§8).

## 11. Tests

- `gate.rs`: full rewrite of `verdict_table` and `gate_reads_the_dependency_flags_only`
  for the new signature; delete `caught_steps_*` tests; add
  tests for the optional-edge Pass-on-anything behaviour (including Pending
  still waiting).
- `cascade.rs`: update fixtures using the old `fs()` test helper for the new
  `depends_on` shape; add a case mirroring `ml-impressions-beta` (one
  required + one optional dependency on the same step).
- `settlement`/hooks tests: replace assertions built on `caught_steps`
  recursion with the direct per-row `tolerated` check; the existing
  "diamond needs every path" and "long chain" tests for `caught_steps` are
  deleted outright (the mechanism they tested no longer exists).
- `stroem-cli` local runner: same gate, same test treatment as `cascade.rs`.
- `tests/e2e-workspace/`: extend the conditional fixtures with an
  optional-dependency scenario.
- A migration test is not applicable — no DB schema changes, this is a YAML
  schema and in-memory gate change only.

## 12. Rollout

- Version: 0.18.0 (another breaking change, on top of 0.17.0's two days
  ago). Release notes lead with the behaviour change and link both the
  0.17.0 and 0.18.0 upgrade guides.
- No DB migration. `job_step.skip_reason` values are unchanged.
- Operational note, not part of this spec's scope: as of 2026-09-29,
  production had likely not yet been upgraded past pre-0.17 server code (the
  job that motivated this spec showed pre-0.17 gating behaviour). If that
  holds at ship time, production's *first* real exposure to the dependency
  gate rewrite will be this design, not bare 0.17.0 — worth confirming before
  release so the upgrade communication targets the right starting point.
- `jobs/.workflows/recalc/pipeline.yaml` is updated as part of this change
  (§3); `ai-traffic-estimator/.workflows/*.yaml` needs the same audit per §9,
  out of scope for this spec's implementation but tracked in TODO.md.

## 13. Non-goals

- A full boolean-expression dependency language (Argo's `depends: "A.Succeeded
  && (B.Succeeded || B.Skipped)"`) — `optional` is a binary per-edge switch,
  not an expression evaluator. Revisit only if a concrete case needs
  combining multiple dependencies' outcomes in one condition.
- Any change to `continue_when_skipped`'s broadcast (non-per-edge) model —
  no case in `recalc-pipeline` needed it, see §2.2.
- UI: visually distinguishing optional edges in the DAG view (e.g. a dashed
  line). Left as a follow-up; tracked in TODO.md.
- Renaming `continue_on_failure` despite its narrowed meaning — keeping the
  name was a deliberate choice to limit churn to behaviour, not vocabulary,
  given 0.17.0's name is two days old.

## Appendix A. Other systems (update to the 2026-09-26 spec's table)

| System | Per-edge optional? | Job-status vs. dependents-run | Skip vs. failure |
|---|---|---|---|
| Tekton | `runAfter` (separate field, ordering only) | Separate: `onError: continue` is status-only | Failed vs. Skipped (own `when` false) vs. `runAfter`-only (never blocked) |
| Argo Workflows | Yes, via boolean `depends:` expression | Not separated | `Skipped` (own condition) vs. `Omitted` (dependency chain skipped) |
| GitHub Actions | Via explicit `needs.<job>.result` checks in `if:` | Separate: `continue-on-error` is status-only | Implicit `success()` silently dropped by a custom `if:` — documented footgun |
| GitLab CI | Yes — `needs: [{job: X, optional: true}]`, inline in the same list | `allow_failure: true` bundles both (the pattern being fixed here) | `optional:true` is about conditional job existence, not failure-tolerance per se |
| Airflow | No — `trigger_rule` is per-task, blanket across all upstreams | Not separated | Distinct Skipped/Failed/`upstream_failed` states, rules read them differently |
| AWS Step Functions | N/A — explicit `Catch`/`Retry` per state, no implicit gating | Explicit re-routing to a named error state | No skip concept; every transition is explicit |
| Dagster | Yes — producer `is_required=False` + consumer fan-in | N/A (no separate "job" status concept) | Unresolved input → skip, cascades unless fan-in used |

GitLab's `needs: [{job: X, optional: true}]` is the closest production
precedent to §2.3's chosen syntax. Tekton and GitHub Actions are the closest
precedent for §2.1's self-scoped `continue_on_failure`.

Sources: see the fork research transcript (2026-09-30) — astronomer.io/docs
(Airflow trigger rules), argo-workflows.readthedocs.io (enhanced-depends-logic),
github.com/tektoncd (pipelines.md, TEP-0050), docs.github.com/actions
(using-conditions-to-control-job-execution), opscanopy.com (`if:` footgun),
docs.gitlab.com/ci/yaml (needs), docs.aws.amazon.com/step-functions
(concepts-error-handling), docs.dagster.io (op graphs).

## Review Log

- 2026-09-30: draft written from the design session that started with prod
  job `a45096f8` (ClickHouse disk-full on `build-sessions`) surfacing that
  `ml-impressions-beta`/`-stage` ran on empty prediction data. Decisions
  §2.1-§2.3 made with the user across several rounds: rejected
  `optional_depends_on` as a second list (confusing), rejected reusing
  `when` (GitHub Actions footgun), rejected a separate `after`/`runAfter`-
  style field (user preference for one list), settled on GitLab's inline
  `{step, optional}` shape. `ai_maintain`'s dependency on `build-sessions`
  confirmed required (not optional) — the action reads session data
  `build-sessions` produces directly, not through a templated input.
