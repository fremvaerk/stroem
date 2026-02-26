---
title: Hooks
description: on_success and on_error hooks for automated job responses
---

Tasks can define `on_success` and `on_error` hooks that fire automatically when a job reaches a terminal state. Each hook references an existing action and can pass input rendered with job context.

## Syntax

```yaml
tasks:
  deploy:
    flow:
      build:
        action: build-app
      deploy:
        action: deploy-app
        depends_on: [build]
    on_success:
      - action: notify-slack
        input:
          webhook_url: "{{ secret.SLACK_WEBHOOK_URL }}"
          message: "Deploy {{ hook.task_name }} succeeded ({{ hook.duration_secs }}s)"
    on_error:
      - action: notify-slack
        input:
          webhook_url: "{{ secret.SLACK_WEBHOOK_URL }}"
          message: "Deploy {{ hook.task_name }} FAILED: {{ hook.error_message }}"
```

## Hook context variables

Hook input values are Tera templates with access to a `hook` object containing job context, plus workspace `secrets` under the `secret` key:

| Variable | Type | Description |
|----------|------|-------------|
| `hook.workspace` | string | Workspace name |
| `hook.task_name` | string | Task that completed/failed |
| `hook.job_id` | string | UUID of the original job |
| `hook.status` | string | `"completed"` or `"failed"` |
| `hook.is_success` | bool | true if completed |
| `hook.error_message` | string/null | All failed step errors combined |
| `hook.source_type` | string | Original job source (`"api"`, `"trigger"`, etc.) |
| `hook.source_id` | string/null | Original job source ID |
| `hook.started_at` | string/null | ISO 8601 timestamp |
| `hook.completed_at` | string/null | ISO 8601 timestamp |
| `hook.duration_secs` | number/null | Execution duration in seconds |
| `hook.failed_steps` | array | Failed step details (see below) |

Each entry in `hook.failed_steps` contains:

| Field | Type | Description |
|-------|------|-------------|
| `step_name` | string | Name of the failed step |
| `action_name` | string | Action that was executed |
| `error_message` | string/null | The step's error message |
| `continue_on_failure` | bool | Whether the step had `continue_on_failure` set |

## Behavior

- **Fire-and-forget**: Hook job creation is best-effort. Failures are logged but never affect the original job.
- **No recursion**: Jobs created by hooks (`source_type = "hook"`) never trigger further hooks.
- **Visible as jobs**: Hook jobs appear in the job list with `task_name = "_hook:<action>"` and `source_type = "hook"`.
- **Normal execution**: Hook jobs go through the normal claim/execute flow on workers.
- **Multiple hooks**: You can define multiple hooks per event. They all fire independently.
- **Validation**: Hook action references are validated at parse time — referencing a non-existent action is an error.
- **Failure visibility**: If a hook job fails at runtime, the failure is logged as a server event on the original job (visible in the "Server Events" panel on the job detail page).

## Task actions in hooks

Hook actions can be `type: task`, creating a full child job for the referenced task instead of a single-step hook job:

```yaml
tasks:
  deploy:
    flow:
      deploy:
        action: deploy-app
    on_error:
      - action: run-cleanup
        input:
          env: "{{ hook.workspace }}"
```
