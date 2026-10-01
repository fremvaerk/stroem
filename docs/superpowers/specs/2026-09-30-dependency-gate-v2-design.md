# Dependency Gate v2 — per-edge optional dependencies, self-scoped `continue_on_failure` — Design

Status: **SUPERSEDED 2026-10-01, before sign-off** — see
`2026-10-01-dependency-conditions-design.md`. Kept for its review history
(Codex rounds 1-2 found and fixed real bugs in the worked example and
migration guidance that remain relevant background); do not implement
against this file.
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

> `continue_on_failure: true` on step `X`: if `X` itself fails, that failure
> does not fail the job. It has no effect on `X`'s dependents — they gate on
> `X` exactly as if the flag were absent.

This drops the "dependents treat `X` as satisfied" half of the 0.17.0
definition entirely. `X`'s own outcome (Failed/Cancelled) is unchanged; only
what it does to job status and to the gate changes.

**Cancellation is not covered by this flag for job-status purposes**, and
keeps its existing, separate precedence, unchanged from shipped `settle.rs`:
an explicit, external job cancellation (a user or system cancel signal)
always produces job status `Cancelled`, regardless of any step's flags.
Short of that, a `Cancelled` *row* (e.g. a single step cancelled by the
recovery sweep without the whole job being cancelled) is folded into
job-status determination *after* the uncaught-failure check — it is never
itself "caught" by `continue_on_failure` for that purpose, matching today's
behaviour. §2.4 restates the full order. This "not covered" claim is scoped
to job status specifically — §6 covers the one place `continue_on_failure`
*does* still interact with a cancelled outcome (a loop's own sequential
advancement past a cancelled instance), which this flag narrowing does not
touch.

**Loops are a pre-existing, narrower exception, unchanged by this spec.** A
`for_each` placeholder's own `continue_on_failure` already does more than
protect job status: it also decides whether the placeholder itself rolls up
as `completed` or `failed` when one of its instances failed, and (for
`sequential: true` loops) whether later instances still run after an earlier
one failed (`cascade.rs::phase_rollup`, R5/R6). A tolerated instance failure
rolls the placeholder up as `completed` — a genuinely Completed row, not a
special exempted status — so its own downstream dependents gate on it
exactly like any other completed step, through ordinary required edges, with
nothing about those edges becoming optional. That is a real execution
effect, but it is *internal to the loop's own rollup*, decided once before
the placeholder's row ever reaches its dependents — it was never part of the
0.17.0 "unblock my dependents on *my own* failure" propagation this spec
removes, and this spec does not change it. §6 restates this explicitly so it
isn't read as an oversight.

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

An entry is either a bare string or an object `{step: <name>, optional:
<bool>}`. Only the bare-string *syntax* is unchanged from today — a bare
string still means this dependency must `complete`, or be excused by *its
own* `continue_when_skipped`. Its *behaviour* changed in one respect
alongside everything else in this spec: a failed bare-string dependency can
no longer be excused by its own `continue_on_failure` (§2.5) the way it
could under 0.17.0.

`optional` defaults to `false`, so `{step: step3}` alone is identical to the
bare string `step3` — the object form exists purely to carry the marker,
not as an alternate spelling.

An `optional: true` entry means: wait for `step3` to reach *any* terminal
state (`completed`, `failed`, `cancelled`, or `skipped` for any reason), then
proceed regardless of which one it was. It is a strict superset of both
`continue_on_failure` and `continue_when_skipped`'s tolerance, evaluated
per-edge, declared by the dependent, not broadcast from the dependency.

`optional: true` only weakens the *outcome* requirement, never the
*existence* one: an optional entry must still name a step that actually
exists in the flow (validated exactly like a required entry, §7), and the
gate still waits for it to reach a terminal state before proceeding.
`optional` never means "this dependency doesn't have to run" or "may be
absent from the flow" — only "I don't care which terminal state it lands
in."

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
didn't declare tolerance for itself.

Job-status determination keeps the exact precedence order already in
shipped `settle.rs` — this spec changes only step 2's definition of
"uncaught", nothing else in the order:

1. The job was explicitly cancelled (external signal) → `Cancelled`,
   unconditional, no flag can override it (unchanged from today).
2. Else, if any row has status `Failed` and its flow step does not have
   `continue_on_failure = true` → `Failed`. (This is the one thing changing:
   "uncaught" used to mean "not caught by `caught_steps()`'s recursive walk";
   it now means "the failing row's own flow step lacks the flag.")
