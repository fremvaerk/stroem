# Dependency Conditions — typed per-edge outcome acceptance — Design

Status: revision 2, Codex round 1 findings addressed; awaiting further review
Ships in: 0.18.0 (breaking, on top of the already-breaking 0.17.0)
Supersedes: `2026-09-30-dependency-gate-v2-design.md` in full (abandoned before
sign-off — a fundamentally different model, per Codex's unconstrained design
critique, 2026-10-01) and, through it, `2026-09-26-dependency-gate-design.md`
§2.1, §2.4, §14.

## 1. Problem

0.17.0's per-dependency-broadcast flags (`continue_on_failure` /
`continue_when_skipped`, read from the dependency, applied to every
dependent alike) and the v2 redesign's per-edge binary `optional` marker
were both evaluated against the concrete requirements of
`jobs/recalc-pipeline` (prod job `a45096f8`, ClickHouse disk-full on
`build-sessions`; see the v2 spec's §1 for the full incident). v2 fixed
0.17.0's execution bugs, but round 2 review of v2's own migration guidance
surfaced a real expressiveness gap it could not work around: a dependency
with `continue_on_failure: true` but not `continue_when_skipped` today means
"tolerate my failure, but still block a dependent if I'm skipped by choice."
There is no single `depends_on` entry under v2's binary required/optional
split that reproduces this — marking the edge `optional: true` tolerates
both outcomes; leaving it required blocks both.

Asked for an honest, unconstrained second opinion (ignore backward
compatibility, pick the best design), Codex proposed replacing the binary
marker with **typed per-edge outcome acceptance**: each edge names exactly
which of its dependency's terminal outcomes satisfy it, plus explicit
`all`/`any` grouping for combining dependencies in shapes a flat conjunction
list can't express at all (e.g. "this must finish, and at least one of these
two must succeed"). The user chose to adopt it.

## 2. The model

Three independent questions, matching the existing code's own boundaries
(`verdict()`/`gate()`/settlement) so this is a policy change inside those
boundaries, not a new architecture:

1. **Readiness** — which dependencies must reach a terminal state before
   this step is decided?
2. **Selection** — which combination of those terminal outcomes satisfies
   this step?
3. **Accounting** — does this step's own failure fail the job?

Accounting (3) is unchanged from the v2 spec: `continue_on_failure: true` on
a step means its own `Failed` status doesn't fail the job, self-scoped only,
no propagation to anyone. §6 restates it. This spec replaces (1) and (2) —
`depends_on`'s binary required/optional shape (v2 §2.3) — and retires
`continue_when_skipped` as a producer-side broadcast (v2 §2.2): a consumer
now states what it accepts directly, instead of a producer declaring "my
skip is fine for everyone who depends on me."

### 2.1 Outcomes

Every dependency resolves, once terminal, to exactly one of five outcomes:

| Outcome | Maps to |
|---|---|
| `completed` | `status = completed` |
| `failed` | `status = failed` |
| `cancelled` | `status = cancelled` |
| `skipped` | `status = skipped`, reason `condition` or `empty` — the step's *own* choice (`when` false, or an empty `for_each`) |
| `omitted` | `status = skipped`, reason `cascade` or `unreachable`, or NULL/unrecognised — blocked by *its own* dependency tree, not by its own choice |

**No DB schema change.** `skipped` and `omitted` are a grouping of the
*existing* `SkipReason` values (`Condition`/`Empty` vs.
`Cascade`/`Unreachable`/NULL) that the gate derives when matching an
`accept` set — `job_step.skip_reason` keeps recording `unreachable` for
every omitted row exactly as today (no new stored value); the grouping is
never itself written to the database.

**The existing UI label for `unreachable` ("upstream failed") stops being
accurate and needs updating.** Today "unreachable" really only ever means a
dependency failed outright, so the label is correct. Under `accept`, a step
can be omitted for reasons that have nothing to do with an upstream
*failing* — e.g. `{step: x, accept: [failed]}` (deliberately accepting only
a failure) is unsatisfied, and the dependent is omitted, precisely when `x`
*succeeds*. §8 tracks the UI copy change this requires (replace "upstream
failed" with outcome-neutral wording such as "a dependency condition was not
satisfied").

### 2.2 `depends_on` entries

A `depends_on` list is implicitly an `all` group. Each entry is one of:

```yaml
depends_on:
  - plain-name                              # sugar for {step: plain-name, accept: [completed]}
  - step: name
    accept: [completed, failed]             # explicit outcome set
  - step: name
    accept: terminal                        # sugar for all five outcomes — "I don't care"
  - all: [a, b, c]                          # nested AND group (rarely needed at top level — a flat list already is one)
  - any: [a, b]                             # nested OR group
```

`accept` is a non-empty subset of `{completed, failed, cancelled, skipped,
omitted}`, or the literal `terminal` (sugar for all five). A bare string is
sugar for `accept: [completed]` — today's default meaning, unchanged for
every workflow that doesn't need anything else. `all`/`any` entries nest
arbitrarily (an `any` can contain an `all`, a `step`, or a bare string); they
share one recursive entry type.

### 2.3 Readiness barrier and timing

**Every step named anywhere in a `depends_on` tree must reach a terminal
state before the dependent is decided** — run, or `omitted`. This is a
deliberate change from the gate's current (0.17.0 and v2) behaviour — stated
precisely, not loosely: today only a hard block (`Verdict::BlockFail`,
`gate.rs:99`) decides immediately, regardless of a still-pending sibling; a
choice-class block (`BlockSkip`) already waits behind a pending sibling.
Under this model *both* cases wait uniformly: the decision always waits for
every referenced step to finish, then evaluates the whole tree once. Two
cases need their own test to pin the change precisely: an `any` already
satisfied by one child, with a second child still pending (today's
equivalent would proceed immediately on `Pass`; this model waits); and a
tree already conclusively unsatisfiable by one child, with a second child
still pending (today's `BlockFail` would omit immediately; this model
waits, which also delays any downstream cleanup step that was only waiting
to react to the eventual `omitted`).

Trade-off: slower propagation in the specific case just described —
accepted, because it replaces the two-pass `all_deps_skipped` machinery the
old gate needed (0.16.2 onward, carried into v2) just to get "fail fast on a
hard block" and "wait for a still-running choice-skip sibling" right at the
same time. One evaluation, one rule, no phase-ordering subtlety to get
wrong — see §5 for what this does and does not simplify in the cascade
itself (less than originally claimed in revision 1 of this spec; a real
issue was found there).

Once every referenced step is terminal, evaluate the tree bottom-up: a
`{step, accept}` leaf is satisfied iff that step's outcome (§2.1) is in its
`accept` set; an `all` node iff every child is satisfied; an `any` node iff
at least one child is satisfied.

- Tree satisfied → evaluate the step's own `when` next (unchanged ordering
  from 0.17.0/v2: dependency condition before `when`, never the reverse).
- Tree not satisfied → the step is `omitted` (skip reason `unreachable`,
  written for diagnostics exactly as the old gate's `BlockFail` case did).

### 2.4 Worked example: mixed fan-in

```yaml
publish:
  action: publish
  depends_on:
    - source                                    # must complete
    - step: enrichment
      accept: [completed, failed]                # tolerates a failed enrichment, not a skip
    - step: audit
      accept: terminal                           # pure ordering: wait for it, don't care how it ends

ranked:
  action: rank-mirrors
  depends_on:
    - step: audit
      accept: terminal
    - any: [mirror-a, mirror-b]                  # at least one mirror must complete
```

`enrichment` and `audit` each carry their own `continue_on_failure: true` if
their own failure shouldn't fail the job (§6) — an independent decision from
whether `publish` accepts their failure.

This is the exact case v2 had no answer for: a dependency that tolerates
failure but blocks a choice-skip is now simply `accept: [completed,
failed]` — `skipped` and `omitted` just aren't in the set.

## 3. `recalc-pipeline` revisited

The same seven rules the v2 spec verified (its §3) hold here with lighter
annotation, since most of its edges never needed anything beyond "required"
or "tolerate everything":

- `ai_sources`: `depends_on: [build-sessions, {step: ai_maintain, accept: terminal}]`.
- `ml-prediction-beta`/`-stage`: unchanged, `continue_on_failure: true`,
  no change to their `depends_on`.
- `ml-impressions-master`: `depends_on: [ml-prediction-master, {step:
  dwell-time, accept: terminal}]` — required prediction, ordering-only
  `dwell-time`. (v2's full diff included this edge for the master variant
  too, since `dwell-time`'s old broadcast reached all three impressions
  steps alike; an earlier revision of this section only showed beta/stage
  and dropped master by omission — fixed here.)
- `ml-impressions-beta`: `depends_on: [ml-prediction-beta, {step:
  dwell-time, accept: terminal}]` — its *own* prediction dependency,
  `ml-prediction-beta`, required; `dwell-time` ordering-only, same as
  master's.
- `ml-impressions-stage`: `depends_on: [ml-prediction-stage, {step:
  dwell-time, accept: terminal}]` — mirrors beta with its own prediction
  dependency, `ml-prediction-stage`. (Spelled out separately from beta
  here, rather than "mirror beta" shorthand, so it isn't misread as also
  depending on `ml-prediction-beta`.)
- `merge-ml`: `depends_on: [ml-impressions-master, {step:
  ml-impressions-beta, accept: terminal}, {step: ml-impressions-stage,
  accept: terminal}]`.

Nothing here needed `any`/`all` grouping — `recalc-pipeline` is a pure
fan-in/fan-out DAG with no "at least one of these" requirement. The grouping
construct exists for workflows that do have one (§2.4's `ranked` example),
which this one doesn't.

## 4. Loops: a real behaviour change, called out explicitly

0.17.0 and v2 both kept today's shipped behaviour: a `for_each` placeholder
with `continue_on_failure: true` and at least one failed instance rolls up
as a `completed` row — the failure is hidden from the placeholder's own
terminal status. Under this model that rollup **stays `failed`**:
`continue_on_failure` still protects the *job* from that failure (§6), but a
dependent that wants to proceed past a tolerated loop failure must say so
explicitly, via `accept: [completed, failed]` (or `terminal`) on its edge to
the placeholder — the same mechanism as any other tolerated failure, not a
loop-specific rollup exemption. More consistent (one mechanism, not a
special case), but a genuine behaviour change for any workflow relying on
the rollup-to-`completed` shortcut — §9 calls this out as a migration item.

**This requires an actual code change, not just removing a branch.**
`cascade.rs:345` today only constructs the rollup's `output` array
(`RollupOutcome::Completed`'s payload) when the outcome is `Completed` — it
is never built for `RollupOutcome::Failed`, because nothing could read a
failed rollup's output before (a `Failed` placeholder always blocked every
dependent under 0.17.0/v2). Now that a `Failed` rollup can legitimately be
consumed by a dependent with `accept: [completed, failed]`, the output
array must be built for **every** rollup outcome, using the existing
documented rule unchanged (CLAUDE.md § For-Each Loops: "one element per
existing instance, `null` where the instance produced none") — not just for
`Completed`. The CLI's separate `for_each` rollup implementation
(`stroem-cli/src/local/run.rs:333`) needs the identical fix; it is a second,
independent implementation of the same rollup logic, not a wrapper around
`cascade.rs`'s.

**Cancellation-only loops are a known, pre-existing quirk this spec does
not change or attempt to fix.** Today's rollup only checks for *failed*
instances (`any_failed`, `cascade.rs:339`) to decide `Completed` vs.
`Failed` — a loop whose instances are all `cancelled` (no `failed`
instances at all) rolls up `Completed` regardless of `continue_on_failure`,
matching the documented rule ("only failed instances fail a loop; cancelled
instances count as terminal but do not," CLAUDE.md § For-Each Loops). That
is unaffected by removing the `cof` branch from the Completed/Failed choice
— "always report the true outcome" in the paragraph above describes the
*failure* case only, not a general truthfulness guarantee. The resulting
inconsistency (a dependent downstream of the placeholder sees `Completed`
and runs normally, while the job can still independently settle `Cancelled`
per §6 step 3, because the underlying instance *rows* are `Cancelled`
regardless of what the placeholder rolled up to) already exists in shipped
0.17.0 and is not introduced by this spec. Resolving it would mean deciding
whether instance-row cancellations should ever be excluded from job-status
consideration — a real question, but orthogonal to this redesign and left
as a follow-up (§12).

Sequential loop advancement past a failed or cancelled instance (today's R5,
`cascade.rs::phase_rollup`) is unchanged — still governed by the
placeholder's own `continue_on_failure`, which continues to serve double
duty here (don't stop promoting instances; don't fail the job for it).
**This is a deliberate, acknowledged departure from Codex's original
proposal**, which recommended splitting loop-iteration policy out from job
accounting entirely (its own "introduce an explicit iteration-failure
policy" recommendation). This spec does not adopt that split: none of
`recalc-pipeline`'s seven rules, nor the prod incident that motivated this
redesign, need it, and introducing a third flag purely for loop-advancement
policy is surface area this spec isn't willing to add speculatively. §12
tracks it as a non-goal, not an oversight — if a concrete workflow need
shows up, revisit then.

## 5. Server cascade (`crates/stroem-server/src/cascade.rs`)

Revision 1 of this spec claimed P2 could simply be deleted and folded into
P1, on the theory that the uniform readiness barrier (§2.3) made the
two-phase split's original purpose — handling "all deps skipped" vs. "the
rest" as two timing-ordered cases — moot. **Codex found this is wrong**: a
single linear P1 scan, run once per `execute()` call, only propagates one
step's decision to its *immediate* dependents within that call. A chain
longer than one hop — concretely, `x` already unreachable, `x → a → b`, with
an independent root loop `p` whose `when` reads whether `b` is defined —
needs `a`'s skip (decided during the scan) and `b`'s skip (which depends on
`a`'s *just-decided* state) to both land before P3 builds the context `p`'s
`when` is evaluated against. A single static-snapshot scan over all pending
rows does not guarantee that depth-2 propagation happens before the scan
ends; today's actual two-phase P1-then-P2 split is precisely what makes
*that specific case* work by giving the system a second look at rows P1's
single pass hadn't yet resolved. Removing P2 outright, as revision 1 did,
loses that second look and can leave `b` undecided when P3 runs, making `p`
permanently (and wrongly) condition-skipped on stale information.

**The fix is not to restore the old two-phase split verbatim — the
one-evaluation-per-tree model (§2.3) genuinely doesn't need its specific
"all-skipped vs. the rest" distinction — but to replace the fixed two-pass
shape with an unbounded fixpoint, since a fixed number of passes (whether
one or two) is the wrong primitive for a dependency chain of arbitrary
depth:**

- **P1** (cascade-skip + promote with `when`) runs to a **fixpoint within
  one `execute()` call**: repeatedly re-scan every still-`pending`,
  non-placeholder row against the current in-memory snapshot, apply every
  tree-evaluation decision (`Open`/`Omitted`) it can now make, and recurse
  — because applying those decisions can make *further* rows decidable in
  turn (exactly the `a → b` case above) — until one full re-scan produces
  no new decisions. **P2 is deleted**, not folded into a single scan: its
  old job (a second look at what one pass missed) is subsumed by the
  fixpoint running as many looks as the chain actually needs, not a fixed
  one or two.
  - *Termination is guaranteed*: the flow graph is a finite DAG (cycles are
    rejected at validation, §8); each fixpoint iteration that produces at
    least one `Change` strictly shrinks the set of still-`pending` rows
    (a decided row never re-enters pending), so the loop terminates in at
    most `N` iterations for `N` pending rows, and terminates immediately
    (zero extra iterations) once no iteration produces a change.
  - Within a single iteration, rows are still evaluated against *one*
    static snapshot (consistent with the project's existing "pure fixpoint,
    not an ad hoc promote→skip→expand loop" design) — only the *outer*
    loop (re-snapshot, re-scan) is new; a single iteration's internal
    ordering is not relied on for correctness, only the fact that the outer
    loop repeats until stable.
- **P0** (rollup) and **P3** (adopt + expand placeholders) are unchanged in
  shape and in when they run relative to P1 — P3 still runs once, after P1
  has *fully* stabilized (not interleaved with it), so its `when`-evaluation
  context (context B) reflects every decision the fixpoint could make, not
  a partial one. This is what fixes the `x → a → b → p` case: `b` is fully
  decided by the time P3 builds context B, because P1's fixpoint doesn't
  hand off to P3 until no row is left that it can still decide.
  P0's rollup logic itself is unaffected by this section — see §4 for its
  own, separate fix (the output-array and Completed/Failed branching).

**Required regression test, directly pinning Codex's counter-example**: a
flow `x(no flag) → a → b`, `x` already `unreachable` from a prior cascade,
plus an independent root step/loop `p` with `when: "{{ b is defined }}"` (or
equivalent "does this other step have a row yet" check) — assert that `b`
is fully decided (`omitted`) *and* `p`'s `when` sees that decision, within
one `execute()` call, not requiring a second external trigger.

## 6. Job status and hooks (unchanged from v2)

`continue_on_failure: true` on step `X` means `X`'s own `Failed` status does
not fail the job. No propagation to dependents — that's entirely `accept`'s
job now (§2). Job status keeps the exact 4-step precedence the v2 spec
established, matching shipped `settle.rs`:

1. Explicit job cancellation (external signal) → `Cancelled`, unconditional.
2. Else, any `Failed` row without `continue_on_failure = true` on its own
   flow step → `Failed`.
3. Else, any `Cancelled` row → `Cancelled`.
4. Else → `Completed`.

`hook.failed_steps[].tolerated` is the same direct self-flag lookup v2's §8
specified — unaffected by this spec, since accounting (§2's question 3) is
untouched.

## 7. Data model (`stroem-common`)

`FlowStep.depends_on` becomes a recursive type:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepEntry {
    pub step: String,
    pub accept: AcceptSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllEntry {
    pub all: Vec<DependsOnEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnyEntry {
    pub any: Vec<DependsOnEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DependsOnEntry {
    Name(String),
    Step(StepEntry),
    All(AllEntry),
    Any(AnyEntry),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcceptSet {
    Terminal(TerminalKeyword),  // the literal string "terminal"
    Outcomes(Vec<Outcome>),     // validated non-empty, no duplicates
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome { Completed, Failed, Cancelled, Skipped, Omitted }
```

Revision 1's sketch put `#[serde(deny_unknown_fields)]` directly on
`DependsOnEntry`'s enum variants — **that doesn't compile**:
`deny_unknown_fields` is a container attribute (it belongs on a struct, or
on an enum as a whole), not a per-variant attribute, and serde rejects it in
that position. The fix is the standard pattern for this exact situation:
give each object-shaped alternative its own named struct
(`StepEntry`/`AllEntry`/`AnyEntry`), put `deny_unknown_fields` on each
*struct*, and have the untagged enum wrap them as newtype variants. This is
load-bearing, not decorative — the exact lesson from the v2 spec's round 2
review: a typo'd key (e.g. `{step: a, optionl: true}` under the old v2
shape, or `{step: a, accpet: [completed]}` here) must be a hard parse error,
not silently dropped. Add a test asserting a mapping with **two**
discriminating keys at once (e.g. both `step` and `any` present) is also
rejected, not resolved by picking whichever variant happens to match first.
An empty `Outcomes` list, or an `all`/`any` with zero children, is a
validation error, not a vacuous pass (an empty `all` would trivially always
be satisfied; an empty `any` would trivially never be) — except the
top-level, *entirely absent* `depends_on` on a step with no dependencies at
all, which is unchanged from today and satisfied immediately (§8 states
this exception precisely).

**Parse-error propagation is unconditional, for both code paths that read a
flow step.** `workflow.rs:578`'s existing pattern — falling back to an empty
`depends_on` list on a deserialization error — must be removed for inline
flow steps *and* for steps pulled in from a referenced library action; this
was already v2's requirement and is carried forward unchanged, stated here
explicitly so it isn't lost in translation again. Separately, the
permissive reference-step struct that currently still accepts
`continue_when_skipped` (`workflow.rs:458`) must not simply stop
recognizing that field once it's retired (§2) — a workspace config that
still sets it needs a named, actionable validation error ("`
continue_when_skipped` was removed; set `accept: [completed, skipped]` on
the dependent's edge instead"), not silent acceptance-and-ignore. Tests for
both of these must exercise actual flow-step loading (the full parse path),
not just the `DependsOnEntry` type in isolation.

`depends_on`'s consumer list is the same one v2's §4 already enumerated,
carried over unchanged — every place that reads `depends_on` is reading a
richer type now, not a new place: cycle detection and validation
(`validation.rs`), the render-context dependency graph, the docs generator,
the UI's DAG payload, `web/api/tasks.rs` **and** `web/api/jobs.rs`, the UI's
TypeScript types (`ui/src/lib/types.ts`), the restart-preview computation
(`restart.rs`, which independently classifies `carried_failed` vs.
`carried_failed_tolerated` and needs the same direct self-flag rule, not
just a shape update), and the CLI local runner
(`stroem-cli/src/local/run.rs`, whose final `RunOutcome` needs the same
treatment).

## 8. Validation (`stroem-common::validation`)

- Every `step:` name referenced anywhere in the tree (including nested
  inside `all`/`any`) must exist in the flow — the same "unknown dependency"
  check as today, applied recursively.
- Cycle detection walks every name referenced in the tree exactly as it
  walks a flat list today — grouping doesn't change the dependency graph,
  only how the gate interprets a satisfied/unsatisfied result once every
  named node is terminal. A step with **no** `depends_on` at all (field
  absent, or explicitly `depends_on: []`) is unchanged from today: it has no
  readiness barrier and its own `when` is evaluated immediately. That
  exception applies only at this top level; an explicit, *nested* `all: []`
  or `any: []` written somewhere inside a tree is still a validation error
  (§7) — a user who writes that almost certainly meant something else, and
  there's no equivalent "this is just how an empty root behaves" precedent
  for a group that was *deliberately* nested.
- **Duplicate-reference rejection is scoped to direct siblings of the same
  `all`/`any` node, not the whole tree.** Revision 1 rejected the same step
  name appearing *anywhere* in a tree — that's too strict: it would reject
  a legitimate expression like `any: [{all: [A(completed), B(completed)]},
  {all: [A(failed), C(completed)]}]` ("`A` and `B` both completed, OR `A`
  failed and `C` completed"), which necessarily references `A` twice with
  *different* `accept` sets, once per branch of a genuine OR. That isn't a
  mistake — collapsing it into one occurrence would lose the conditional
  relationship with `B`/`C` entirely. The rule that still catches the
  original motivating case (`[a, {step: a, optional: true}]` under v2's flat
  list, which *was* two siblings of the implicit root `all`) without
  rejecting cross-branch repetition: reject a duplicate name among the
  **direct children of the same `all`/`any` node** (including the implicit
  root group); allow the same name to recur across *different* branches of
  nested groups. The readiness barrier and cycle detection still treat the
  name as one graph node regardless of how many branches reference it —
  only the per-branch `accept` evaluation differs.
- The 0.17.0-era "step will be skipped whenever its dependency is skipped,
  add `continue_when_skipped`" warning is retired along with the flag it was
  about — there's nothing left for it to warn about, since the consumer now
  states its own tolerance directly in `accept` rather than relying on an
  upstream broadcast it could forget to add.

## 9. Migration (0.17.0 → this design)

v2 never shipped — it's superseded before sign-off — so there is one
migration path to document: 0.17.0 (released, likely not yet deployed to
production per the v2 spec's rollout note) to this design.

**The translation is precise, not a two-bullet approximation — it depends on
exactly which of the two old flags the dependency carried**, since each one
tolerated a different, specific outcome set (`gate.rs:55`), and `accept:
[completed, failed]` alone silently drops outcomes the old flag(s) covered:

| Dependency's 0.17.0 flags | Dependent's new `accept` |
|---|---|
| neither | `[completed]` (bare name, unchanged default) |
| `continue_when_skipped` only | `[completed, skipped]` |
| `continue_on_failure` only | `[completed, failed, cancelled, omitted]` (i.e. `terminal` minus `skipped` — tolerates every failure-class outcome, including `cascade`/`unreachable`-turned-`omitted`, but still blocks a choice-skip) |
| both | `terminal` |

The "tolerate failure, block choice-skip" case v2's own migration guidance
had no answer for is row 3 — `continue_on_failure` only — now expressible
exactly, not approximated. Getting rows 3 and 4 right matters specifically
because `omitted` (today's `cascade`/`unreachable`) was tolerated by
`continue_on_failure` alone, and is easy to drop by writing `[completed,
failed]` instead of the full row-3 set — a worked example:
`A(when=false) → B(continue_when_skipped=true) → C` lets `C` run today once
`B` is cascade-skipped; migrating `B`'s flags to row 2's `accept:
[completed, skipped]` on `C`'s edge preserves this exactly (cascade-skip of
`B` is `skipped` in the new vocabulary, since `B`'s own skip originates from
its own choice-class ancestor, not a failure) — but naively using
`[completed, failed]` here would be wrong in the other direction, rejecting
the very outcome the migration is supposed to preserve.
- The loop-rollup behaviour change (§4) needs its own checklist item: any
  workflow whose dependents currently rely on a tolerated loop failure
  rolling up as `completed` needs those dependents' edges updated to
  `accept: [completed, failed]`.
- The multi-hop/leaf-catch structural audit v2's §9 specified still
  applies unchanged: walk flow *definitions* (not job history) with the old
  `caught_steps()` logic to find every failure 0.17.0 structurally catches
  several hops downstream — that recursive catch mechanism (precisely:
  `caught(s) = s.continue_on_failure || (dependents non-empty && *every*
  one of them is caught)` — **every** downstream path needs a catcher for
  `s` to be considered caught, not merely one; getting this backwards
  understates how many workflows are actually relying on it) never had a
  replacement in v2, and still doesn't here. The origin of a failure still
  needs its own `continue_on_failure` if the job must keep completing when
  it fails; no amount of `accept` tuning on downstream edges substitutes
  for that, because `accept` only ever answers "do I run," never "does the
  job still count as succeeded."
- `depends_on: [a, a]`-style duplicates (valid, if pointless, YAML today)
  must be deduplicated before upgrading, same as v2's §9 already noted.

## 10. Tests

- `gate.rs` (or its successor module): tree-evaluation unit tests for every
  outcome against every `accept` combination, including `terminal`; `all`/
  `any` nesting (including an `any` containing an `all`); the uniform
  readiness barrier (a satisfied `any` still waits for a `Pending` sibling,
  and a conclusively-unsatisfiable tree still waits for a `Pending` sibling
  too — §2.3's two named cases); sibling-scoped duplicate-reference
  rejection *and* a positive test that the same name legitimately recurring
  across different `any`/`all` branches (§8's `A`-in-two-branches example)
  is accepted; `deny_unknown_fields` rejection for a typo'd key, for both an
  inline flow step and a step pulled from a referenced library action, and
  for a mapping carrying two discriminating keys at once (e.g. both `step`
  and `any`); a test that a workspace still setting the retired
  `continue_when_skipped` gets the named migration error, not silent
  acceptance (§7).
- `cascade.rs`: the §5 fixpoint regression test (`x → a → b` plus an
  independent `p` whose `when` reads `b`'s existence) — the specific case
  Codex found broken by revision 1's "just delete P2" claim — is mandatory,
  not optional; the `recalc-pipeline` worked example (§3) as an
  integration-style test driving all seven rules; a loop-rollup test
  asserting a tolerated instance failure now rolls up `failed` (not
  `completed`) *with its output array populated* (not just the status —
  §4's data-loss fix), and that a dependent with `accept: [completed,
  failed]` on that placeholder still runs and can read that output, while
  one without it is `omitted`; a cancelled-only loop test pinning the
  existing (unchanged, pre-existing) quirk that it still rolls up
  `completed` while the job can independently settle `cancelled` (§4) — a
  regression test for the *current* behaviour, not a fix.
- `restart.rs` / CLI local runner: outcome-classification tests using the
  direct self-flag rule, per §7's note that these aren't just shape
  adapters; the CLI's own `for_each` rollup implementation
  (`run.rs:333`) gets the identical output-array-on-failure test §4/this
  section's `cascade.rs` bullet specifies — it is a separate
  implementation, not a thin wrapper, and needs its own coverage.
- `tests/e2e-workspace/`: a fan-in fixture using `any`/`all` grouping
  (§2.4's `ranked` shape), since nothing in `recalc-pipeline` itself
  exercises grouping.
- Migration-behaviour tests: the same `A → B → C(flagged) → D` structural-
  catch scenario v2's §11 specified, both freshly created and seeded as an
  already-running (pre-upgrade) job.

## 11. Rollout

Same as v2's §12, unchanged: version 0.18.0, no DB migration, in-flight jobs
re-evaluated under the new gate on their next cascade (same accepted risk
class as both 0.17.0's and v2's own rollout notes), and the same open
question about whether production has actually reached 0.17.0 server code
yet — worth confirming before release so the upgrade communication targets
the right starting point.

**Loops add their own timing wrinkle, worth calling out explicitly rather
than leaving implicit in the general in-flight-job note above.** P0 only
visits placeholders with status `running` — a loop that had *already*
rolled up (any status, including a pre-upgrade `completed` rollup that hid
a tolerated failure) before the upgrade deployed is never re-evaluated; its
historical rollup status is frozen as-is. A structurally identical loop
still `running` at upgrade time rolls up under the *new* rule the first
time it finishes afterward. Two otherwise-identical loops can therefore
show different final statuses purely because of which side of the
deployment they happened to finish on — a test for this specific timing
case (one loop seeded already-rolled-up pre-upgrade, one seeded mid-flight)
belongs alongside the other migration-behaviour tests in §10.

## 12. Non-goals

- Per-reference *diagnostic* detail on *why* a tree was unsatisfied (which
  specific branch) beyond today's single `skip_reason` on the omitted row —
  **this is an explicit scope reduction from Codex's original proposal**,
  which included an operator-facing evaluation trace (e.g. "Waiting for
  audit. Source completed and satisfies its dependency. Enrichment failed
  and is explicitly accepted.") as part of the core design. This spec defers
  it rather than silently dropping it. Revisit if real operator feedback
  says "omitted" alone isn't enough to debug a complex `any`/`all` tree.
- Short-circuiting an `any` group once satisfied, ahead of its slower
  siblings — explicitly rejected per §2.3's uniform readiness barrier;
  revisit only with a concrete latency complaint.
- Renaming `continue_on_failure` — the user decided to keep the name for
  now (2026-10-01).
- Any change to how `when` or `for_each` are evaluated, beyond §4's
  loop-rollup status fix.
- Named, reusable sub-groups — `all`/`any` are inline combinators scoped to
  one step's `depends_on`, not separately named/referenceable entities (no
  system in Appendix A's research has these either).
- Deciding whether a cancelled `for_each` instance row should ever be
  excluded from job-status consideration when its placeholder rolled up
  `completed` (§4's cancellation-only-loop quirk) — pre-existing in shipped
  0.17.0, not introduced or made worse by this spec, and left for a
  dedicated follow-up rather than folded into this redesign.

## Appendix: why not Argo's full expression strings, or Airflow's `trigger_rule`

| Model | Strength for Strøm | Weakness for Strøm |
|---|---|---|
| Argo-style boolean expression strings (`"A.Succeeded && (B.Succeeded \|\| C.Succeeded)"`) | Maximally expressive in one line | A compact string hides broad allowances at a glance; needs a parser/validator for a mini-language; harder to show structurally in the UI's DAG view than a typed tree |
| Airflow-style `trigger_rule` enum (`all_success`, `none_failed`, …) | Excellent concise names for common joins | One rule per *task*, blanket across all its upstreams — cannot express `recalc-pipeline`'s actual need (one required dependency, one ordering-only dependency, on the *same* step) |
| **This spec: typed `{step, accept}` + `all`/`any` tree** | Same expressiveness as Argo for the cases that matter, but structured (validatable field-by-field, inspectable by tooling/UI) rather than a string to parse; keeps the common case (`depends_on: [a, b]`) exactly as simple as today | More YAML than Argo's one-liner for a complex condition; still real Boolean complexity in the `all`/`any` case, the tree doesn't make that free |

Full systems table (Tekton, GitHub Actions, GitLab, AWS Step Functions,
Dagster) carried over unchanged from `2026-09-30-dependency-gate-v2-design.md`'s
Appendix A, with two corrections Codex flagged on review that the inherited
table got wrong and this spec should not keep repeating:

- **Tekton's `onError: continue`** doesn't just change whether the
  *PipelineRun* is marked failed (a pure status-accounting precedent, as
  the inherited table implied) — it also affects whether execution
  continues and whether that task's results remain usable downstream. It's
  closer to this spec's `continue_on_failure` + a dependent's `accept`
  together than to `continue_on_failure` alone.
- **GitHub Actions' implicit `success()` check** is displaced specifically
  by a *status-check function* (`success()`, `failure()`, `always()`,
  `cancelled()`) appearing in a custom `if:`, not by an arbitrary custom
  condition as such — the inherited table's framing overstated how easy the
  footgun is to trigger. The underlying reason this spec keeps `when`
  separate from dependency-status gating (§1.1 of the v2 spec) still
  stands on its own merits regardless of this correction.

It was Argo and Airflow specifically that this spec's shape was chosen
against; nothing else in that inherited comparison changes with this model
beyond the two corrections above.

## Review Log

- 2026-10-01: Codex's unconstrained design critique (same thread as the v2
  spec's reviews, `01a0f180-78b2-7642-b836-62a928629273`) proposed this
  model in place of v2's per-edge binary `optional` marker, specifically to
  close the "no v2 equivalent" gap v2's own round 2 review found in its
  migration guidance (v2 §9, "choice-skip migration remedy incorrect"). User
  chose to adopt it over patching v2 further. User declined to rename
  `continue_on_failure` for now, open to revisiting the name later.
- 2026-10-01, Codex round 1 (same thread, verdict SHIP WITH FIXES on this
  spec): core model confirmed faithful to the proposal (typed acceptance,
  `all`/`any`, uniform barrier, independent accounting); 15 findings fixed
  in revision 2.
  - **Real design bug, not wording**: §5's claim that P2 simply folds into
    P1 was wrong — Codex traced a concrete regression (`x → a → b` plus an
    independent `p` reading `b`'s existence) where a single P1 scan doesn't
    propagate a 2-hop skip before P3 builds its `when`-evaluation context.
    Replaced with: P1 runs to a fixpoint within one `execute()` call
    (bounded, terminating argument included), P3 runs only after that
    fixpoint stabilizes. Added as a mandatory regression test.
  - **Real data-loss bug**: a tolerated-`Failed` loop rollup never had its
    output array built (`cascade.rs:345` only did this for `Completed`,
    since nothing could read a Failed rollup before). §4 now requires
    building the array for every outcome, and requires the identical fix in
    the CLI's separate rollup implementation (`run.rs:333`).
  - §4: acknowledged the cancelled-only-loop / job-status inconsistency as
    a known, pre-existing (not newly introduced) quirk, tracked as a
    follow-up rather than silently left or incorrectly claimed fixed.
  - §4: the loop-advancement/job-accounting coupling is now stated as a
    deliberate, named departure from Codex's original proposal (which
    wanted it split out), not glossed as "unchanged."
  - §7: fixed the `deny_unknown_fields`-on-enum-variants sketch, which
    doesn't compile under serde — moved to named structs per variant, the
    standard pattern. Added parse-error-propagation and
    `continue_when_skipped`-removal-diagnostic requirements explicitly.
  - §8: added the empty/absent-`depends_on` exception; narrowed
    duplicate-reference rejection from "anywhere in the tree" to "direct
    siblings of the same group," preserving legitimate cross-branch
    repetition of the same name with different `accept` sets.
  - §9: replaced the two-bullet migration approximation with an exact
    4-row translation table keyed to which of the two old flags were
    present; fixed "any downstream path" to "every downstream path" for
    the structural-catch description.
  - §2.1/§8: acknowledged the existing `unreachable` UI label ("upstream
    failed") becomes inaccurate under `accept` and needs updating.
  - §3: restored the `dwell-time` edge for `ml-impressions-master` that an
    over-shortened example had dropped; disambiguated beta/stage's
    separate prediction dependencies instead of "mirrors beta" shorthand.
  - §11: added the in-flight loop-timing nuance (a loop finishing just
    before vs. just after upgrade can show different final status).
  - §12/Appendix: marked diagnostic tracing as an explicit scope reduction
    from Codex's proposal; corrected two inherited inaccuracies about
    Tekton's `onError: continue` and GitHub Actions' `success()` display.
