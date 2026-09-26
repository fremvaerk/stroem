---
title: Conditional Flow Steps
description: Using when conditions to control step execution
---

Steps can be conditionally executed based on dynamic expressions evaluated at runtime. The `when` field on a flow step contains a Tera template expression that determines whether the step runs.

## Overview

The `when` field provides runtime control flow without explicit step branching:

- **Condition evaluation**: When a step's dependencies are met, the `when` expression is evaluated
- **Truthy/falsy**: If the result is truthy (non-empty, not "false", not "0"), the step runs. Otherwise it's skipped
- **Strict AND**: A step runs only when **every** dependency lets it through — completed, or not completed but carrying the matching flag **on itself**. A dependency skipped by choice (its own `when`, an empty `for_each`) needs `continue_when_skipped: true` on itself; a failed or cancelled dependency needs `continue_on_failure: true` on itself. There is no automatic convergence — a completed sibling never makes up for an unflagged skipped dependency (see [Skip Reasons](#skip-reasons))
- **Validation**: `when` syntax is validated at YAML parse time (syntax errors are caught early); `stroem validate` also warns when a merge depends on a skippable step that lacks `continue_when_skipped`

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
        continue_when_skipped: true
        # Lets `process` through when this step is skipped by its own `when`.

      process:
        action: process-data
        depends_on: [setup, check-data]
        # Runs when setup completed AND check-data either completed
        # or was skipped (its continue_when_skipped lets it through).
        # Without that flag on check-data, process would be skipped `cascade`.
```

## Truthiness Rules

Tera templates render to strings. The following values are considered **falsy** and cause the step to be skipped:

| Value | Skipped? |
|-------|----------|
| Empty string `""` | Yes |
| `"false"` (case-insensitive: `"False"`, `"FALSE"`, etc.) | Yes |
| `"0"` | Yes |
| `"null"` (case-insensitive: `"Null"`, `"NULL"`, etc.) | Yes |
| `"none"` (case-insensitive: `"None"`, `"NONE"`, etc.) | Yes |
| `"true"`, `"1"`, any other string | No |
| Template error (e.g., undefined variable) | Fails the step |

:::caution
Empty strings evaluate to falsy. Use `{{ input.value \| default(value='') }}` carefully — it will skip the step if undefined.
:::

## Available Template Variables

Inside a `when` expression, you can reference:

| Variable | Description |
|----------|-------------|
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

## Convergence Pattern

When multiple branches converge back to a single step, the merge runs only if **every** dependency lets it through. A dependency that completed always does; a dependency that was skipped *by choice* — its own `when` was false, its `for_each` produced nothing, or it was itself skipped `cascade` — only does if it carries `continue_when_skipped: true` **on itself**. There is no automatic convergence: a completed sibling branch does not make up for an unflagged skipped one.

```yaml
tasks:
  branching-workflow:
    input:
      use_fast_path: { type: boolean }
    flow:
      # Branch 1: fast path
      fast-check:
        action: quick-validation
        when: "{{ input.use_fast_path }}"
        continue_when_skipped: true

      # Branch 2: slow path
      slow-check:
        action: comprehensive-validation
        when: "{{ not input.use_fast_path }}"
        continue_when_skipped: true

      # Convergence: merge branches
      # Exactly one of fast-check / slow-check runs; the other is skipped
      # `condition`. Both carry continue_when_skipped, so either way
      # process-results sees every dependency as satisfied.
      process-results:
        action: handle-checks
        depends_on: [fast-check, slow-check]
```

**How it works**: each dependency is judged on its own — completed is always a Pass; a choice-skip is a Pass only with `continue_when_skipped` on that same step. `process-results` runs only when both verdicts are Pass. Drop `continue_when_skipped` from either branch step and the merge is skipped `cascade` instead of running — `stroem validate` warns about exactly this shape ("step 'process-results' will be skipped whenever 'fast-check' is skipped (add continue_when_skipped: true to 'fast-check' to let process-results run)").

## Running After a Skipped Branch

Sometimes a step should run even when the only step it depends on was skipped — a report after an optional check, or a merge after an if/else where both arms may be off. Set `continue_when_skipped: true` on the step that may be skipped; steps that depend only on it are then not cascade-skipped. When a step has several skipped dependencies, every one of them must carry the flag — there is no convergence shortcut.

```yaml
tasks:
  optional-check:
    input:
      run_check: { type: boolean, default: false }
    flow:
      check:
        action: run-check
        when: "{{ input.run_check }}"
        continue_when_skipped: true
        # Dependents run whether check completed or was skipped by its condition.

      report:
        action: write-report
        depends_on: [check]
        # {{ check.output }} is null when check was skipped.
```

A skipped dependency renders as `null` in templates, and a template error fails the step. So in a step whose skipped dependency you're tolerating, guard any reference into that dependency's output: use `{% if check.output %}…{% endif %}` or `{{ check.output.count | default(value=0) }}` rather than `{{ check.output.count }}`.

`continue_when_skipped` covers skips **by choice** only: a `when` that rendered false, an empty `for_each`, or a chain of such skips. If the dependency was instead skipped because an upstream step **failed** or was cancelled (`unreachable`), or it failed itself, the dependent needs `continue_on_failure` on that same dependency. To run no matter what happened upstream — skipped by choice, failed, or cancelled — set **both** flags on the dependency:

```yaml
      last-step:
        action: do-something
        continue_when_skipped: true
        continue_on_failure: true

      cleanup:
        action: remove-temp-files
        depends_on: [last-step]
        # Runs no matter what happened to last-step: completed, skipped, or failed.
```

Both flags are read from `last-step` itself, never from `cleanup` — a step's own flags never make *it* run. See [Migration Guide: 0.17](/operations/upgrade-0-17-dependency-flags/) if your workflows still put `continue_on_failure` on the dependent (the 0.16.x placement) or rely on automatic convergence.

A dependent's own `when` is still evaluated on its own terms once the gate is open: a dependency's flags decide whether the dependent is even considered, not what its own condition renders to.

## Skip Reasons

Every skipped step records why it was skipped. The job detail page shows it as a badge and the API returns it as `skip_reason` on the step:

| Reason | Meaning |
|---|---|
| `condition` | the step's own `when` rendered falsy |
| `empty` | the step's `for_each` produced no items |
| `cascade` | a dependency was skipped by choice and does not carry `continue_when_skipped` (no dependency failed) |
| `unreachable` | a dependency failed or was cancelled, or a dependency was itself skipped `unreachable` — or a dependency's skip reason is missing/unrecognized (read conservatively as a failure) |

`unreachable` travels down a chain: if `a` fails, `b` is skipped `unreachable` unless `a` itself has `continue_on_failure`, and so is anything that depends on `b` unless `b` itself has the flag — **even a step whose other dependencies completed**, because a failure verdict always wins over a completed one (strict AND, [Overview](#overview)). A merge after three parallel branches does not run when one branch failed upstream and that branch (or something below it) has no `continue_on_failure`, and neither does anything after the merge — an uncaught failure fails the job even if it never shows up as a `failed` row past that point. `continue_when_skipped` never reaches past an `unreachable` skip; only `continue_on_failure`, set on the failing (or intervening) step itself, does.

:::note[Changed in 0.17]
Before 0.17, whether a dependent ran after a failure or a choice-skip was decided partly by the dependent's *own* `continue_on_failure`, and a choice-skip with at least one completed sibling converged automatically. Now every dependency is judged only by its own flags, strict AND applies with no exception, and there is no automatic convergence — see the [0.17 upgrade guide](/operations/upgrade-0-17-dependency-flags/). A skipped step with no recorded reason, or an unrecognized one (jobs from before 0.16.2, or carried into a restart), is still read as `unreachable`, as since 0.16.5.
:::

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
        # Add `continue_when_skipped: true` here so main runs anyway.

      main:
        action: do-work
        depends_on: [setup]
        # If setup is skipped and main has no other deps, main is also
        # cascade-skipped unless setup itself sets continue_when_skipped.
```

## Error Handling

If a `when` expression **fails to render** (e.g., undefined variable or syntax error), the step fails (status = `failed`, not skipped). This prevents silent failures:

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
        continue_when_skipped: true

      slow-path:
        action: thorough-process
        when: "{{ input.mode == 'slow' }}"
        continue_when_skipped: true

      merge:
        action: finalize
        depends_on: [fast-path, slow-path]
        # Exactly one branch runs, the other is skipped `condition`.
        # Both branch steps carry continue_when_skipped, so merge sees
        # every dependency as satisfied either way. Drop the flag from
        # either branch and merge is skipped `cascade` instead.
```

### Multi-step branch with cascade

A condition gates a branch of multiple steps. All downstream steps cascade if the root is skipped:

```yaml
tasks:
  multi-step-branch:
    input:
      enable_advanced: { type: boolean }
    flow:
      # Root of the advanced branch
      advanced-setup:
        action: advanced-init
        when: "{{ input.enable_advanced }}"

      advanced-step-1:
        action: advanced-processing-1
        depends_on: [advanced-setup]
        # Skipped if advanced-setup was skipped (cascade)

      advanced-step-2:
        action: advanced-processing-2
        depends_on: [advanced-step-1]
        # Also skipped if advanced-step-1 was skipped

      # Convergence (note: summary only has one dep, so it also skips if entire branch is skipped)
      summary:
        action: generate-report
        depends_on: [advanced-step-2]
        # Skips if advanced-step-2 is skipped (all-deps-skipped rule)
```

Note: In this pattern, `summary` also skips because its only dependency (`advanced-step-2`) is skipped when the branch is disabled. If you want `summary` to run even when the whole branch was skipped, set `continue_when_skipped: true` on `advanced-step-2` itself (see [Running After a Skipped Branch](#running-after-a-skipped-branch)).

### Optional step (skip if not needed)

An optional step in a linear pipeline. Set `continue_when_skipped` on the optional step so its dependents run whether it runs or is skipped:

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
        continue_when_skipped: true

      process:
        action: use-data
        depends_on: [prepare, validate]
        # Runs when prepare completed AND validate either completed or
        # was skipped (its own continue_when_skipped lets it through).
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
      # Root condition step
      verify:
        action: pre-check
        when: "{{ not input.skip_pre_check }}"
        continue_when_skipped: true

      # Conditional branches based on input
      fast:
        action: fast-process
        depends_on: [verify]
        when: "{{ input.use_fast }}"
        continue_when_skipped: true

      slow:
        action: slow-process
        depends_on: [verify]
        when: "{{ not input.use_fast }}"
        continue_when_skipped: true

      # Convergence: merge branches
      finish:
        action: cleanup
        depends_on: [fast, slow]
        # Runs because fast and slow each either complete or are skipped
        # `condition` with continue_when_skipped set on themselves.
```

This workflow:
1. Optionally runs the pre-check based on input. `verify` carries `continue_when_skipped`, so a skip does not cascade-block `fast` and `slow` — without it, `fast` and `slow` would be skipped `cascade` without evaluating their own `when` — and because they carry `continue_when_skipped`, `finish` would still run, just with neither branch processed.
2. When `verify` runs (or is skipped), branches into fast or slow path based on input.
3. Converges at `finish` — exactly one of `fast` / `slow` runs, the other is skipped `condition`; both carry `continue_when_skipped`, so `finish` sees every dependency as satisfied.

**When do you still need `continue_on_failure`?** When you want a step's dependents to run even though it **failed** (error, crash), was cancelled, or was itself skipped `unreachable` — set the flag on that step, not on the dependent. `continue_when_skipped` never covers a failure-class skip, only a choice-skip; and there is no automatic convergence, so every choice-skipped dependency of a merge needs the flag on itself, not just one of them.
