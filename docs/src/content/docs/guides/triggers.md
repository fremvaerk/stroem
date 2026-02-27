---
title: Triggers
description: Cron schedules and webhook endpoints for automated task execution
---

Triggers define automated task execution. Two types are supported: `scheduler` (cron-based) and `webhook` (HTTP-triggered).

## Cron scheduler

```yaml
triggers:
  nightly-backup:
    type: scheduler
    cron: "0 0 2 * * *"
    task: backup-db
    input:
      retention_days: 30
    enabled: true
```

### Cron expressions

The `cron` field supports both standard 5-field (minute granularity) and extended 6-field (second granularity) expressions:

```
# 5-field: minute hour day-of-month month day-of-week
cron: "0 2 * * *"           # Every day at 2:00 AM

# 6-field: second minute hour day-of-month month day-of-week
cron: "0 0 2 * * *"         # Every day at 2:00:00 AM
cron: "*/10 * * * * *"      # Every 10 seconds
cron: "0 30 9 * * MON-FRI"  # Weekdays at 9:30 AM
```

Extended features (via the `croner` library):
- `L` — last day of month or last weekday occurrence (`5L` = last Friday)
- `#` — nth weekday (`5#2` = second Friday of the month)
- `W` — closest weekday to a day (`15W` = closest weekday to the 15th)
- Text names — `MON`, `TUE`, `JAN`, `FEB`, etc.

### How the scheduler works

- The scheduler runs inside the server process and wakes only when a trigger is due (no fixed polling interval).
- When workspace configs are hot-reloaded, the scheduler picks up new/changed/removed triggers automatically.
- If a trigger's cron expression changes, its next fire time is recalculated. If unchanged, the existing schedule is preserved.
- Jobs created by triggers have `source_type: "trigger"` and `source_id: "{workspace}/{trigger_name}"` for audit trail.
- If the server was down when a trigger was due, it fires on the next startup.

### Scheduler fields

| Field | Required | Description |
|-------|----------|-------------|
| `type` | Yes | `scheduler` |
| `cron` | Yes | Cron expression (5 or 6 fields) |
| `task` | Yes | Name of the task to execute |
| `input` | No | Input values passed to the task |
| `enabled` | No | Whether the trigger is active (default: `true`) |

## Webhook triggers

Webhook triggers expose an HTTP endpoint that external systems (GitHub, GitLab, monitoring tools) can call to trigger tasks.

```yaml
triggers:
  on-push:
    type: webhook
    name: github-push          # URL-safe name — endpoint: POST /hooks/github-push
    task: ci-pipeline
    secret: "whsec_abc123"     # Optional — omit for public webhooks
    input:                     # Optional — default values merged into request input
      environment: staging
    enabled: true

  deploy-sync:
    type: webhook
    name: deploy
    task: do-deploy
    mode: sync                 # Wait for job completion before responding
    timeout_secs: 60           # Max wait time (default: 30, max: 300)
    secret: "whsec_deploy"
```

### Calling a webhook

```bash
# POST with JSON body and secret via query param
curl -X POST http://localhost:8080/hooks/github-push?secret=whsec_abc123 \
  -H "Content-Type: application/json" \
  -d '{"ref": "refs/heads/main", "commits": []}'

# GET with secret via Authorization header
curl http://localhost:8080/hooks/github-push \
  -H "Authorization: Bearer whsec_abc123"

# Public webhook (no secret configured) — no auth needed
curl -X POST http://localhost:8080/hooks/public-hook \
  -H "Content-Type: application/json" \
  -d '{"event": "deploy"}'
```

### Authentication

- If the trigger has a `secret` field, callers must provide it via `?secret=xxx` query parameter or `Authorization: Bearer xxx` header.
- If no `secret` is configured, the webhook is public.
- Invalid or missing secrets return `401 Unauthorized`.

### Input structure

The webhook handler wraps the entire HTTP request into a structured input map:

```json
{
  "body": { "ref": "refs/heads/main", "commits": [] },
  "headers": { "content-type": "application/json", "x-github-event": "push" },
  "method": "POST",
  "query": { "env": "production" },
  "environment": "staging"
}
```

- `body`: JSON-parsed if `Content-Type: application/json`, raw string otherwise, `null` for GET
- `headers`: lowercase key map of all request headers
- `method`: `"GET"` or `"POST"`
- `query`: query parameters (the `secret` param is excluded)
- Trigger YAML `input` defaults merge at top level (don't overwrite `body`, `headers`, `method`, `query`)

### Webhook fields

| Field | Required | Description |
|-------|----------|-------------|
| `type` | Yes | `webhook` |
| `name` | Yes | URL-safe name (alphanumeric, hyphens, underscores) |
| `task` | Yes | Name of the task to execute |
| `secret` | No | Secret for authentication |
| `input` | No | Default input values merged with request data |
| `enabled` | No | Whether the trigger is active (default: `true`) |
| `mode` | No | `"async"` (default) or `"sync"` — sync waits for job completion |
| `timeout_secs` | No | Max wait time in sync mode (default: 30, max: 300) |

### Sync vs async mode

By default, webhooks return immediately with a `job_id` (async mode). The caller must poll `GET /api/jobs/{id}` to track completion.

In **sync mode** (`mode: sync`), the webhook handler blocks until the job reaches a terminal state (completed or failed) or the timeout is reached.

**Sync response** (job completed or failed):
```json
{
  "job_id": "...",
  "trigger": "deploy",
  "task": "do-deploy",
  "status": "completed",
  "output": { "result": "ok" }
}
```
HTTP status: `200 OK`.

**Timeout response** (job still running):
```json
{
  "job_id": "...",
  "trigger": "deploy",
  "task": "do-deploy",
  "status": "running"
}
```
HTTP status: `202 Accepted` — use the `job_id` to poll manually.

### Webhook name uniqueness

Webhook names should be unique across all workspaces. If the same name appears in multiple workspaces, the first match wins at dispatch time.

See [Webhook API](/reference/webhook-api/) for the full endpoint reference.
