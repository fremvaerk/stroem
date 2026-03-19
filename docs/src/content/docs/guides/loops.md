---
title: For-Each Loops
description: Iterating over lists with for_each to fan-out and fan-in
---

Steps can iterate over a list using `for_each`, running the same action for each item. This enables fan-out/fan-in patterns: expand a step into N parallel (or sequential) instances, then converge when all are done.

## Overview

- **Fan-out**: A `for_each` step is expanded into N instance steps at runtime, one per item in the list
- **Fan-in**: Downstream steps wait for all instances to complete. The placeholder step's output is an array of all instance outputs
- **Parallel by default**: All instances run concurrently. Use `sequential: true` for one-at-a-time execution
- **`each` variable**: Inside loop instances, `each.item` is the current array element and `each.index` is the zero-based index
- **Composable**: Combine with `when` conditions, `type: task` actions, and `continue_on_failure`

## YAML Syntax

### Parallel loop (default)

```yaml
actions:
  get-items:
    type: script
    script: echo 'OUTPUT: {"items": ["a","b","c"]}'
  process-item:
    type: script
    script: |
      echo "Processing {{ input.item }}"
      echo 'OUTPUT: {"result": "done-{{ input.item }}"}'

tasks:
  main:
    flow:
      fetch:
        action: get-items

      process:
        action: process-item
        depends_on: [fetch]
        for_each: "{{ fetch.output.items }}"
        input:
          item: "{{ each.item }}"
          index: "{{ each.index }}"

      aggregate:
        action: combine
        depends_on: [process]
        input:
          results: "{{ process.output }}"
```

When the job runs:
1. `fetch` completes and outputs `{ "items": ["a", "b", "c"] }`
2. `process` expands into `process[0]`, `process[1]`, `process[2]` — all run in parallel
3. `aggregate` waits for all 3 instances, then sees `process.output` as `[{"result":"done-a"}, {"result":"done-b"}, {"result":"done-c"}]`

### Literal array

```yaml
flow:
  deploy:
    action: deploy-to-region
    for_each: ["us-east-1", "eu-west-1", "ap-south-1"]
    input:
      region: "{{ each.item }}"
```

### Sequential loop

Use `sequential: true` to run instances one at a time, in order:

```yaml
flow:
  migrate:
    action: run-migration
    for_each: "{{ fetch.output.schemas }}"
    sequential: true
    input:
      schema: "{{ each.item }}"
```

In sequential mode:
- Instance `[0]` starts immediately; `[1]` starts after `[0]` completes, etc.
- If an instance fails and `continue_on_failure` is not set, remaining instances are skipped
- Useful for ordered operations like database migrations

## The `each` Variable

Inside loop instances, two special variables are available:

| Variable | Type | Description |
|----------|------|-------------|
| `each.item` | any | The current array element for this iteration |
| `each.index` | number | Zero-based index (0, 1, 2, ...) |

These are available in `input` templates and action templates (env, script, cmd).

## Fan-In and Output Aggregation

When all loop instances complete, the placeholder step's output becomes an array of all instance outputs, ordered by index:

```yaml
flow:
  process:
    action: compute
    for_each: ["a", "b", "c"]

  summary:
    action: aggregate
    depends_on: [process]
    input:
      # Array of outputs: [output_0, output_1, output_2]
      all_results: "{{ process.output }}"
```

- Failed instances contribute `null` to the output array (when `continue_on_failure` is set)
- Skipped instances also contribute `null`

## Combining with `when` Conditions

`when` is evaluated **before** `for_each`. If the condition is falsy, the step is skipped entirely (no expansion):

```yaml
flow:
  deploy:
    action: deploy-to-region
    for_each: "{{ fetch.output.regions }}"
    when: "{{ input.should_deploy }}"
    input:
      region: "{{ each.item }}"
```

## Sub-Job Fan-Out (`type: task` + `for_each`)

A `for_each` step can reference a `type: task` action — each iteration creates a separate child job:

```yaml
actions:
  deploy-env:
    type: task
    task: deploy-pipeline

tasks:
  main:
    flow:
      deploy-all:
        action: deploy-env
        for_each: ["staging", "prod"]
        input:
          environment: "{{ each.item }}"

  deploy-pipeline:
    input:
      environment: { type: string }
    flow:
      setup:
        action: init-env
      deploy:
        action: run-deploy
        depends_on: [setup]
```

Each instance creates a full child job with its own steps, logs, and lifecycle. The called task can itself use `for_each` — enabling nested iteration through separate job scopes.

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Empty array | Step is skipped (not failed) |
| Non-array result | Step fails with error |
| Instance failure (no `continue_on_failure`) | Remaining parallel instances continue; placeholder fails when all done |
| Instance failure (sequential, no `continue_on_failure`) | Remaining instances are skipped; placeholder fails |
| Instance failure (`continue_on_failure: true`) | Placeholder completes; failed instance output is `null` in array |
| Template error in `for_each` expression | Step fails with error |

## Limits

- Maximum 10,000 items per `for_each` expression
- Step names must not contain `[` or `]` (reserved for instance naming)
- Nested `for_each` within the same task is not supported — use `type: task` for nesting
