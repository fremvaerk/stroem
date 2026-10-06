---
title: Conditional Flow Steps
description: Using when conditions and depends_on's accept sets to control step execution
---

Steps can be conditionally executed based on dynamic expressions evaluated at runtime, and a step's `depends_on` entries control exactly which of its dependencies' outcomes let it run.

## Overview

Two independent mechanisms work together:

- **`when`**: a Tera template expression on a step itself, evaluated once its dependency tree is satisfied. Truthy → runs; falsy → skipped (reason `condition`); empty `for_each` → skipped (reason `empty`); template error → the step **fails**.
- **`depends_on`**: a tree of dependencies this step waits on. Each entry names which of its dependency's terminal **outcomes** — `completed`, `failed`, `cancelled`, `skipped`, or `omitted` — satisfy that edge. A step runs only when its whole `depends_on` tree is satisfied; `when` is then evaluated on top of that, never instead of it.

A bare step name (`depends_on: [a]`) is sugar for "must complete" (`accept: [completed]`) — today's default, unchanged for any workflow that doesn't need anything else. To tolerate something other than a clean completion, write the dependency as an object with an explicit `accept` set:

```yaml
depends_on:
  - a                                  # sugar for {step: a, accept: [completed]}
  - step: b
    accept: [completed, failed]        # tolerates b failing, not being skipped
  - step: c
    accept: terminal                   # ordering only — don't care how c ends
```

**Readiness waits for everything named, every time.** Whether a dependency is required or tolerated, the step is only decided once *every* step named anywhere in its `depends_on` tree has gone terminal — there's no fail-fast on an already-unsatisfiable tree, and no shortcut for an `any` group already satisfied by its first child. See [Validation](#validation) and the [YAML reference](/reference/workflow-yaml/#dependencies) for the full grammar, including `all`/`any` grouping.

**Validation**: `when` syntax and `depends_on` shape are validated at YAML parse time — a typo'd key (`accpet` instead of `accept`) or a mapping with two discriminating keys at once (`step` and `any` together) is a hard error, not a silent default.

:::note[Upgrading from 0.17 or earlier]
If your workflows still use `continue_on_failure` to decide whether a *dependent* runs, or `continue_when_skipped` at all, read the [0.18 upgrade guide](/operations/upgrade-0-18-dependency-conditions/) first — the model below replaces both.
:::

## YAML Syntax

```yaml
tasks:
  conditional-task:
    input:
      run_checks: { type: boolean, default: false }
    flow:
      setup:
        action: init-workspace

      check-data:
        action: validate-input
        depends_on: [setup]
        when: "{{ input.run_checks }}"

      process:
        action: process-data
        depends_on:
          - setup
          - step: check-data
            accept: [completed, skipped]
            # Lets `process` through whether check-data ran or was
            # skipped by its own `when` — but not if it failed.
```

## Truthiness Rules

Tera templates render to strings, and Strøm decides truthiness on the **rendered text** (after trimming). The following are **falsy** and cause the step to be skipped:

| Value | Skipped? |
|-------|----------|
| Empty string `""` | Yes |
| `"false"` (case-insensitive: `"False"`, `"FALSE"`, etc.) | Yes |
| `"0"` | Yes |
| `"null"` (case-insensitive: `"Null"`, `"NULL"`, etc.) | Yes |
| `"none"` (case-insensitive: `"None"`, `"NONE"`, etc.) | Yes |
| `"[]"` and `"{}"` (an empty array or object) | Yes |
| Any number equal to zero: `"0"`, `"0.0"`, `"-0.0"` | Yes |
| `"true"`, `"1"`, any other string | No |
| Template error (e.g., undefined variable) | Fails the step |

Worked cases (with `z = 0.0`, `e = []`, `m = {}`, `s = "false"`, `n = null`, `one = 1`):

| `when:` | Result |
|---|---|
| `{{ z }}`, `{{ e }}`, `{{ m }}`, `{{ s }}`, `{{ n }}` | skipped |
| `{{ e and one }}`, `{{ one and e }}` | skipped (`and` returns the empty operand) |
| `{{ e or one }}` | runs (`or` returns `1`) |
| `{{ one }}`, `{{ [0] }}`, `0.5`, `x` | runs (`[0]` is a non-empty array) |
| empty string, `0`, `-0.0`, `None`, `NULL` | skipped |

:::note
A condition that renders an empty array (`when: "{{ scan.output.items }}"`) is
now **false**; under Tera 1 it rendered `[]` and was truthy.
:::

