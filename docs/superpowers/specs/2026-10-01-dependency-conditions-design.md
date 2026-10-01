# Dependency Conditions — typed per-edge outcome acceptance — Design

Status: draft, awaiting user review
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
`accept` set — `job_step.skip_reason` keeps recording the specific reason
for diagnostics (UI, hooks) exactly as today; the grouping is never itself
written to the database.

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
deliberate change from the gate's current (0.17.0 and v2) behaviour, where
an unaccepted outcome on one branch immediately omits the dependent even if
a sibling dependency is still running. Under this model the decision always
waits for every referenced step to finish, then evaluates the whole tree
once. Trade-off: slower propagation in the specific case where a dependent's
tree is already decided by its fastest branch (an `any` satisfied by its
first success still waits for its slower sibling) — accepted, because it
replaces the two-pass `all_deps_skipped` machinery the old gate needed
(0.16.2 onward, carried into v2) just to get "fail fast on a hard block" and
"wait for a still-running choice-skip sibling" right at the same time. One
evaluation, one rule, no phase-ordering subtlety to get wrong.

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
- `ml-impressions-beta`/`-stage`: `depends_on: [ml-prediction-beta, {step:
  dwell-time, accept: terminal}]` (required prediction, ordering-only
  dwell-time).
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

Sequential loop advancement past a failed or cancelled instance (today's R5,
`cascade.rs::phase_rollup`) is unchanged — still governed by the
placeholder's own `continue_on_failure`, which continues to serve double
duty here (don't stop promoting instances; don't fail the job for it),
exactly as today. Only the *rollup status* changes, not the advancement
behaviour.

## 5. Server cascade (`crates/stroem-server/src/cascade.rs`)

This needs real restructuring, not just a signature change in `gate.rs`:

- **P1** (cascade-skip + promote with `when`) evaluates the new entry tree
  instead of the old `Verdict`/`all_deps_skipped` split. Because tree
  evaluation already waits for every reference to go terminal before
  deciding (§2.3), **P2's** separate "skip-unreachable, the rest of the
  cases P1 couldn't yet decide" pass is no longer needed — one phase now
  does what two used to split purely for timing reasons. P2 is removed;
  its responsibility folds into P1.
- **P0** (rollup) keeps its existing shape but no longer flips a
  tolerated-failure rollup to `completed` (§4) — the `cof` check that today
  picks `RollupOutcome::Completed` vs. `RollupOutcome::Failed`
  (`cascade.rs:345`) stops branching on `continue_on_failure`; it always
  reports the true outcome. The *advancement* check (`cascade.rs:303`, stop
  promoting on a failed/cancelled instance unless tolerated) is unchanged.
