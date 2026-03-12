---
title: Conditional Flow Steps
description: Using when conditions to control step execution
---

Steps can be conditionally executed based on dynamic expressions evaluated at runtime. The `when` field on a flow step contains a Tera template expression that determines whether the step runs.

## Overview

The `when` field provides runtime control flow without explicit step branching:

- **Condition evaluation**: When a step's dependencies are met, the `when` expression is evaluated
- **Truthy/falsy**: If the result is truthy (non-empty, not "false", not "0"), the step runs. Otherwise it's skipped
- **Cascade**: Steps that depend on a skipped step are also skipped, unless they have `continue_on_failure: true`
- **Convergence**: After conditional branches, use `continue_on_failure: true` on the merge step to accept skipped dependencies
- **Validation**: `when` syntax is validated at YAML parse time (syntax errors are caught early)

## YAML Syntax

```yaml
tasks:
  conditional-task:
    input:
      run_checks: { type: boolean, default: false }
    flow:
      check-data:
        action: validate-input
        when: "{{ input.run_checks }}"

      use-data:
        action: process-data
        depends_on: [check-data]
        # Only runs if check-data ran (not skipped)

      cleanup:
        action: finalize
        depends_on: [use-data]
        continue_on_failure: true
        # Runs regardless of whether use-data was skipped
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

## Convergence Pattern (continue_on_failure)

When multiple branches converge back to a single step, use `continue_on_failure: true` so the merge step can accept both skipped and completed dependencies.

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
      # continue_on_failure allows us to accept the skipped one
      process-results:
        action: handle-checks
        depends_on: [fast-check, slow-check]
        continue_on_failure: true
        # Proceeds whether both ran, or one was skipped
```

Without `continue_on_failure: true` on the merge step, if `fast-check` is skipped, `process-results` would also be skipped (cascade). With it, `process-results` runs regardless.

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
        # Runs regardless — depends_on is the ordering rule,
        # even if setup was skipped due to when condition
        continue_on_failure: true
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

Two mutually exclusive branches, merge with continue_on_failure:

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
        continue_on_failure: true
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

      # Convergence
      summary:
        action: generate-report
        depends_on: [advanced-step-2]
        continue_on_failure: true
        # Runs whether advanced branch ran or was skipped
```

### Input-based skip

Skip a step based on job input:

```yaml
tasks:
  input-conditional:
    input:
      skip_validation: { type: boolean, default: false }
      data: { type: string, required: true }
    flow:
      validate:
        action: validate-data
        input:
          data: "{{ input.data }}"
        when: "{{ not input.skip_validation }}"

      process:
        action: use-data
        input:
          data: "{{ input.data }}"
        depends_on: [validate]
        continue_on_failure: true
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
        continue_on_failure: true

      slow:
        action: slow-process
        depends_on: [verify]
        when: "{{ not input.use_fast }}"
        continue_on_failure: true

      # Convergence with error tolerance
      finish:
        action: cleanup
        depends_on: [fast, slow]
        continue_on_failure: true
```

This workflow:
1. Optionally skips the pre-check based on input
2. Branches into fast or slow path based on input
3. Converges at cleanup regardless of which branch ran
4. Tolerates failures with `continue_on_failure: true`
