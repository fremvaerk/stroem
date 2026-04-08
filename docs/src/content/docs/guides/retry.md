---
title: Retry Mechanisms
description: Step-level and task-level retry with backoff strategies for handling transient failures
---

Strøm provides powerful retry mechanisms for handling transient failures. You can retry individual steps with configurable backoff, or retry entire tasks by creating new jobs. This guide covers both strategies and shows when to use each.

## Overview

Strøm has two retry layers:

1. **Step-level retry**: Retries a failed step in-place, with optional backoff between attempts
2. **Task-level retry**: Retries the entire task by creating a new job when the task fails

Both can use **fixed** or **exponential** backoff, with optional **jitter** to prevent thundering herd.

## Step-Level Retry

Step-level retry retries a single step without re-running dependencies. The step is reset and allows a worker to claim and re-execute it. This is fast and focused, ideal for transient failures.

### Basic syntax

```yaml
tasks:
  main:
    flow:
      fetch:
        action: fetch-data
        retry:
          max_attempts: 3
          delay: "30s"
          backoff: fixed
```

**Behavior:**
- Attempt 1: Step runs and fails
- Wait 30s
- Attempt 2: Step resets and a worker claims it again
- If Attempt 2 fails, wait 30s again
- Attempt 3: Final attempt
- If all 3 fail, the step fails and downstream steps are skipped

### Backoff strategies

#### Fixed backoff (default)

Same delay between all retry attempts:

```yaml
retry:
  delay: "30s"
  backoff: fixed
```

Use when: Failures are truly transient and waiting a fixed time is sufficient (e.g., temporary network blip).

#### Exponential backoff

Delay doubles after each failure, up to a 64x cap:

```yaml
retry:
  delay: "5s"
  backoff: exponential
```

Delays: 5s, 10s, 20s, 40s, 80s (capped at 320s), 320s, ...

Use when: You expect increasing wait times help (e.g., service warming up, queue draining).

#### Jitter

Add 0–25% random variance to smooth out synchronized retries across many concurrent steps:

```yaml
retry:
  delay: "10s"
  backoff: exponential
  jitter: true
```

With jitter, each delay varies randomly:
- Nominal 10s → actual 7.5s–12.5s
- Nominal 20s → actual 15s–25s

Use when: Running many tasks in parallel (prevents thundering herd at retry windows).

### Example: Unreliable API calls

```yaml
actions:
  call-external-api:
    type: script
    script: |
      curl -f https://api.example.com/data || exit 1
      echo 'OUTPUT: {"status": "success"}'

tasks:
  fetch-and-process:
    flow:
      fetch:
        action: call-external-api
        retry:
          max_attempts: 5
          delay: "10s"
          backoff: exponential
          jitter: true

      process:
        action: process-data
        depends_on: [fetch]
        input:
          data: "{{ fetch.output }}"
```

The `fetch` step will retry up to 5 times with exponential backoff + jitter before failing the entire task.

### Step retry vs. action defaults

Actions can define default retry config, overridable per step:

```yaml
actions:
  unstable-task:
    type: script
    script: "flaky-operation"
    retry:
      max_attempts: 2

tasks:
  main:
    flow:
      step1:
        action: unstable-task
        # Uses action default: 2 max_attempts

      step2:
        action: unstable-task
        retry:
          max_attempts: 5  # Overrides action default
```

**Resolution:** Step-level retry takes precedence. If a step doesn't specify retry, it inherits from the action.

## Task-Level Retry

Task-level retry retries the **entire task** by creating a new job when the task fails. All steps restart from the beginning.

### Basic syntax

```yaml
tasks:
  deploy:
    retry:
      max_attempts: 2
      delay: "60s"
    flow:
      build:
        action: build-app
      test:
        action: run-tests
      deploy:
        action: deploy-app
```

**Behavior:**
- Original job fails (e.g., at `deploy` step)
- Wait 60s
- Server creates a **new job** with the same task and input, starting from `build`
- If new job fails, no further retries (max_attempts: 2 exhausted)
- `on_error` hooks fire only on final failure, not after each retry attempt

### Retry chain

Each retry job is linked to the previous via `retry_of_job_id`:

```
Job A (failed at deploy step)
  └─ Job B (failed at test step) [retry of Job A]
      └─ Job C (success) [retry of Job B]
```

View the chain in the UI or via API to understand failure progression.

### When to use task retry

