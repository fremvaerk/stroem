---
title: Hooks
description: Task-level and workspace-level on_success / on_error hooks for automated job responses
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

## on_suspended hooks

Tasks can define `on_suspended` hooks that fire when any step in the task enters `suspended` state, typically from an approval gate. Use this to notify approvers or escalate when human action is required.

### Syntax

```yaml
tasks:
  deploy-production:
    on_suspended:
      - action: notify-approvers
        input:
          channel: "#deploy-approvals"
          message: "{{ hook.task_name }} in {{ hook.workspace }} is awaiting approval"
    flow:
      approve:
        type: approval
        message: "Approve production deployment?"
      deploy:
        action: deploy-script
        depends_on: [approve]
```

### Available variables

`on_suspended` hooks use the same template context as `on_success` and `on_error`:

| Variable | Type | Description |
|----------|------|-------------|
| `hook.workspace` | string | Workspace name |
| `hook.task_name` | string | Task containing the suspended step |
| `hook.job_id` | string | UUID of the job |
| `hook.status` | string | Always `"suspended"` |
| `hook.source_type` | string | Original job source (`"api"`, `"trigger"`, etc.) |
| `hook.source_id` | string/null | Original job source ID |
| `hook.started_at` | string/null | ISO 8601 timestamp |

### Practical examples

**Slack notification with actionable message:**

```yaml
actions:
  notify-slack-approval:
    type: script
    script: |
      curl -X POST "$WEBHOOK_URL" \
        -H 'Content-Type: application/json' \
        -d '{
          "text": "{{ input.message }}",
          "blocks": [
            {
              "type": "section",
              "text": {"type": "mrkdwn", "text": "{{ input.message }}"}
            },
            {
              "type": "context",
              "elements": [{"type": "mrkdwn", "text": "Job ID: {{ input.job_id }}"}]
            }
          ]
        }'
    input:
      message: { type: string }
      job_id: { type: string }

tasks:
  deploy:
    on_suspended:
      - action: notify-slack-approval
        input:
          message: |
            *Approval needed for {{ hook.task_name }}*
            Workspace: {{ hook.workspace }}
            Job: {{ hook.job_id }}
          job_id: "{{ hook.job_id }}"
    flow:
      approve:
        type: approval
        message: "Deploy {{ input.service }} to production?"
      deploy:
        action: deploy-app
        depends_on: [approve]
```

**PagerDuty escalation:**

```yaml
tasks:
  critical-maintenance:
    on_suspended:
      - action: create-pagerduty-incident
        input:
          title: "{{ hook.task_name }} awaiting approval in {{ hook.workspace }}"
          service_id: "{{ secret.pagerduty_service_id }}"
          job_id: "{{ hook.job_id }}"
    flow:
      approve:
        type: approval
        message: "Proceed with critical maintenance?"
        timeout: 1h
      execute:
        action: maintenance-script
        depends_on: [approve]
```

## Behavior

- **Fire-and-forget**: Hook job creation is best-effort. Failures are logged but never affect the original job.
- **No recursion**: Jobs created by hooks (`source_type = "hook"`) never trigger further hooks.
- **Visible as jobs**: Hook jobs appear in the job list with `task_name = "_hook:<action>"` and `source_type = "hook"`.
- **Normal execution**: Hook jobs go through the normal claim/execute flow on workers.
- **Multiple hooks**: You can define multiple hooks per event. They all fire independently.
- **Validation**: Hook action references are validated at parse time — referencing a non-existent action is an error.
- **Failure visibility**: If a hook job fails at runtime, the failure is logged as a server event on the original job (visible in the "Server Events" panel on the job detail page).

## Workspace-level hooks

You can define `on_success` and `on_error` hooks at the top level of your workflow YAML. These act as **fallback defaults**: they fire only when the specific task has no hooks defined for that event type.

```yaml
actions:
  notify-slack:
    type: script
    script: 'curl -X POST "$WEBHOOK_URL" -d "{\"text\": \"$MESSAGE\"}"'
    input:
      message:
        type: string

on_success:
  - action: notify-slack
    input:
      message: "Job {{ hook.task_name }} in {{ hook.workspace }} completed successfully"

on_error:
  - action: notify-slack
    input:
      message: "Job {{ hook.task_name }} in {{ hook.workspace }} FAILED: {{ hook.error_message }}"

tasks:
  deploy:
    flow:
      step1:
        action: deploy-app
    # No on_success/on_error here — workspace-level hooks fire

  backup:
    flow:
      step1:
        action: backup-db
    on_error:
      - action: notify-slack
        input:
          message: "Backup failed — page oncall!"
    # Task-level on_error takes precedence; workspace on_error is skipped
    # Workspace on_success still fires (because task has no on_success)
```

### Rules

- `on_success` and `on_error` are evaluated independently. A task can override one while inheriting the other.
- Workspace hooks only fire for **top-level jobs** (source type `api` — programmatic calls, `user` — authenticated API calls, `trigger` — cron triggers, `webhook` — webhook triggers, or `mcp` — MCP tool invocations). Child jobs from `type: task` actions do not trigger workspace hooks.
- If multiple YAML files define workspace-level hooks, they are merged (extended, not replaced).
- The same `hook.*` template context and `secret.*` variables are available as in task-level hooks.

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
