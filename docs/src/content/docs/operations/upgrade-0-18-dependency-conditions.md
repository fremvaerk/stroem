---
title: Upgrading to 0.18 — dependency conditions
description: continue_when_skipped is removed; depends_on becomes a typed {step, accept} / all / any tree, and continue_on_failure is now self-scoped only
---

Release 0.18.0 replaces 0.17.0's per-dependency flags (`continue_on_failure` /
`continue_when_skipped`, read from the dependency, applied to every dependent
alike) with typed **per-edge outcome acceptance**: each `depends_on` entry
names exactly which of its dependency's terminal outcomes satisfy it, plus
`all`/`any` grouping for shapes a flat list can't express. This is a breaking
behaviour change on top of 0.17.0's own breaking change — no schema
migration is involved, but existing workflows can skip or run steps
differently than they did on 0.17.x. Read this page before upgrading if any
workflow uses `continue_on_failure` on a step that has a `depends_on`, or
uses `continue_when_skipped` at all.

If you're migrating directly from 0.16.x (never deployed 0.17.0), read the
[0.17 upgrade guide](/operations/upgrade-0-17-dependency-flags/) first — this
page assumes you're starting from 0.17.0's rules, not 0.16.x's.

## The model

Full design: `docs/superpowers/specs/2026-10-01-dependency-conditions-design.md`.
Full syntax and worked examples: the [Conditionals guide](/guides/conditionals/).
In short:

- **`continue_when_skipped` is gone.** A workspace config that still sets it
  gets a named, actionable parse error (`continue_when_skipped was removed
  in 0.18.0; see the 0.18 upgrade guide...`) instead of silently being
  ignored.
- **`continue_on_failure` is now self-scoped only.** It means this step's own
  `failed` status doesn't fail the *job*. It no longer has any effect on
  whether a dependent runs.
- **`depends_on` is now a tree.** Each entry is a bare name (sugar for
  `accept: [completed]`), a `{step, accept}` object, or an `all`/`any` group.
  `accept` is a non-empty list drawn from five outcomes —
  `completed`/`failed`/`cancelled`/`skipped`/`omitted` — or the literal
  `terminal` (all five). A dependent now states directly what it tolerates
  about each dependency, instead of relying on the dependency to broadcast a
  flag to everyone downstream.
- **Readiness is uniform.** The gate always waits for every step named
  anywhere in a `depends_on` tree to go terminal before deciding anything —
  including a tree that's already conclusively unsatisfiable by one child
  with a sibling still pending, and an `any` group already satisfied by its
  first child. Neither case short-circuits.

## Migration table

0.17.0's two flags keyed off a dependency's **skip reason**, not off why that
reason arose: `continue_when_skipped` tolerated the dependency being
`skipped` with reason `condition`, `empty`, *or* `cascade`;
`continue_on_failure` tolerated `failed`/`cancelled`/`skipped` with reason
`unreachable` or unknown. 0.18.0 collapses `cascade` and `unreachable` into
one unified `omitted` outcome — deliberately, because once a dependency is
itself blocked by its own tree, nothing further downstream can (or should)
tell whether the ultimate cause several hops back was a choice or a failure.
That collapse means two of the four migration cases are **not** exact
translations — the table below says so honestly rather than papering over it:

| Dependency's 0.17.0 flags | Closest new `accept` | What changes, precisely |
|---|---|---|
| neither | `[completed]` (unchanged default) | Exact. |
| both | `terminal` | Exact (both old flags together already tolerated everything but `pending`). |
| `continue_when_skipped` only | `[completed, skipped]` | **Narrows**: today this also tolerates the dependency being cascade-skipped (an upstream choice-block reaching it); `omitted` isn't in this set, so that case is now newly rejected. |
| `continue_on_failure` only | `[completed, failed, cancelled, omitted]` (`terminal` minus `skipped`) | **Widens**: `omitted` here also covers the dependency being cascade-skipped, which the old flag never tolerated — only `unreachable`/unknown did. |