Use task-level retry when:
- Failures require starting from scratch (e.g., state cleanup)
- Dependencies need reloading (e.g., config refresh)
- Full job restart is acceptable (overhead is small)

Use step retry instead when:
- Failure is isolated to one step (don't restart everything)
- Each attempt is expensive (build steps, deployments)
- You want faster recovery

### Example: Deployment with state recovery

```yaml
tasks:
  deploy-service:
    retry:
      max_attempts: 2
      delay: "30s"
      backoff: exponential
    flow:
      check-health:
        action: check-service-health

      backup:
        action: backup-current-version
        depends_on: [check-health]

      deploy:
        action: deploy-new-version
        depends_on: [backup]

      verify:
        action: verify-deployment
        depends_on: [deploy]

    on_error:
      - action: notify-ops
        input:
          message: "Deploy failed after {{ hook.failed_steps | length }} step(s)"
```

If the deployment fails, the entire task retries — all steps run again with the latest version and state.

## Interactions with Other Features

### Retry + `continue_on_failure`

Step retry exhausts before `continue_on_failure` applies:

```yaml
flow:
  unstable:
    action: might-fail
    retry:
      max_attempts: 3
    continue_on_failure: true

  next:
    action: next-step
    depends_on: [unstable]
    # Runs even if all 3 retry attempts fail
```

If `unstable` exhausts retries and fails, `next` still runs because of `continue_on_failure`.

### Retry + `for_each` loops

Loop instances inherit the step's retry config:

```yaml
flow:
  process-items:
    action: process
    for_each: ["item-a", "item-b", "item-c"]
    retry:
      max_attempts: 2
    input:
      item: "{{ each.item }}"

  # Each instance (process[0], process[1], process[2]) gets up to 2 attempts
```

If `process[1]` fails, it retries independently; `process[0]` and `process[2]` are unaffected.

### Retry + `when` conditions

Retry only applies if the step actually runs:

```yaml
flow:
  maybe-deploy:
    action: deploy
    when: "{{ input.deploy_now == true }}"
    retry:
      max_attempts: 3
    # If condition is false, step is skipped — no retry attempted
```

### Retry + timeouts

**Step timeout** applies per attempt. **Task timeout** applies to entire retry chain:

```yaml
tasks:
  main:
    timeout: 10m
    flow:
      slow-step:
        action: slow-operation
        timeout: 2m     # 2m per attempt
        retry:
          max_attempts: 3
          delay: "30s"
        # Possible timeline: 2m + 30s + 2m + 30s + 2m = 7m total
        # Well under 10m task timeout
```

If the task timeout is exceeded, all remaining retries are cancelled.

### Retry + hooks

Hooks fire **only on final failure** (after all retries exhausted), not after each retry attempt:

```yaml
tasks:
  main:
    flow:
      unstable:
        action: flaky-operation
        retry:
          max_attempts: 3

    on_error:
      - action: alert-ops
        # Fires only after all 3 attempts fail
```

On success (before exhausting retries), `on_success` hooks fire normally.

### Retry + task actions

Task-level retry does **not** retry child jobs created by `type: task` actions. Only the top-level task retries:

```yaml
tasks:
  parent:
    retry:
      max_attempts: 2
    flow:
      spawn-child:
        action: run-subtask
        # run-subtask is a type: task action

      next:
        action: next-step
        depends_on: [spawn-child]

  subtask:
    flow:
      work: { action: do-something }
```

If `subtask` fails, it does **not** trigger `parent`'s retry. To retry a child job, define retry on the individual steps within `subtask`.

## Limits and Constraints

| Constraint | Limit | Notes |
|-----------|-------|-------|
| `max_attempts` | 1–10 | 1 = no retries (attempt once), 10 = max 9 retries |
| `delay` | 0–3600s (1h) | Can be `0s` for immediate retry, or `1h` for long waits |
| Exponential backoff cap | 64x initial delay | Prevents unbounded growth |
| Step timeout per attempt | Max 24h | Applies to each individual attempt |
| Task timeout (entire retry chain) | Max 7 days | Includes all retry attempts and delays |

## Best Practices

### 1. Use step retry for isolated transient failures

```yaml
retry:
  max_attempts: 3
  delay: "5s"
  backoff: exponential
```

Examples: network timeouts, temporary rate limiting, brief service unavailability.

### 2. Use task retry for cascading or state-dependent failures

```yaml
retry:
  max_attempts: 2
  delay: "60s"
```

Examples: deployment rollback needed, config reload required, full health check.

### 3. Add jitter for high concurrency

```yaml
retry:
  delay: "10s"
  backoff: exponential
  jitter: true  # Spreads out retry waves
```

### 4. Avoid excessive retries

Too many attempts waste time:

```yaml
# Good
retry:
  max_attempts: 3
  delay: "10s"

# Avoid
retry:
  max_attempts: 10  # Likely a sign of deeper issue
  delay: "1s"
```

### 5. Combine with logging for observability

Include details in actions so you can debug:

```yaml
actions:
  call-api:
    type: script
    script: |
      echo "Attempt with timeout..." >&2
      curl --max-time 5 https://api.example.com || {
        echo "Request failed, will retry" >&2
        exit 1
      }

tasks:
  main:
    flow:
      fetch:
        action: call-api
        retry:
          max_attempts: 3
          delay: "10s"
```

### 6. Monitor retry metrics

In the UI:
- View retry attempt count and history on step detail
- Check `retry_of_job_id` on task-retried jobs to trace failure chains
- Use server logs to identify patterns (e.g., if step fails on Attempt 1 every time, increase initial delay)

## Examples

### Example 1: Flaky external API

```yaml
actions:
  fetch-weather:
    type: script
    script: |
      curl -f "https://api.weather.example.com/forecast?lat=55&lon=12" || exit 1

tasks:
  get-forecast:
    input:
      city: { type: string }
    flow:
      weather:
        action: fetch-weather
        retry:
          max_attempts: 5
          delay: "5s"
          backoff: exponential
          jitter: true

      process:
        action: process-forecast
        depends_on: [weather]
        input:
          data: "{{ weather.output }}"
```

Calls the weather API with exponential backoff (5s, 10s, 20s, 40s, 80s) and jitter.

### Example 2: Database migration retry

```yaml
tasks:
  run-migration:
    retry:
      max_attempts: 2
      delay: "30s"
    flow:
      check-db:
        action: health-check-db

      migrate:
        action: run-sql-migration
        depends_on: [check-db]

      verify:
        action: verify-schema
        depends_on: [migrate]

    on_success:
      - action: notify-team
        input:
          message: "Migration {{ hook.task_name }} completed"

    on_error:
      - action: alert-dba
        input:
          message: "Migration failed: {{ hook.error_message }}"
```

Entire migration retries if any step fails, up to 2 total attempts.

### Example 3: Batch processing with step retries

```yaml
tasks:
  process-batch:
    input:
      items: { type: array }
    flow:
      fetch-items:
        action: get-items-from-db
        input:
          filter: "{{ input.filter }}"

      process:
        action: transform-item
        for_each: "{{ fetch-items.output.items }}"
        retry:
          max_attempts: 3
          delay: "10s"
          backoff: exponential
        input:
          item: "{{ each.item }}"

      aggregate:
        action: combine-results
        depends_on: [process]
        continue_on_failure: true
        input:
          results: "{{ process.output }}"
```

Each item is processed with step retry; `aggregate` runs even if some items exhaust retries.

## Troubleshooting

### Steps keep retrying, slowing down the job

**Symptom:** A step retries repeatedly, causing the job to stall.

**Causes:**
- `max_attempts` too high (e.g., 10 with 30s delay = 5 minutes of retrying)
- `delay` too long or `backoff` too aggressive

**Fix:**
- Lower `max_attempts` to 2–3
- Use `fixed` backoff instead of exponential
- Investigate root cause — if failure is consistent, retries won't help

### Hooks fire on each retry attempt

**Symptom:** `on_error` hook fires multiple times during retries.

**Cause:** Misunderstanding — hooks only fire when the step/task is **terminal** (all retries exhausted).

**Verify:** Check hook logs; they should appear once, after the final attempt.

### Task retry creates orphaned jobs

**Symptom:** Retry chain breaks in the UI.

**Cause:** This shouldn't happen; `retry_of_job_id` foreign key ensures referential integrity.

**Fix:** Check logs for errors during retry creation. Contact support if the issue persists.

### Exponential backoff delays feel wrong

**Symptom:** Delays don't seem to match expected exponential growth.

**Cause:** Backoff is capped at 64x the initial delay. With `delay: 10s`, max is 640s (~11 min).

**Verify:** Check the step detail UI, which shows the delay before each retry.

---

See the [Workflow YAML reference](/reference/workflow-yaml/#retry) for complete field documentation.
