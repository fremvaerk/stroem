# Dependency Conditions — typed per-edge outcome acceptance — Design

Status: revision 3, Codex round 2 findings addressed; awaiting further review
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

**No new stored value, but a real change in what gets written.** `omitted`
reuses the *existing* `SkipReason::Unreachable` value — no new enum variant,
no DB migration. It is **not** accurate to say this is written "exactly as
today," though: going forward, `unreachable` is written for *every* omitted
row, including ones that today would be written as `cascade` (a choice-class
block, origin folded away per §2.1's unification). `cascade` is never
written by new code after this ships; it only ever appears on rows written
before the upgrade. §9's migration guidance depends on this distinction and
states it precisely.

**The existing UI label for `unreachable` ("upstream failed") stops being
accurate and needs updating**, for two independent reasons: first, because
`cascade` rows (a choice-class block, not a failure) now read as
`unreachable` going forward too; second, because under `accept`, a step can
be omitted for reasons that have nothing to do with an upstream *failing*
at all — e.g. `{step: x, accept: [failed]}` (deliberately accepting only a
failure) is unsatisfied, and the dependent is omitted, precisely when `x`
*succeeds*. Replace "upstream failed" with outcome-neutral wording such as
"a dependency condition was not satisfied" wherever the UI currently
renders that label from a `skip_reason` of `unreachable`.

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
cases need their own test to pin this precisely. First, a tree already
conclusively unsatisfiable by one child, with a second child still pending
— today's `BlockFail` (the only one of the two old cases with no uniform
pending-wait) would omit immediately; this model waits, which also delays
any downstream cleanup step that was only waiting to react to the eventual
`omitted`. Second, an `any` group already satisfied by one child, with a
second child still pending — note there is no literal "today's behaviour"
to contrast here, since `any` grouping doesn't exist before this spec; what
needs pinning is that the *new* construct doesn't accidentally reintroduce
a short-circuit of its own (an `any` satisfied by its first child must
still wait for its other children per this section's barrier, not resolve
early just because it's already logically decided) — §12 already lists
this as an explicit non-goal (no short-circuiting), this test is what pins
it in code.

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

**This requires changes at three separate layers, not just "build the
array" — verified directly against the code, not assumed:**

1. **`cascade.rs`'s in-memory `RollupOutcome` type.** Today,
   `RollupOutcome::Completed(Value)` carries the aggregated output array;
   `RollupOutcome::Failed(String)` carries only an error message
   (`cascade.rs:40-43`) — there is nowhere for an output value to go on the
   failure branch. `Failed` needs a second field, e.g.
   `Failed(String, Value)`, and the rollup-building code (`cascade.rs:345`)
   needs to construct the output array unconditionally, before branching on
   `any_failed`, using the existing documented rule unchanged (CLAUDE.md §
   For-Each Loops: "one element per existing instance, `null` where the
   instance produced none").
2. **Persistence (`JobStepRepo::fail_placeholder_tx`,
   `crates/stroem-db/src/repos/job_step.rs:464`).** Checked directly: this
   function's `UPDATE` sets `status`, `error_message`, `completed_at` — it
   never touches the `output` column at all, unlike
   `complete_placeholder_tx` (`job_step.rs:441`), which does. It needs a new
   `output: &JsonValue` parameter and the corresponding `output = $4` in
   the `UPDATE`, and `cascade.rs`'s call site (`cascade.rs:798-801`) needs
   to pass the array from fix 1 through.
3. **Render-context exposure (`render_context.rs:271-293`).** Checked
   directly: the `failed` branch unconditionally sets
   `entry.insert("output", Value::Null)` (`render_context.rs:286-287`) —
   it doesn't even read `s.output`, unlike the `completed` branch just
   above it, which does. The simplest correct fix is to merge the two
   branches so a failed row's `output` is read from the column exactly
   like a completed row's (falling back to `Value::Null` only if the
   column is actually empty) — safe for an *ordinary* failed step, whose
   `output` column was never populated on failure either before or after
   this change, so nothing observable changes for that case; it only
   starts exposing real data for the one case fix 1/2 newly populate, a
   tolerated loop rollup. The CLI's parallel implementation
   (`run.rs:117`'s `record_failure` and the rollup builder at `run.rs:333`)
   needs the identical three-layer fix, independently — it's a second,
   separate implementation of the same rollup logic, not a wrapper around
   `cascade.rs`'s.

**Correcting an inaccurate claim from the previous revision**: a `Failed`
placeholder did *not* previously block every dependent — a placeholder
with `continue_on_failure: true` already passed the shipped 0.17.0/v2 gate
for an *optional*/accepting dependent; the actual previous limitation was
narrower and specific: such a dependent could run, but could never see any
real data, because the array was never built for that branch (fix 1) even
when the gate let execution through.

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

Both revision 1 and revision 2 of this section were wrong about the actual
cascade architecture, and the only way to get this right was to read
`run()` directly (`cascade.rs:596-649`) and the project's own existing
pinned tests (`cascade.rs:3080-3160`, comment: "pass timing (spec §5, Codex
rounds 1-3)" — a pre-existing section from the *original* 0.17.0
dependency-gate work, not this spec; the comment's phrasing is a coincidence
of this project's recurring process vocabulary). Both corrections below are
grounded directly in that code and those tests, not re-derived from memory.

**Correction 1: `execute()`/`run()` already has an outer fixpoint loop.**
`run()` repeats P0→P1→P2→P3 — rebuilding `ctx_a` and `ctx_b` fresh from the
current snapshot on every repetition — until one full pass produces zero
`Change`s (`cascade.rs:613-649`, the `loop { ...; if pass.is_empty() { break
} }` body). Revision 1's claim that `execute()` runs each phase exactly once
was simply wrong; revision 2's attempted fix (an unscoped "P1 runs to a
fixpoint") didn't account for this *existing* outer loop at all. Any
redesign here has to say precisely how a new inner mechanism relates to
this existing outer one, not pretend it doesn't exist.

**Correction 2: P2 is not redundant with the outer loop — it's what makes
one specific case resolve one pass sooner than the outer loop alone would,
and a naive merge of P1+P2 loses exactly that, reproducing the real
regression Codex found.** Walking the existing pinned tests against the
*current* code explains why, precisely:

- `timing_unreachable_chain_keeps_legacy_pass`: `x` starts already
  `skipped`/`unreachable`. P1's guard is specifically `all_deps_skipped`
  (every dependency has row *status* `'skipped'`, regardless of reason) —
  true for `a` here, so **P1 decides `a` right away**. `apply_all` runs
  before P2 executes, so **P2 — which has no such guard, it unconditionally
  checks every still-pending row's gate — now sees `a` already terminal and
  decides `b` in the *same pass*.** `ctx_b` (built after P2) already
  reflects both decisions, so the placeholder `p`'s `when` ("is `b`
  defined") sees it and `p` expands, all within pass 1.
- `timing_failed_root_keeps_legacy_pass`: `x` starts `failed` (not
  `skipped`). `all_deps_skipped(a)` is **false** (`status` is `'failed'`,
  not `'skipped'` — the guard checks status literally), so **P1 defers on
  `a`**; P2's unconditional check still decides it, so `a` *is* decided in
  pass 1 — but only by P2, one phase later than the other test, with
  nothing left afterward in *that* phase to give `b` the same within-pass
  relay P2 gave `a`. `b` stays pending through all of pass 1; `ctx_b` built
  at the end of pass 1 doesn't yet reflect `b`; `p` (a placeholder, decided
  in P3) **permanently** condition-skips on that stale read, because P3 only
  ever revisits rows that are still `pending`, and `p` no longer is. The
  outer loop *does* go on to decide `b` in pass 2 — one pass too late for
  `p`, which already committed. The test's name (`keeps_legacy_pass`)
  records that this was a deliberately preserved 0.16-era quirk when the
  dependency gate shipped, not an accident.
- `timing_accepted_change_flagged_placeholder_retires_immediately`: `l`
  depends on `[x (already unreachable), y (still running)]`. Today's
  `gate()` returns a hard `BlockFail` on the **first** unsatisfied
  dependency found, full stop, regardless of `y` still being pending — `l`
  retires immediately, without waiting for `y`. This is precisely the
  fail-fast dominance §2.3 deliberately replaces with a uniform
  wait-for-everything barrier.

**A single merged P1+P2 phase (revision 1's approach), run once per outer
pass, reproduces only the *first* test's happy path — a merged phase with
no internal relay is strictly weaker than today's P1-then-P2 one-hop relay,
because it collects decisions for `a` and `b` into one batch without `b`
ever seeing `a`'s mid-batch result. That's the regression Codex found: it
downgrades the `unreachable`-chain case to the `failed`-chain case's
(already worse) timing.**

**The fix: give the merged phase (replacing P1+P2, call it P1) its own
*internal* relay loop — re-scan pending rows against the snapshot, apply
every decision the tree evaluation can now make, and repeat — nested
*inside* one iteration of the existing outer loop, run to its own
stability before that outer iteration proceeds to P3:**

- Internally, this subsumes and strictly exceeds what the old P1-then-P2
  one-hop relay provided: it keeps relaying for as many hops as a chain
  actually has, not just one, and it does so for a `failed`-rooted chain
  exactly as readily as a `skipped`-rooted one (today's asymmetry — one
  case resolves in pass 1, the other needs pass 2 — came specifically from
  P1's `all_deps_skipped` guard checking row *status*, which this spec's
  uniform barrier (§2.3) has no equivalent special case for; every blocking
  outcome is just "terminal and not accepted," skip or fail alike).
- *Termination*: the pending set only shrinks (a decided row never
  re-enters pending) and is finite, so the inner relay terminates in at
  most `N` iterations for `N` pending rows in that outer pass, plus one
  final no-change scan; it does not depend on acyclicity, only on
  monotonic shrinking.
- P0 and P3 are unchanged in shape. P3 still runs once per *outer* pass
  (unchanged from today), after the merged P1 has internally stabilized —
  so `ctx_b` reflects everything decidable within that outer pass, not a
  partial result.
- **Scope note, since an earlier draft of this test over-reached**: this
  specifically fixes placeholder timing (P3 only revisits still-`pending`
  rows, so a placeholder that commits early on a stale read can never
  self-correct — the exact mechanism above). An *ordinary* (non-placeholder)
  step's `when` is evaluated inline, within the same merged-P1 relay, the
  moment its own tree is satisfied — if that `when` references some other
  step's output *without* that step being in its own `depends_on` tree
  (unsupported today, and not addressed by this spec either way), it can
  still observe whatever the relay's current iteration happens to show. The
  regression test (and the existing pinned tests above) use a placeholder
  for `p`/`l`, matching every one of today's existing pinned tests — this
  spec does the same, deliberately, rather than generalizing to ordinary
  steps.

**This is a real, net-positive timing change beyond fixing one broken test,
and §12 needs to say so plainly rather than deny it.** Two existing pinned
tests need their expected outcomes updated, not just the counter-example
this section started from:

- `timing_failed_root_keeps_legacy_pass` — `p` now **expands** within pass
  1 instead of condition-skipping, because the failed-rooted chain now gets
  the same within-pass relay the skipped-rooted chain already got. The test
  needs renaming (its current name specifically celebrates *preserving* the
  quirk this spec removes) and its assertion flipped.
- `timing_accepted_change_flagged_placeholder_retires_immediately` — `l` now
  **waits for `y`** before retiring, instead of retiring immediately on `x`
  alone — the direct, already-acknowledged (§2.3) consequence of replacing
  fail-fast `BlockFail` dominance with a uniform barrier. Needs the same
  treatment: rename and flip.
- `timing_unreachable_chain_keeps_legacy_pass` and
  `timing_accepted_change_dependent_cof_no_longer_delays` are unaffected —
  both already resolve within one pass today, and the merged relay still
  does so (walked through §2.5's verdict table, no change to their
  outcome). `timing_accepted_change_unknown_reason_is_failure_class` is
  also unaffected — an unrecognised skip reason still has row `status`
  `'skipped'`, so it was already a same-pass case, and under §2.1's
  classification it is `omitted`, same conservative bucket as `unreachable`
  was.

P0's rollup logic itself is unaffected by this section — see §4 for its own,
separate fix (the output-array and `Completed`/`Failed` branching).

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

/// Deserializes only from the exact string "terminal"; any other string
/// value is a parse error, not a silently-accepted typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalKeyword;
// (Deserialize impl: match the input string against "terminal" literally;
// omitted here for brevity, not because it's optional — the literal-match
// requirement is the point.)
```

(This sketch omits the crate's normal `use` imports for brevity; nothing
about them is load-bearing to the design.)

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
An empty `Outcomes` list, or a *nested* `all`/`any` with zero children, is a
validation error, not a vacuous pass (an empty `all` would trivially always
be satisfied; an empty `any` would trivially never be) — except at the top
level: a step with no dependencies at all, written either as an absent
`depends_on` field or as an explicit `depends_on: []`, is unchanged from
today and satisfied immediately (§8 states this same exception precisely,
for both spellings).

**Parse-error propagation is unconditional, for both code paths that read a
flow step.** `workflow.rs:578`'s existing pattern — falling back to an empty
`depends_on` list on a deserialization error — must be removed for inline
flow steps *and* for steps pulled in from a referenced library action; this
was already v2's requirement and is carried forward unchanged, stated here
explicitly so it isn't lost in translation again. Separately, the
permissive reference-step struct that currently still accepts
`continue_when_skipped` (`workflow.rs:458`) must not simply stop
recognizing that field once it's retired (§2) — a workspace config that
still sets it needs a named, actionable validation error, not silent
acceptance-and-ignore. The error message must not prescribe a single fixed
`accept` set — §9 shows the two single-flag migration cases are
context-dependent, not a fixed substitution — so it should point at the
migration guide (§9) rather than assert a specific replacement, e.g. "`
continue_when_skipped` was removed in 0.18.0; see the 0.18 upgrade guide to
choose the right `accept` set for this dependent, it depends on what else
was present on this dependency." Tests for both of these must exercise
actual flow-step loading (the full parse path), not just the
`DependsOnEntry` type in isolation.

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
  only the per-branch `accept` evaluation differs. Implementation caution:
  this check must run against the tree exactly as authored, before any
  normalization step (e.g. flattening nested `all`s into their parent) —
  flattening first could turn a legitimate cross-branch reference into a
  same-level sibling and wrongly reject it.
- The 0.17.0-era "step will be skipped whenever its dependency is skipped,
  add `continue_when_skipped`" warning is retired along with the flag it was
  about — there's nothing left for it to warn about, since the consumer now
  states its own tolerance directly in `accept` rather than relying on an
  upstream broadcast it could forget to add.

## 9. Migration (0.17.0 → this design)

v2 never shipped — it's superseded before sign-off — so there is one
migration path to document: 0.17.0 (released, likely not yet deployed to
production per the v2 spec's rollout note) to this design.

**Revision 2's table claimed this translation is exact. It is not, and
cannot be, for the two single-flag rows — not a bug to fix, but a direct,
unavoidable consequence of §2.1's own design choice, which the migration
guidance needs to state honestly rather than paper over.**

Here is precisely why. The shipped 0.17.0 gate keys its two flags off *skip
reason*, not off *why* that reason arose: `continue_when_skipped` tolerates
a dependency being `Skipped` with reason `condition`, `empty`, *or*
`cascade`; `continue_on_failure` tolerates `Failed`/`Cancelled`/`Skipped`
with reason `unreachable` or unknown (`gate.rs:55`). §2.1 of *this* spec
deliberately does not preserve that split — per Codex's own original
recommendation, adopted in §2 — it collapses `cascade` and `unreachable`
into one unified `omitted` outcome, because once a dependency is itself
blocked by *its own* tree, nothing further downstream can (or, by design,
should be able to) tell whether the ultimate cause several hops back was a
choice or a failure. That is the entire point of unifying `Omitted` — and
it means the two single-flag migration cases genuinely have no exact
equivalent:

| Dependency's 0.17.0 flags | Closest new `accept` | What changes, precisely |
|---|---|---|
| neither | `[completed]` (unchanged default) | Exact. |
| both | `terminal` | Exact (both old flags together already tolerated everything but `Pending`). |
| `continue_when_skipped` only | `[completed, skipped]` | **Narrows**: today this also tolerates the dependency being cascade-skipped (an upstream choice-block reaching it); `omitted` isn't in this set, so that case is now newly rejected. |
| `continue_on_failure` only | `[completed, failed, cancelled, omitted]` (`terminal` minus `skipped`) | **Widens**: `omitted` here also covers the dependency being cascade-skipped, which the old flag never tolerated — only `unreachable`/unknown did. |

**Worked example, corrected** (revision 2's version of this example was
wrong under the spec's own §2.1): `A(when=false) → B(continue_when_skipped=
true) → C`. Today, `B` is cascade-skipped and `C` runs. Under this spec,
`B`'s own resulting outcome is `omitted` (§2.1 — `B` was blocked by *its
own* tree, specifically by `A`'s choice-skip; this is exactly the kind of
case §2.1 unifies, not `skipped`, which is reserved for a step's own direct
`when`/empty-loop choice). Migrating via the table's row-3 recommendation,
`accept: [completed, skipped]` on `C`'s edge to `B`, therefore does **not**
reproduce today's behaviour — `omitted` isn't in that set, so `C` would now
be omitted too, where it used to run. Reproducing today's behaviour here
needs `accept: [completed, omitted]` (or `terminal`) instead — the opposite
of what a literal reading of "migrate `continue_when_skipped` to `accept:
[completed, skipped]`" would suggest. **This is precisely why the table
above is framed as "closest approximation," not "exact translation," and
why a migrator must check which specific case they're in.**

When the distinction genuinely matters (a workflow really does need
"tolerate a failure several hops back, but not a choice-skip several hops
back," or vice versa, through an intermediate dependency), there is no
single-edge-on-the-intermediate-dependency way to express it, because the
intermediate dependency's own `omitted` outcome has already thrown that
distinction away. The only faithful option is to stop relying on the
intermediate hop's broadcast at all and name the actual origin directly:
give the edge several hops up an explicit `accept` for its own direct
outcome (`failed`/`cancelled` vs. `skipped`), and have the intermediate
steps use `accept: terminal` purely for ordering. This is more verbose, but
it's the one way to recover precision the unified `omitted` outcome
deliberately gave up.

Migration tests (§10) must cover both directions concretely: a workflow
whose historical rows include a `cascade` skip reason, and one whose
historical rows include `unreachable` — not just the happy "neither/both"
cases, which are the only ones that translate exactly.
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
- `cascade.rs`: update the five existing pinned timing tests
  (`cascade.rs:3080-3160`) for the new merged-phase relay (§5) —
  `timing_unreachable_chain_keeps_legacy_pass` and
  `timing_accepted_change_dependent_cof_no_longer_delays` keep their current
  assertions (unaffected); `timing_accepted_change_unknown_reason_is_failure_class`
  keeps its assertion too; `timing_failed_root_keeps_legacy_pass` and
  `timing_accepted_change_flagged_placeholder_retires_immediately` both need
  renaming and their assertions flipped (§5 explains exactly why for each).
  Do not just add a new test alongside these — update them in place, since
  they're what pins the actual behavior change; the `recalc-pipeline` worked
  example (§3) as an integration-style test driving all seven rules; a
  loop-rollup test
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
the right starting point. To state precisely what "re-evaluated" covers: a
row already past the gate — `ready`, `claimed`, `running`, or terminal — is
never re-gated by the new code; only rows still `pending` when the upgrade
takes effect are evaluated under the new tree/`accept` model on their next
cascade. A step that already started executing under 0.17.0's rules keeps
running to its own completion under those rules; only its *dependents*,
still pending, see the new gate.

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
- Changing how an individual `when` expression is *written* or evaluated —
  still the same Tera condition, still evaluated after the dependency tree
  is satisfied (§2.3). **Not a non-goal, and withdrawn as a claim**: the
  *timing* of when a placeholder's `when` gets evaluated relative to a
  multi-hop dependency chain resolving does change, deliberately — §5
  documents it precisely, including the two existing pinned tests whose
  expected outcome flips as a direct, intentional consequence.
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
- 2026-10-01, Codex round 2 (same thread, verdict Still SHIP WITH FIXES):
  6 of 15 round-1 findings confirmed fully resolved; 3 required real
  rework, addressed in revision 3 by reading the actual code and existing
  tests directly rather than re-deriving from memory a third time.
  - **§9's migration table was still wrong, called the "core unresolved
    issue"**: §2.1's unification of `cascade`/`unreachable` into one
    `omitted` outcome (a deliberate design choice, kept) means the two
    single-flag migration cases have no exact equivalent — one narrows,
    one widens. Rewrote §9 to state this honestly with a corrected worked
    example (the original was wrong under the spec's own §2.1), instead of
    claiming an exactness that doesn't hold; added guidance for when the
    distinction actually matters (name the origin directly, several hops
    up, rather than relying on an intermediate hop's broadcast).
  - **§5 was substantively wrong, twice**: revision 1 assumed `execute()`
    ran each phase once (it doesn't — `run()` already loops P0-P3 to a
    fixpoint, `cascade.rs:596-649`); revision 2's "P1 runs to a fixpoint"
    fix didn't account for that existing outer loop, and its regression
    test over-generalized from a placeholder to an ordinary step (where it
    doesn't apply). Rewrote §5 from scratch, grounded directly in the
    actual code and the project's own pre-existing pinned timing tests
    (`cascade.rs:3080-3160`), which turned up exactly why P2 isn't
    redundant with the outer loop (it's what lets a same-pass relay reach
    one hop further than a bare merged-P1 would) and exactly which two of
    those five existing tests need their assertions flipped, not just a
    new test added. §12's "no other timing changes" claim is withdrawn and
    replaced with an explicit, honest statement of what changes and why.
  - **§4's loop-output fix was incomplete**: "build the array" doesn't
    reach a template. Traced the full chain in the actual code — `cascade.rs:40`'s
    `RollupOutcome::Failed` carries no output value at all,
    `job_step.rs:464`'s `fail_placeholder_tx` never writes the `output`
    column, and `render_context.rs:286` unconditionally nulls it for any
    failed row regardless — and specified the fix at all three layers,
    plus removed the incorrect "blocked every dependent" claim.
  - Smaller fixes: §2.1's "exactly as today" overclaim corrected (going
    forward, `cascade` is never written, only read on legacy rows); §2.3's
    imprecise "today's equivalent" sentence for the `any` case corrected
    (no such equivalent exists, since grouping is new); §7 given its
    omitted `TerminalKeyword` definition; §7's empty-`depends_on`
    exception now explicitly covers both the absent-field and `[]`
    spellings, matching §8; §8 given an implementation caution about
    checking duplicates against the authored tree, not a flattened one;
    §11 given a precise statement of what "re-evaluated" does and doesn't
    cover for already-running work; §7's migration error message no longer
    prescribes a fixed `accept` set given §9's correction.