3. Else, if any row has status `Cancelled` (e.g. a step cancelled by the
   recovery sweep without the job itself being cancelled) → `Cancelled`. A
   cancelled row is never excused by `continue_on_failure` — cancellation
   sits outside the catch mechanism entirely, matching shipped behaviour.
4. Else → `Completed`.

(Skipped rows never decide job status, at any step of this order — unchanged
from every prior revision.) `caught_steps()`, `failure_caught()`, and their
cycle-detection machinery are deleted; step 2's check becomes a direct
lookup keyed by the failed row's flow-step name (loop instances still
normalize to their placeholder's name, exactly as `flow_step_name` does
today).

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

**Combining multiple entries.** A step's overall gate result takes the worst
verdict across all its `depends_on` entries, in this dominance order:
`BlockFail` > `Pending` > `BlockSkip` > `Open`. So a step with one required
dependency that failed and one optional dependency still running is
immediately `unreachable` (`BlockFail` wins outright, it does not wait for
the optional one); a step with one required choice-skipped dependency and
one optional pending dependency waits (`Pending` beats `BlockSkip`). This is
the existing 0.17.0 combination rule, unchanged — §2.3 only adds a new way
for a single entry to resolve to `Pass`. §3's fix to `ai_sources` is a direct
application of this: its required `build-sessions` edge dominates its
optional `ai_maintain` edge.

**Duplicate entries for the same step are rejected by validation**, not
given an implicit precedence. A `depends_on` list naming the same step twice
(e.g. `[a, {step: a, optional: true}]`) is almost always a copy-paste
mistake, and picking a silent winner (required wins? optional wins? first
listed wins?) would hide it instead of surfacing it. This is a new rejection,
not just new syntax: today's `depends_on` is already a plain list of
strings, so `depends_on: [a, a]` is valid (if pointless) YAML today — the DAG
builder counts both edges without complaint. §9's migration guidance needs
an explicit "deduplicate `depends_on`" checklist item, since upgrading could
otherwise turn a previously-harmless duplicate into a hard validation error.

## 3. Worked example: `jobs/.workflows/recalc/pipeline.yaml`

The seven target rules, and the config that satisfies each:

| # | Rule | Config |
|---|---|---|
| 1 | `build-sessions` fails → job fails, nothing depending on it runs | No `continue_on_failure` on `build-sessions`. Its (required, unannotated) dependents block. |
| 2, 3 | `ai_maintain` fails or is skipped → `ai_sources` still runs — but a `build-sessions` failure must still stop it (rule 1) | `ai_sources`: `depends_on: [build-sessions, {step: ai_maintain, optional: true}]` — `build-sessions` stays required so rule 1 holds transitively through `ai_sources`; `ai_maintain` is optional so its own outcome (fail or skip) never blocks. |
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
+   depends_on: [build-sessions, {step: ai_maintain, optional: true}]
    # build-sessions is added here, required. Without it, ai_maintain's own
    # optional edge would let ai_sources run even when build-sessions itself
    # failed: ai_maintain would be blocked "unreachable", which an optional
    # edge tolerates the same as a genuine failure — violating rule 1. (This
    # fixes a bug Codex's review of this spec's first draft found: the
    # example as originally written let ai_sources bypass a build-sessions
    # failure it was never meant to.) Per §2.5's combined-verdict dominance,
    # the required build-sessions edge failing blocks ai_sources outright,
    # regardless of the optional ai_maintain edge.
    continue_on_failure: true            # kept, now self-scoped: protects the job if
                                          # ai_sources itself crashes — an independent
                                          # decision from the gating question above

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
        // NOTE: deny_unknown_fields is load-bearing, not decorative — see below.
    },
}