- **P3** (adopt + expand placeholders) evaluates the same new tree before
  expanding, exactly as it evaluates the old `Gate` today.

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
#[serde(untagged)]
pub enum DependsOnEntry {
    Name(String),
    #[serde(deny_unknown_fields)]
    Step { step: String, accept: AcceptSet },
    #[serde(deny_unknown_fields)]
    All { all: Vec<DependsOnEntry> },
    #[serde(deny_unknown_fields)]
    Any { any: Vec<DependsOnEntry> },
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

`#[serde(deny_unknown_fields)]` on every struct variant is load-bearing, not
decorative — the exact lesson from the v2 spec's round 2 review: a typo'd
key must be a hard parse error, not silently dropped. An empty `Outcomes`
list, or an `all`/`any` with zero children, is a validation error, not a
vacuous pass (an empty `all` would trivially always be satisfied; an empty
`any` would trivially never be).

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
  named node is terminal.
- A duplicate reference to the same step within one step's dependency tree
  (the same name appearing as both a bare `step:` leaf and inside a nested
  `any:`, for instance) is rejected — generalizing v2 §2.5's flat-list
  duplicate rejection to the tree shape, for the same reason: a silent
  precedence choice between two conflicting `accept` sets for the same name
  would hide what's almost always a mistake.
- The 0.17.0-era "step will be skipped whenever its dependency is skipped,
  add `continue_when_skipped`" warning is retired along with the flag it was
  about — there's nothing left for it to warn about, since the consumer now
  states its own tolerance directly in `accept` rather than relying on an
  upstream broadcast it could forget to add.

## 9. Migration (0.17.0 → this design)

v2 never shipped — it's superseded before sign-off — so there is one
migration path to document: 0.17.0 (released, likely not yet deployed to
production per the v2 spec's rollout note) to this design.

- Every `continue_on_failure`-broadcasts-to-dependents case (0.17.0's actual
  shipped behaviour) becomes an explicit `accept: [completed, failed]` (or
  `terminal`, if a choice-skip should pass too) on each dependent's edge.
  The "tolerate failure, block choice-skip" case v2's own migration guidance
  had no answer for is now simply `accept: [completed, failed]` with
  `skipped`/`omitted` left out of the set — solved, not worked around.
- Every `continue_when_skipped` producer-side flag is removed; each
  dependent that relied on it gains `accept: [completed, skipped]` (or a
  broader set, as needed) instead.
- The loop-rollup behaviour change (§4) needs its own checklist item: any
  workflow whose dependents currently rely on a tolerated loop failure
  rolling up as `completed` needs those dependents' edges updated to
  `accept: [completed, failed]`.
- The multi-hop/leaf-catch structural audit v2's §9 specified still
  applies unchanged: walk flow *definitions* (not job history) with the old
  `caught_steps()` logic to find every failure 0.17.0 structurally catches
  several hops downstream — that recursive "catch via any downstream path"
  mechanism never had a replacement in v2, and still doesn't here. The
  origin of a failure still needs its own `continue_on_failure` if the job
  must keep completing when it fails; no amount of `accept` tuning on
  downstream edges substitutes for that, because `accept` only ever answers
  "do I run," never "does the job still count as succeeded."
- `depends_on: [a, a]`-style duplicates (valid, if pointless, YAML today)
  must be deduplicated before upgrading, same as v2's §9 already noted.

## 10. Tests

- `gate.rs` (or its successor module): tree-evaluation unit tests for every
  outcome against every `accept` combination, including `terminal`; `all`/
  `any` nesting (including an `any` containing an `all`); the uniform
  readiness barrier (a satisfied `any` still waits for a `Pending` sibling);
  duplicate-reference rejection; `deny_unknown_fields` rejection for a
  typo'd key, for both an inline flow step and a step pulled from a
  referenced library action.
- `cascade.rs`: P1/P2 merge — a test that a step previously split across
  old-P1/old-P2 timing now resolves in one pass with the same final
  outcome; the `recalc-pipeline` worked example (§3) as an integration-style
  test driving all seven rules; a loop-rollup test asserting a tolerated
  instance failure now rolls up `failed` (not `completed`) and that a
  dependent with `accept: [completed, failed]` on that placeholder still
  runs while one without it is `omitted`.
- `restart.rs` / CLI local runner: outcome-classification tests using the
  direct self-flag rule, per §7's note that these aren't just shape
  adapters.
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

## 12. Non-goals

- Per-reference *diagnostic* detail on *why* a tree was unsatisfied (which
  specific branch) beyond today's single `skip_reason` on the omitted row.
  Revisit if real operator feedback says "omitted" alone isn't enough to
  debug a complex `any`/`all` tree.
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

## Appendix: why not Argo's full expression strings, or Airflow's `trigger_rule`

| Model | Strength for Strøm | Weakness for Strøm |
|---|---|---|
| Argo-style boolean expression strings (`"A.Succeeded && (B.Succeeded \|\| C.Succeeded)"`) | Maximally expressive in one line | A compact string hides broad allowances at a glance; needs a parser/validator for a mini-language; harder to show structurally in the UI's DAG view than a typed tree |
| Airflow-style `trigger_rule` enum (`all_success`, `none_failed`, …) | Excellent concise names for common joins | One rule per *task*, blanket across all its upstreams — cannot express `recalc-pipeline`'s actual need (one required dependency, one ordering-only dependency, on the *same* step) |
| **This spec: typed `{step, accept}` + `all`/`any` tree** | Same expressiveness as Argo for the cases that matter, but structured (validatable field-by-field, inspectable by tooling/UI) rather than a string to parse; keeps the common case (`depends_on: [a, b]`) exactly as simple as today | More YAML than Argo's one-liner for a complex condition; still real Boolean complexity in the `all`/`any` case, the tree doesn't make that free |

Full systems table (Tekton, GitHub Actions, GitLab, AWS Step Functions,
Dagster) carried over unchanged from `2026-09-30-dependency-gate-v2-design.md`'s
Appendix A — nothing in that comparison changes with this model; it was
Argo and Airflow specifically that this spec's shape was chosen against.

## Review Log

- 2026-10-01: Codex's unconstrained design critique (same thread as the v2
  spec's reviews, `01a0f180-78b2-7642-b836-62a928629273`) proposed this
  model in place of v2's per-edge binary `optional` marker, specifically to
  close the "no v2 equivalent" gap v2's own round 2 review found in its
  migration guidance (v2 §9, "choice-skip migration remedy incorrect"). User
  chose to adopt it over patching v2 further. User declined to rename
  `continue_on_failure` for now, open to revisiting the name later.