:::caution
Empty strings evaluate to falsy. Use `{{ input.value \| default(value='') }}` carefully — it will skip the step if undefined.
:::

## Available Template Variables

Inside a `when` expression, you can reference:

| Variable | Description |
|----------|--------------|
| `input.*` | Job-level input from the API call or trigger |
| `<step_name>.output.*` | Output from a completed upstream step |
| `secret.*` | Workspace secrets (after rendering) |

**Step name rules**: Step names with hyphens become underscores in templates. A step named `check-data` is referenced as `check_data.output.*`.

### Example: Referencing step outputs

```yaml
actions:
  check-status:
    type: script
    script: |
      status=$(curl -s https://api.example.com/status)
      echo "OUTPUT: {\"ok\": $(echo $status | jq '.healthy')}"
    output:
      ok: { type: boolean }

  do-something:
    type: script
    script: "echo Running..."

tasks:
  monitor:
    flow:
      check:
        action: check-status

      proceed:
        action: do-something
        depends_on: [check]
        # Use check_status's output (note: step name check becomes check, so check.output)
        when: "{{ check.output.ok }}"
```

## Mixed fan-in: required, tolerated, and ordering-only in one tree

Three dependencies on one step rarely all mean the same thing. `accept` lets each edge say exactly what it needs:

```yaml
tasks:
  publish-task:
    flow:
      source:
        action: fetch-source

      enrichment:
        action: enrich-data
        depends_on: [source]

      audit:
        action: audit-log
        depends_on: [source]

      publish:
        action: publish
        depends_on:
          - source                                 # must complete
          - step: enrichment
            accept: [completed, failed]              # tolerates a failed enrichment, not a skip
          - step: audit
            accept: terminal                          # pure ordering: wait for it, don't care how it ends
```