impl DependsOnEntry {
    pub fn name(&self) -> &str { .. }       // both variants
    pub fn is_optional(&self) -> bool { .. } // false for Name
}
```

The `Detailed` variant **must** carry `#[serde(deny_unknown_fields)]`.
Without it, `{step: a, optionl: true}` (a typo'd key) parses successfully as
`Detailed { step: "a", optional: false }` — the typo'd key is silently
dropped and `optional` silently defaults to `false`, exactly the kind of
silent data loss the "malformed entry must error" requirement below exists
to prevent. `#[serde(untagged)]` tries `Name` first (fails on a mapping
input, since it expects a bare scalar) and falls through to `Detailed`; with
`deny_unknown_fields` on `Detailed`, that fallthrough then correctly
produces a parse error instead of a silent typo. Deserialization of a
malformed `Detailed` entry must produce a hard parse error, not silently
fall back to an empty or partial `depends_on` list — this applies to both
inline flow steps and steps pulled in from a referenced library action;
neither path may use an `unwrap_or_default()`-style fallback (the existing
inline-step pattern at `workflow.rs:578` does exactly this today and must be
changed) that would drop dependencies without surfacing an error. Add tests
for both the inline and the referenced-library-action parse path.

Every current call site that treats `depends_on` as `&[String]` moves to
`.iter().map(DependsOnEntry::name)` or a small adapter — but for two of
these consumers, a name-only adapter is not enough, because they each make
their own independent "was this failure tolerated" decision, separately from
the gate:

- **Restart preview** (`restart.rs`) classifies a carried-over failed step
  as `carried_failed` or `carried_failed_tolerated` — this classification
  needs the same direct self-flag rule as §2.4/§8 (`continue_on_failure` on
  the failed step's own flow step, nothing recursive), not just a
  `depends_on`-shape update.
- **The CLI local runner** (`stroem-cli/src/local/run.rs`) computes its own
  final `RunOutcome` from the local flow's terminal rows — this also needs
  the direct self-flag rule applied explicitly, with its own outcome-level
  test, not just a shared-gate test.

The remaining consumers are name-only: cycle detection and the
transitive-skippable check (`validation.rs`), the render-context dependency
graph, the docs generator, the UI's DAG payload, the task/job detail API
responses (`web/api/tasks.rs` **and** `web/api/jobs.rs`, which separately
serializes step dependencies), the UI's TypeScript types
(`ui/src/lib/types.ts`, currently typed as a plain string array). Each of
these needs to either keep working on plain names (via the adapter) or,
where it's user-facing (the API responses, the UI, the restart preview),
surface the `optional` marker verbatim so a viewer can tell which edges are
required. Cycle detection is unaffected by `optional` — an optional edge is
still an edge for graph-shape purposes; only the gate's runtime verdict
differs.

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
  per-row check `flow.get(flow_step_name(step_name,
  loop_source)).is_some_and(|fs| fs.continue_on_failure)`, applied in the
  exact 4-step order §2.4 specifies — explicit cancellation, then uncaught
  failures, then any cancelled row, then completed — not a flat "does this
  row have the flag" scan.

## 6. Server cascade (`crates/stroem-server/src/cascade.rs`)

Phase structure (P0–P3) is unchanged — this spec touches only what `gate_for`
computes, not when it's called. `phase_rollup`'s existing use of a `for_each`
placeholder's own `continue_on_failure` — deciding whether a failed loop
instance fails the *placeholder* (R6) and whether a `sequential` loop keeps
promoting instances after a failure (R5) — is the loop exception §2.1
carves out explicitly: unchanged by this spec, and deliberately not
"job-status only" (it's intra-loop execution, not the cross-step propagation
being removed). A placeholder that currently *also* relies on its
`continue_on_failure` to unblock a step that depends on the placeholder as a
whole — a different question from the loop's own rollup — needs the same
audit as any other step, see §9.

## 7. Validation (`stroem-common::validation`)

The existing "step will be skipped whenever its dependency is skipped, add
`continue_when_skipped`" warning needs two changes, not one:

1. Suppress the warning on an entry already marked `optional: true` — an
   optional edge needs no `continue_when_skipped` on the dependency, it's
   unconditionally satisfied regardless of skip reason.
2. `is_transitively_skippable`'s propagation must *stop* at an optional
   edge, not just skip warning about it. Today it walks forward through
   `depends_on` to find steps that could be skipped as a side effect of an
   upstream `when`. Without this, `A(when) → B(optional: A) → M(required:
   B, C)` still gets flagged as "M will be skipped whenever A is skipped"
   even though `B`'s optional edge already absorbs that — exactly the false
   positive the warning exists to avoid.

A bad `step:` name inside a `Detailed` entry is caught by the same "unknown
dependency" check as a bad bare-string entry; a duplicate entry for the same
step name is rejected per §2.5.

## 8. Hooks

`hook.failed_steps[].tolerated` is redefined from "caught anywhere
downstream" to "this step's own `continue_on_failure` is set" — a direct
flag lookup instead of a graph walk. Under §2.4's precedence this is now
exactly consistent with job status, not just narrower: the job can only end
`completed` if *every* `Failed` row has `tolerated: true` on itself. (An
earlier draft of this spec described a case where a failure showed
`tolerated: false` while the job still completed via a downstream `optional`
edge — that case is impossible under §2.4 as written: an optional edge
changes whether execution continues past a block, never whether an
already-failed, unflagged row is excused from job status.)

This is still a behaviour change worth its own line in the upgrade guide
(§10): a failure that used to show `tolerated: true` only because something
further downstream had the flag now shows `tolerated: false`, and — because
job status is computed the same way — the job itself now ends `failed` where
it used to end `completed`. That status flip has knock-on effects:

- `on_error` fires instead of `on_success`.
- A task-level `retry` (if configured) can now fire where it didn't before.
- `stroem_jobs_completed_total` counts the job under `status="failed"`
  instead of `"completed"`.

(A cancellation-only job — no `Failed` rows, at least one `Cancelled` row —
never reaches `Failed` status under §2.4's order; it stays `Cancelled`. So
there is no case where `hook.failed_steps` needs to populate anything for a
cancellation-driven failure: a `Failed` job always has at least one `Failed`
row to describe it.)

## 9. Migration guidance (for the upgrade doc, §10)

Any workflow relying on 0.17.0's `continue_on_failure`-on-the-dependency
propagation (the exact case this spec removes) needs each such dependent
edge re-expressed as `{step: <name>, optional: true}` on the dependent's own
`depends_on`. This is mechanical but not automatable in general, and it is
**not** a behaviour-preserving search-and-replace — two things make it
broader than a direct swap:

**1. Transitive/leaf catches are easy to miss, and the audit must inspect
flow *definitions*, not job history.** 0.17.0's `caught_steps()` walks
arbitrarily far downstream: for a chain `A → B → C(flagged) → D`, 0.17.0
catches `A`'s failure at `C` (every path from `A` reaches a flag), so `B`
and `C` are skipped `unreachable`, `D` still runs, and the *job* completes —
even though `A`, `B`, and the flag itself are three, two, and one hop apart
respectively. Adding `optional: true` on `D`'s edge to `C` restores `D`
running, but does **not** restore the job completing — under this spec only
`A`'s *own* `continue_on_failure` can do that, and `A` has none.

Critically, this audit cannot be done by looking at what completed or
failed historically: `caught_steps()` is a *structural* property of the flow
graph, computed from the flow definition alone, independent of whether any
row has ever actually failed. A workflow `A(no flag) → B(flagged leaf, no
dependents of its own)` that has never once failed `A` is *already* relying
on this structural catch — there is no past failed row to find by searching
job history, and no outgoing edge on `B` to rewrite (`B` is a leaf). The
first time `A` fails in production, the job silently stops completing. The
0.17.0 upgrade checklist's search ("`continue_on_failure` on a step that has
a `depends_on`") only ever looks at direct edges in job output, not at the
flow definition's full reachability graph; it surfaces neither this
multi-hop case nor this never-yet-triggered leaf case. The migration audit
needs to walk every workflow's **flow definition** (not its job history)
with the 0.17.0 `caught_steps()` logic itself, find every step whose failure
is *structurally* caught today, and for each one confirm the failure's
*origin* — not its immediate catcher — has (or is deliberately given) its
own `continue_on_failure` if the job must still complete when that origin
fails.

**2. The mechanical swap widens tolerance beyond failure, and for "tolerate
failure but still block choice-skip" there is no direct v2 equivalent.**
`optional: true` tolerates *any* terminal outcome, including a choice-class
skip (`condition`/`empty`/`cascade`) — `continue_on_failure` never did; that
was `continue_when_skipped`'s job. A dependency that has `continue_on_failure`
but not `continue_when_skipped` today blocks a downstream choice-skip while
tolerating a downstream failure. There is no way to reproduce exactly that
combination with a single `depends_on` entry under this spec: marking the
edge `optional: true` tolerates both (too permissive — it also passes a
choice-skip); leaving the edge required means the dependency's `Failed`
outcome is an unconditional `BlockFail` per §2.5 (too strict — it no longer
tolerates the failure either, since `continue_on_failure` was removed from
that arm of the verdict). **State this plainly in the migration guide rather
than proposing a workaround that doesn't actually preserve it**: this
specific combination (tolerate failure, block choice-skip, on the same edge)
has no direct equivalent in v2 and requires a deliberate decision — either
accept the edge becomes fully permissive (`optional: true`), or accept it
becomes fully strict (leave it required, lose the failure-tolerance), there
is no third option that reproduces 0.17.0's asymmetric behaviour on one
edge.

**3. Deduplicate `depends_on` before upgrading.** §2.5 makes a duplicate
entry naming the same step twice a hard validation error; it is valid (if
pointless) YAML today. Any workflow with such a duplicate needs it removed
before the upgrade, or the workspace fails to load afterward.

The existing 0.17.0 upgrade guide's checklist item gets a second pass
covering all three of these, not just "search for `continue_on_failure` on a
step that has dependents and add `optional: true`."

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
- `CONTEXT.md` glossary: the `Settlement`, `Dependency gate`, and `Caught
  failure` entries describe the 0.17.0 recursive-catch model; update them to
  the direct per-row check (§2.4).

## 11. Tests

- `gate.rs`: full rewrite of `verdict_table` and `gate_reads_the_dependency_flags_only`
  for the new signature; delete `caught_steps_*` tests; add
  tests for the optional-edge Pass-on-anything behaviour (including Pending
  still waiting); add cases for combined-verdict dominance (§2.5) — a
  required failed entry alongside a pending optional entry resolves
  immediately, not after waiting; add a validation test rejecting a
  duplicate entry naming the same step twice; add a deserialization test
  asserting `{step: a, optionl: true}` (typo'd key) is a hard parse error,
  for both an inline flow step and a step pulled from a referenced library
  action (§4).
- `cascade.rs`: update fixtures using the old `fs()` test helper for the new
  `depends_on` shape; add a case mirroring `ml-impressions-beta` (one
  required + one optional dependency on the same step); add the corrected
  §3 worked example itself as an integration-style test driving all seven
  `recalc-pipeline` rules through one flow and asserting each outcome; add a
  case where a `for_each` placeholder with a tolerated instance failure
  rolls up `completed` and a *required* (non-optional) downstream consumer
  correctly runs, confirming §2.1's "ordinary required edge, nothing
  becomes optional" claim; add a `sequential: true` loop case where an
  instance is `Cancelled` rather than `Failed`, confirming the existing
  advance-past-cancellation behaviour is unchanged by this spec.
- `settlement`/hooks tests: replace assertions built on `caught_steps`
  recursion with the direct per-row `tolerated` check; the existing
  "diamond needs every path" and "long chain" tests for `caught_steps` are
  deleted outright (the mechanism they tested no longer exists).
- `restart.rs`: a test asserting `carried_failed` vs.
  `carried_failed_tolerated` classification uses the direct self-flag rule
  (§4), not a recursive check.
- `stroem-cli` local runner: same gate, same test treatment as `cascade.rs`,
  plus its own test asserting the final `RunOutcome` follows the direct
  self-flag rule (§4) — not just that the shared gate behaves correctly in
  isolation.
- `tests/e2e-workspace/`: extend the conditional fixtures with an
  optional-dependency scenario.
- A DB migration test is not applicable — no schema changes. Two
  *behaviour* migration tests are needed: (1) build a flow shaped like §9's
  `A → B → C(flagged) → D` chain, drive it through a failure at `A`, and
  assert the job now ends `Failed` (matching §9's documented, accepted
  change) with `D` still running/skipped correctly per its own edge's
  `optional` marker — pinning that this specific regression from 0.17.0 is
  deliberate, not accidental; (2) the same chain, but seeded as an
  *already-running* job (rows pre-existing, as if created under 0.17.0 and
  mid-flight at upgrade), advanced under the new gate, asserting the same
  outcome — pinning §12's in-flight-job claim, not just the fresh-job case.

## 12. Rollout

- Version: 0.18.0 (another breaking change, on top of 0.17.0's two days
  ago). Release notes lead with the behaviour change and link both the
  0.17.0 and 0.18.0 upgrade guides.
- No DB migration. `job_step.skip_reason` values are unchanged.
- **In-flight jobs are re-evaluated under the new gate on their next
  cascade** (settlement resolves the task's current config on every advance;
  it is never pinned to the config a job started with — see CLAUDE.md §
  Settlement). A job created under 0.17.0 that is still running at upgrade
  time can see a downstream step it was counting on to transitively catch an
  earlier failure (§9) stop catching it mid-flight, flipping what would have
  been a `completed` job to `failed`. This is the same class of risk
  0.17.0's own rollout note accepted for its cascade change; it is accepted
  here for the same reason (re-running the cascade on every step transition
  makes an atomic "old rules until this job finishes" cutover impractical),
  and should be called out in the release notes alongside the upgrade guide,
  not left implicit.
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
- 2026-09-30, Codex round 1 (thread `01a0f180-78b2-7642-b836-62a928629273`,
  verdict SHIP WITH FIXES): all findings addressed in revision 2.
  - High, `ai_sources` bypassed rule 1: `build-sessions` added as a required
    dependency alongside `ai_maintain` (optional) — §3, backed by the new
    combined-verdict dominance rule in §2.5.
  - High, loop rollup wasn't job-status-only: §2.1 now carves this out
    explicitly as a pre-existing, unchanged, intra-loop exception; §6
    restates it.
  - High, cancellation precedence unspecified: §2.1/§2.4 now state the full
    4-step order matching shipped `settle.rs` (explicit cancel → uncaught
    failure → any cancelled row → completed), narrowing `continue_on_failure`
    to cover only `Failed`, not `Cancelled`.
  - §8 contradiction (tolerated:false yet job completed) resolved — shown
    impossible under the corrected §2.4 order; added the status-flip
    knock-on effects (hooks, retry, metrics) and noted the cancellation/empty-
    `failed_steps` case is pre-existing and out of scope.
  - §3's incorrect claim that `ai_sources`' own `continue_on_failure` did
    nothing — reverted the removal, corrected the reasoning.
  - §10/§11 cross-references fixed; CONTEXT.md glossary added to §10.
  - §2.5 gained combined-verdict dominance and duplicate-entry rejection;
    §2.3 gained the optional-≠-absent clarification.
  - §7 extended: `is_transitively_skippable` must stop propagating through
    optional edges, not just suppress the warning on them.
  - §4 gained a strict-parsing requirement (no silent `unwrap_or_default()`
    on malformed entries) and the full wire-format/consumer list (API, UI
    types, restart preview, CLI).
  - §9 expanded for transitive/leaf catches (a multi-hop chain's original
    failure needs its own flag, not just a fix at the immediate catcher) and
    for the choice-skip-tolerance widening the mechanical migration
    introduces.
  - §12 gained an explicit in-flight-job upgrade-risk paragraph; §11 gained
    a behaviour migration test and combined-verdict/duplicate-entry test
    cases.
- 2026-10-01, Codex round 2 (same thread, verdict Still SHIP WITH FIXES):
  the rule-1 bug fix, cancellation precedence, and 6 of 15 findings
  confirmed resolved; remaining findings addressed in revision 3.
  - High, §9's choice-skip migration remedy didn't actually preserve the
    old behaviour (keeping the edge required still unconditionally blocks
    the failure it needed to tolerate). §9 now states plainly that
    "tolerate failure, block choice-skip" on one edge has **no direct v2
    equivalent** — a deliberate decision is required, not a workaround.
  - High, the `DependsOnEntry::Detailed` sketch had no
    `deny_unknown_fields`, so the "strict parsing" prose wasn't actually
    enforced by the code shown — a typo'd key would silently parse and
    default `optional` to `false`. §4 now calls this out explicitly as
    load-bearing, with the reasoning for why `#[serde(untagged)]` needs it.
  - §4/§11: restart preview's `carried_failed`/`carried_failed_tolerated`
    classification and the CLI's final `RunOutcome` are now named
    explicitly as needing the direct self-flag rule (not just a
    `depends_on`-shape adapter), with their own tests; `web/api/jobs.rs`
    added alongside `tasks.rs`.
  - §9's audit guidance now covers flow *definitions* structurally (walking
    `caught_steps()`'s own logic over every workflow), not just observed
    historical failures — a never-yet-triggered leaf catch has no job
    history to find it by.
  - §2.1: loop wording corrected — a tolerated instance failure rolls the
    placeholder up as a genuinely `Completed` row whose downstream required
    edges pass normally (not "become optional"); "cancellation not covered"
    reworded to be explicit it's scoped to job-status only, not to a loop's
    own sequential-advance behaviour.
  - §8: removed the self-contradictory "cancellation-driven Failed job hook
    gap" paragraph — shown to be impossible under §2.4's own precedence, so
    there was nothing to describe as a gap.
  - §2.3: "unchanged meaning" narrowed to "unchanged syntax" (behaviour did
    change, per §2.5).
  - §2.5/§9: duplicate-entry rejection is a new, user-facing validation
    change (today's `depends_on: [a, a]` is valid YAML) — added as an
    explicit migration-checklist item.
  - §11: added the corrected 7-rule worked example as an actual test, a
    strict-parsing rejection test, a loop-rollup-into-required-consumer
    test, a sequential-loop-cancellation test, restart/CLI outcome tests,
    and a second migration-chain test covering an already-running
    (pre-upgrade) job, not just a freshly-created one.