**The "neither"/"both" rows are exact. The two single-flag rows are
approximations, and you need to know which case you're actually in before
trusting them.**

### Worked example: where the single-flag rows go wrong

```yaml
flow:
  a:
    action: maybe-run
    when: "{{ false }}"            # always skipped `condition`

  b:
    action: middle-step
    depends_on: [a]
    continue_when_skipped: true    # 0.17.0 flag, on the dependency

  c:
    action: final-step
    depends_on: [b]                # no flag at all
```

Under 0.17.0: `a` is skipped `condition`. `b`'s own `continue_when_skipped`
tolerates that, so `b` is cascade-skipped rather than blocked — and `c`, even
with no flag of its own, runs, because `b`'s skip is choice-class and `b`
itself carries the flag that lets dependents through. **`c` runs.**

Migrating literally via the table's row 3 (`continue_when_skipped only →
accept: [completed, skipped]`), applied to `c`'s edge to `b`, does **not**
reproduce this:

- `b`'s own resulting outcome is **not** `skipped` — it's `omitted`. `b` was
  blocked by *its own* tree (specifically, by `a`'s choice-skip), and §2.1's
  whole point is that this is indistinguishable, from `c`'s vantage point,
  from `b` being blocked by a failure several hops back. `skipped` is
  reserved for a step's own direct `when`/empty-loop choice — `b` never made
  that choice itself, `a` did.
- So `accept: [completed, skipped]` on `c`'s edge to `b` doesn't include
  `omitted` — `c` is now **omitted too**, where it used to run.
- Reproducing 0.17.0's behaviour here needs `accept: [completed, omitted]`
  (or `terminal`) instead — the *opposite* of what a literal reading of
  "migrate `continue_when_skipped` to `accept: [completed, skipped]`" would
  suggest.

This is exactly why the table above is framed as "closest approximation,"
not "exact translation." **Check which case you're actually in** —
specifically, whether the chain between the flagged dependency and the step
you're updating has any intermediate hops — before applying a row
mechanically.

### When the distinction actually matters

If a workflow genuinely needs "tolerate a failure several hops back, but not
a choice-skip several hops back" (or vice versa) through an intermediate
dependency, there is no single-edge-on-the-intermediate-dependency way to
express it — the intermediate's own `omitted` outcome has already thrown
that distinction away. The only faithful fix is to stop relying on the
intermediate hop's broadcast and name the actual origin directly: give the
edge **several hops up** an explicit `accept` for its own direct outcome
(`failed`/`cancelled` vs. `skipped`), and have the intermediate steps use
`accept: terminal` purely for ordering.

## Structural-catch audit

0.17.0 had a recursive "caught somewhere downstream" rule: a failure at step
`f` didn't fail the job if **every** downstream path from `f` eventually
passed through a step with `continue_on_failure` (`f` itself counted). That
mechanism has no replacement in 0.18.0 — `continue_on_failure` is now a
direct, self-scoped flag check only (`f` catches itself or it isn't caught
at all; see the [Conditionals guide](/guides/conditionals/#accounting-vs-readiness)).

**If your workflows relied on a downstream step's flag to catch an upstream
failure — not the failing step's own flag — the job's pass/fail outcome
changes.** You need to audit for this *before* upgrading, and the audit has
to walk **flow definitions**, not job history: a job that happened to
complete under 0.17.0's rule tells you nothing about whether the *next* run
of the same shape would, since the rule depended on the flow graph, not on
which rows actually ran.

Audit methodology — reproduce 0.17.0's `caught_steps()` logic by hand (or
script it) against each task's flow:

1. For every step `f`, find every step that `f`'s outcome can reach via
   `depends_on` edges (ignore `when`/`for_each` — this is a pure graph walk
   over the dependency edges).
2. `f` was "caught" under 0.17.0 iff `f` itself has `continue_on_failure`, OR
   every one of `f`'s downstream paths to the end of the flow passes through
   *some* step with `continue_on_failure` — **every** path needs a catcher,
   not merely one. Getting this backwards understates how many workflows are
   actually relying on it.
3. For every `f` that was caught **only** by a downstream step's flag (not
   its own), decide: does the job still need to end `completed` when `f`
   fails? If yes, move `continue_on_failure` onto `f` itself. If the
   downstream step merely needs to *run* after `f` fails (without changing
   the job's outcome), that's a readiness question instead — give that
   step's edge to `f` an `accept` that includes `failed` (or `terminal`),
   and leave `continue_on_failure` off both steps.

No amount of `accept` tuning on downstream edges substitutes for step 3's
first branch — `accept` only ever answers "do I run," never "does the job
still count as succeeded." The origin step still needs its own
`continue_on_failure` for that.

## Checklist

- [ ] Run `stroem validate` (or load the workspace against a 0.18+ server) —
  any lingering `continue_when_skipped` is now a hard parse error naming the
  task and step.
- [ ] For every dependency that had `continue_when_skipped` or
  `continue_on_failure` under 0.17.0, find its dependents and give each edge
  the right `accept` set from the [migration table](#migration-table) above
  — checking the single-flag rows against the [worked example](#worked-example-where-the-single-flag-rows-go-wrong)
  if there's more than one hop between the flagged step and the dependent.
- [ ] Run the [structural-catch audit](#structural-catch-audit) over every
  task's flow. Any step caught only by a *downstream* `continue_on_failure`
  needs that flag moved onto the failing step itself if the job must still
  end `completed`.
- [ ] **Loop rollup**: if a `for_each` placeholder has `continue_on_failure`
  and at least one dependent relies on that placeholder rolling up
  `completed` despite a tolerated instance failure — 0.17.0 and earlier hid
  the failure behind a `completed` status; 0.18.0 always reports the true
  rollup status (`failed`, if any instance failed), and the output array is
  now built and visible on that `failed` rollup too. Update every such
  dependent's edge to the placeholder to `accept: [completed, failed]` (or
  `terminal`).
- [ ] Deduplicate any `depends_on: [a, a]`-style duplicate entries — still
  valid YAML, but redundant, and the validator now rejects a duplicate name
  among the **direct siblings** of the same `all`/`any` node (the same name
  recurring across *different* branches with different `accept` sets is
  fine and unaffected).
- [ ] If your dashboards or alerting read the `unreachable` skip-reason by its
  UI label: the badge changes from "upstream failed" to the outcome-neutral
  "not satisfied" (and the step-detail explanation drops the
  `continue_on_failure` wording) in 0.18.0, because a step can now be
  `omitted` for reasons that have nothing to do with an upstream step
  *failing* — e.g. `{step: x, accept: [failed]}` is unsatisfied, and the
  dependent omitted, precisely when `x` *succeeds*. This label change is
  **retroactive**: it applies to every `unreachable` row the UI renders,
  including rows on jobs that ran well before the upgrade, not just newly
  skipped steps — expect old jobs in your job list to show the new wording
  too once you're running 0.18.0. The `cascade` badge ("upstream skipped")
  is unchanged, but its explanation text is now framed as describing
  pre-0.18 rows specifically, since `cascade` is never written by new code.
- [ ] Existing jobs in flight at upgrade time: a row already past the gate
  (`ready`, `claimed`, `running`, or terminal) is never re-gated. Only rows
  still `pending` are evaluated under the new tree/`accept` model on their
  next cascade. A `for_each` placeholder that had *already* rolled up before
  the upgrade (any status) keeps that historical status frozen as-is; a
  placeholder still `running` at upgrade time rolls up under the *new* rule
  the first time it finishes afterward — two otherwise-identical loops can
  show different final statuses purely because of which side of the
  deployment they happened to finish on.

See also: [Conditionals](/guides/conditionals/) for the full `accept`/`all`/`any`
syntax and worked examples, and the [0.17 upgrade guide](/operations/upgrade-0-17-dependency-flags/)
if you're migrating from 0.16.x and haven't already applied that one.
