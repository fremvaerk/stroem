---
title: Conditional Flow Steps
description: Using when conditions to control step execution
---

Steps can be conditionally executed based on dynamic expressions evaluated at runtime. The `when` field on a flow step contains a Tera template expression that determines whether the step runs.

## Overview

The `when` field provides runtime control flow without explicit step branching:

- **Condition evaluation**: When a step's dependencies are met, the `when` expression is evaluated
- **Truthy/falsy**: If the result is truthy (non-empty, not "false", not "0"), the step runs. Otherwise it's skipped
- **Cascade**: If ALL of a step's dependencies are skipped, the step is also skipped (mid-branch cascade). If at least one dependency completed, the step proceeds normally.
- **Convergence**: When conditional branches merge, convergence steps run automatically — no `continue_on_failure` needed.
- **Validation**: `when` syntax is validated at YAML parse time (syntax errors are caught early)

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
        depends_on: [setup, check-data]
        # Runs whether check-data completed or was skipped
        # (setup completed → not all deps skipped → step proceeds)
```

## Truthiness Rules

Tera templates render to strings. The following values are considered **falsy** and cause the step to be skipped:

| Value | Skipped? |
|-------|----------|
| Empty string `""` | Yes |
| `"false"` | Yes |
| `"0"` | Yes |
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
      properties:
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

When multiple branches converge back to a single step, the merge step runs automatically because skipped dependencies from conditional `when` expressions are treated as satisfied.

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

      # Branch 2: slow path
      slow-check:
        action: comprehensive-validation
        when: "{{ not input.use_fast_path }}"

      # Convergence: merge branches
      # One of fast-check or slow-check ran, one was skipped
      # Since skipped deps are treated as satisfied, process-results runs automatically
      process-results:
        action: handle-checks
        depends_on: [fast-check, slow-check]
        # Proceeds whether both ran, or one was skipped
```

**How it works**: Skipped dependencies count as satisfied. A convergence step runs as long as at least one of its dependencies completed. If ALL dependencies are skipped (which would mean no branch was taken), the step is also skipped (mid-branch cascade).

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
        depends_on: [setup]
        # If setup is skipped and main has no other deps,
        # main is also cascade-skipped (all deps skipped).
        # To make main always run, remove the dependency
        # or add a non-conditional dep.
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

      slow-path:
        action: thorough-process
        when: "{{ input.mode == 'slow' }}"

      merge:
        action: finalize
        depends_on: [fast-path, slow-path]
        # Runs automatically: one branch ran, one was skipped
        # Skipped deps are treated as satisfied
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

Note: In this pattern, `summary` also skips because its only dependency (`advanced-step-2`) is skipped when the branch is disabled. If you want `summary` to always run, add a second dependency from outside the branch to ensure at least one dependency completes.

### Optional step (skip if not needed)

An optional step in a linear pipeline. Use a shared root dependency so the downstream step has at least one completed dep:

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
        depends_on: [prepare, validate]
        # Runs whether validate completed or was skipped
        # (prepare completed → not all deps skipped → step proceeds)
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
      properties:
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
      properties:
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

      # Conditional branches based on input
      fast:
        action: fast-process
        depends_on: [verify]
        when: "{{ input.use_fast }}"

      slow:
        action: slow-process
        depends_on: [verify]
        when: "{{ not input.use_fast }}"

      # Convergence: merge branches
      finish:
        action: cleanup
        depends_on: [fast, slow]
        # Runs automatically: at least one branch completes
```

This workflow:
1. Optionally runs the pre-check based on input (if skipped, the entire downstream cascade is skipped)
2. When verify runs, branches into fast or slow path based on input
3. Converges at finish — one branch completed, one skipped → finish runs automatically

**When do you still need `continue_on_failure`?** Only when you want a step to run even if its dependency **failed** (error, crash). Skipped dependencies from `when` conditions are handled automatically.