`enrichment` and `audit` each carry their own `continue_on_failure: true` if their own failure shouldn't fail the *job* — that's a separate decision (see [Accounting vs. readiness](#accounting-vs-readiness) below) from whether `publish` *accepts* their failure. This is the shape 0.17's flags had no answer for: "tolerate a dependency's failure, but still block on it being skipped" needs `[completed, failed]` — `skipped` and `omitted` just aren't in the set.

### `any`/`all` grouping

For "at least one of these must succeed," wrap dependencies in `any` (or `all` for an explicit, nestable AND):

```yaml
tasks:
  ranked-task:
    flow:
      audit:
        action: audit-log

      mirror-a:
        action: sync-to-mirror-a
        continue_on_failure: true   # its own failure doesn't fail the job

      mirror-b:
        action: sync-to-mirror-b

      ranked:
        action: rank-mirrors
        depends_on:
          - step: audit
            accept: terminal
          - any: [mirror-a, mirror-b]   # at least one mirror must complete
```

`ranked` runs once `audit` is terminal (whatever its outcome) **and** at least one of `mirror-a`/`mirror-b` completed — even if `mirror-a` fails, as long as `mirror-b` completes. Per the readiness rule above, `ranked` still waits for *both* mirrors to finish before deciding, even though the `any` is already satisfied once one of them completes — no short-circuiting ahead of a slower sibling.

`all`/`any` nest arbitrarily: an `any` can contain an `all`, a `step` entry, or a bare name. The same step name can legitimately appear in two different branches with different `accept` sets (e.g. "`A` and `B` both completed, OR `A` failed and `C` completed" needs `A` once per branch) — that's not a mistake, and it's accepted. What's rejected is the same name appearing twice as a **direct sibling** of the same `all`/`any` node (see [Validation](#validation)).

## Accounting vs. readiness

These are two independent questions, and `accept` only answers one of them:

- **Readiness** — does a dependent get to run? Answered entirely by `depends_on`'s `accept` sets, evaluated per edge.
- **Accounting** — does a step's own failure fail the *job*? Answered entirely by `continue_on_failure` on that step itself, self-scoped, with no propagation to anyone downstream.

```yaml
flow:
  last-step:
    action: do-something
    continue_on_failure: true   # its own failure doesn't fail the job

  cleanup:
    action: remove-temp-files
    depends_on:
      - step: last-step
        accept: terminal         # runs whether last-step completed, failed, or was cancelled
```

`continue_on_failure` on `last-step` means `last-step` failing doesn't fail the job — full stop. It has no effect on whether `cleanup` runs; that's `cleanup`'s own `accept: terminal`. The example above, as written (no `continue_on_failure` on `last-step`), already does the more direct thing: `cleanup` runs after a failure **and** the job still ends `failed` (so `on_error` fires too) — `accept` and `continue_on_failure` are fully decoupled, so a cleanup step doesn't need to leave the flow just to keep the job's outcome honest. Reach for an [`on_error` / `on_cancel` hook](/guides/hooks/) instead when what needs to run isn't really part of the task's own DAG — paging on-call, tearing down infrastructure the flow itself never touched — and you want it to fire exactly once on any terminal outcome without wiring an `accept` edge to every step that might fail.

A dependent's own `when` is still evaluated on its own terms once its `depends_on` tree is satisfied: the tree decides whether the dependent is even considered, not what its own condition renders to.

## Validation

- Every step name referenced anywhere in a `depends_on` tree — including nested inside `all`/`any` — must exist in the flow.
- A typo'd key in a `depends_on` entry (e.g. `accpet` instead of `accept`), or a mapping carrying two discriminating keys at once (e.g. both `step` and `any`), is a hard parse error.
- The same step name may appear more than once across *different* branches of a tree with different `accept` sets — that's a legitimate OR expression, not a mistake. It's rejected only as a **direct sibling** of the same `all`/`any` node (including the implicit top-level `all`): `depends_on: [a, a]`-style duplicates must be deduplicated.
- An empty `Outcomes` list, a duplicate entry within one `accept: [...]` list, or a nested `all: []`/`any: []`, is a validation error (an empty group would trivially always or never pass; a duplicate outcome is always redundant). The one exception: a step with **no** `depends_on` at all — field absent or explicitly `depends_on: []` — is unchanged from today and ready immediately.
- `continue_when_skipped` is retired. A task with a step that still sets it gets a named error pointing at the [0.18 upgrade guide](/operations/upgrade-0-18-dependency-conditions/) instead of silent acceptance — from `stroem validate`, or at job-creation time (server) / before execution (`stroem run`). It is **not** a parse error: a server loading or reloading the workspace does not catch it on its own, only an actual job-creation attempt for the affected task does.

## Outcomes and skip reasons

Every dependency resolves, once terminal, to exactly one of five **outcomes** — this is the vocabulary `accept` sets are built from:

| Outcome | Maps to |
|---|---|
| `completed` | `status = completed` |
| `failed` | `status = failed` |
| `cancelled` | `status = cancelled` |
| `skipped` | `status = skipped`, reason `condition` or `empty` — the step's **own** choice (its `when` was false, or its `for_each` was empty) |
| `omitted` | `status = skipped`, reason `unreachable` (or a legacy `cascade` row from before 0.18, or an unrecognized/NULL reason) — blocked by **its own** dependency tree, not by its own choice |

The job detail page and the API both still show `skip_reason` on the step itself (`condition`, `empty`, or `unreachable`) — that's a diagnostic on the row, independent of what a *downstream* dependent's `accept` set does with it. An `omitted` step tells you "something in my own tree didn't let me through," not *what* — see the [0.18 upgrade guide](/operations/upgrade-0-18-dependency-conditions/#structural-catch-audit) if you need to trace the actual origin several hops back.

## Root Step Conditions

A step with no `depends_on` is ready from the start. Its `when` condition is evaluated immediately at job creation time:

```yaml
tasks:
  root-conditional:
    input:
      skip_setup: { type: boolean, default: false }
    flow:
      setup:
        action: initialize
        when: "{{ not input.skip_setup }}"

      main:
        action: do-work
        depends_on:
          - step: setup
            accept: [completed, skipped]
        # Runs whether setup ran or was skipped by its own `when`.
```

## Error Handling

If a `when` expression **fails to render** (e.g., an undefined variable, an undefined operand of `and` / `or`, or a syntax error), the step fails (status = `failed`, not skipped). This prevents silent failures:

```yaml
tasks:
  error-condition:
    flow:
      check:
        action: validate

      proceed:
        action: next-step
        depends_on: [check]
        # If check.output.status doesn't exist, this step FAILS
        # (doesn't skip — you get an error to fix)
        when: "{{ check.output.status }}"
```

Comparing against a missing **field** is not an error: `{{ check.output.status == 'ok' }}` renders `false` when `status` is missing (`check.output` itself must still exist), so the step is skipped rather than failed. A missing *parent* (`a.b.c` with `b` missing) is still an error — use `a.b?.c`. Template errors never contain rendered values; run `stroem run` locally for Tera's full report.

To handle undefined outputs gracefully, use Tera filters:

```yaml
when: "{{ check.output.status | default(value='') }}"
```

This will skip the step if `check.output.status` is undefined (renders to empty string).

## Common Patterns

### If/else branch

Two mutually exclusive branches that merge back together:

```yaml
tasks:
  if-else:
    input:
      mode: { type: string, enum: [fast, slow] }
    flow:
      fast-path:
        action: quick-process
        when: "{{ input.mode == 'fast' }}"

      slow-path:
        action: thorough-process
        when: "{{ input.mode == 'slow' }}"

      merge:
        action: finalize
        depends_on:
          - step: fast-path
            accept: [completed, skipped]
          - step: slow-path
            accept: [completed, skipped]
        # Exactly one branch runs, the other is skipped `condition`.
        # Both edges accept `skipped`, so merge sees every dependency
        # as satisfied either way.
```

### Multi-step branch

A condition gates a branch of multiple steps. Downstream steps only need `accept` where they actually need to tolerate a skip or failure — a plain `depends_on: [advanced-step-1]` still requires `completed`, and cascades to `omitted` if the branch root was skipped:

```yaml
tasks:
  multi-step-branch:
    input:
      enable_advanced: { type: boolean }
    flow:
      advanced-setup:
        action: advanced-init
        when: "{{ input.enable_advanced }}"

      advanced-step-1:
        action: advanced-processing-1
        depends_on: [advanced-setup]
        # Omitted if advanced-setup was skipped (default accept: [completed])

      advanced-step-2:
        action: advanced-processing-2
        depends_on: [advanced-step-1]
        # Also omitted if advanced-step-1 was omitted

      summary:
        action: generate-report
        depends_on:
          - step: advanced-step-2
            accept: [completed, omitted]
        # Runs even if the whole branch was never entered.
```

### Optional step (skip if not needed)

An optional step in a linear pipeline. Accept its `skipped` outcome on the edge so dependents run whether it runs or is skipped:

```yaml
tasks:
  input-conditional:
    input:
      skip_validation: { type: boolean, default: false }
      data: { type: string, required: true }
    flow:
      prepare:
        action: prepare-data
        input:
          data: "{{ input.data }}"

      validate:
        action: validate-data
        depends_on: [prepare]
        when: "{{ not input.skip_validation }}"

      process:
        action: use-data
        depends_on:
          - prepare
          - step: validate
            accept: [completed, skipped]
```

### Condition based on step output

Run a step only if a previous step's output meets a condition:

```yaml
actions:
  check-data:
    type: script
    script: |
      count=$(curl -s https://api.example.com/count)
      echo "OUTPUT: {\"count\": $count}"
    output:
      count: { type: integer }

  process-large-dataset:
    type: script
    script: "echo Processing large dataset..."

tasks:
  conditional-processing:
    flow:
      count-data:
        action: check-data

      process:
        action: process-large-dataset
        depends_on: [count-data]
        # Only run if count is >= 1000
        when: "{{ count_data.output.count >= 1000 }}"
```

## Example Workflow

A complete example combining multiple conditional patterns:

```yaml
actions:
  pre-check:
    type: script
    script: "echo 'Checking...' && echo 'OUTPUT: {\"ready\": true}'"
    output:
      ready: { type: boolean }

  fast-process:
    type: script
    script: "echo Fast processing"

  slow-process:
    type: script
    script: "echo Slow processing"

  cleanup:
    type: script
    script: "echo Cleanup"

tasks:
  full-example:
    input:
      use_fast: { type: boolean, default: true }
      skip_pre_check: { type: boolean, default: false }
    flow:
      verify:
        action: pre-check
        when: "{{ not input.skip_pre_check }}"

      fast:
        action: fast-process
        depends_on:
          - step: verify
            accept: [completed, skipped]
        when: "{{ input.use_fast }}"

      slow:
        action: slow-process
        depends_on:
          - step: verify
            accept: [completed, skipped]
        when: "{{ not input.use_fast }}"

      finish:
        action: cleanup
        depends_on:
          - step: fast
            accept: [completed, skipped]
          - step: slow
            accept: [completed, skipped]
```

This workflow:
1. Optionally runs the pre-check based on input. Both `fast` and `slow` accept `verify` being `skipped`, so a skip doesn't block either branch from evaluating its own `when`.
2. When `verify` runs (or is skipped), branches into fast or slow path based on input.
3. Converges at `finish` — exactly one of `fast`/`slow` runs, the other is skipped `condition`; `finish` accepts `skipped` on both edges, so it sees every dependency as satisfied.

**When do you still need a non-default `accept`?** Any time a dependent should proceed past something other than a clean `completed` — a dependency that failed, was cancelled, or was skipped (by choice or because its own tree blocked it). There's no automatic convergence: every edge of a merge needs its own `accept`, not just one of them.
