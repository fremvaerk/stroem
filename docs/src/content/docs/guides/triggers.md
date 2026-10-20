---
title: Triggers
description: Cron schedules and webhook endpoints for automated task execution
---

Triggers define automated task execution. Two types are supported: `scheduler` (cron-based) and `webhook` (HTTP-triggered).

:::note[Disabling all triggers of a workspace on one server]
Every trigger has its own `enabled` flag (in the workspace YAML, so it applies wherever the repository is loaded). To stop a whole workspace's triggers on a single server (for example a staging server that mirrors production), set `triggers: false` on that workspace in the **server** config instead — see [Multi-Workspace → Disabling triggers per server](/guides/multi-workspace/#disabling-triggers-per-server). Cron, webhook, and event-source triggers are all affected; tasks stay runnable manually.
:::

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

A trigger's `task` may name another workspace's task (`ws.task`), and may
carry a `ref` — see [Git Refs](/guides/git-refs/). Such a trigger creates its
job in the task's owner workspace (at that commit, with `ref`). The ref is
resolved when the trigger fires, before the concurrency policy; a fire that
cannot resolve its task or ref is logged as `Trigger '…' MISSED: …` and has
no other effect (no `cancel_previous`, no skipped row). The concurrency
policy and `source_id` stay keyed on the **defining** workspace's trigger,
and `triggers: false` on the defining workspace still suppresses it.

### Last run and missed runs

Every trigger shows its **last run**. A scheduler trigger can also show **missed runs**. Both appear on the workspace page, on the task page and on the Dashboard.

**Last run.** A trigger's runs are the jobs it created: `source_type: "trigger"` for a scheduler, `"webhook"` for a webhook. Jobs that someone started by hand are not runs of the trigger. This includes a manual run of the same task, a Re-run and a Restart.

- The time links to the job. The result is the job's status: `pending`, `running`, `completed`, `failed`, `cancelled` or `skipped`.
- If the run has [task retry](/guides/retry/) jobs, the result is the status of the newest retry job. A nightly run that failed once and passed on retry shows `completed`. The time still links to the trigger's own run.
- With `concurrency: skip`, every fire that finds the previous job still running writes a `skipped` row. The last run ignores those rows. Only a trigger with no other run shows a `skipped` last run.
- `never` means the trigger has no run in the database.
- `no access` means the run exists, but you may not view that job. The job is checked on its own task and folder, so a cross-workspace `ws.task` target or a pinned `ref:` job can be hidden even when the trigger is visible.

**Missed runs.** For an enabled scheduler trigger, a run is missed when a scheduled fire time:

- is later than the trigger's newest job of any status (a `skipped` row counts, because it proves the scheduler fired), and
- is at least 5 minutes in the past, and
- has no job.

The UI shows the number of missed runs and the first missed time. The count stops at 1000 and shows `1000+`. The 5 minute grace period lets a fire that is late by seconds pass. A small clock difference between the server and the database can record a fire a few milliseconds before its scheduled time, so the first scheduled time that is at most 5 seconds after the newest job is not counted (at most one time is skipped this way). For a schedule that fires every 5 seconds or more often, one missed fire right after the newest job may therefore not be counted. A trigger with no job at all never shows a mark, because the server cannot know when the trigger was added. A disabled trigger and a trigger in a workspace with `triggers: false` never show a mark. Webhook and event source triggers have no schedule, so they show a last run but never a missed mark.

The mark clears only when the trigger itself runs again. A manual run, a Re-run or a Restart of the task does not clear it, and does not change the last result.

**Where it shows.**

- Workspace page: a "Last run" column in the Triggers table. A warning box above the table lists the triggers with missed runs.
- Task page: the same last run and missed mark on each trigger of the task.
- Dashboard: a card "Triggers that need attention". It lists triggers that have missed runs or whose last result is `failed`, missed first. It hides when no trigger needs attention, and when the request fails. `cancelled` and `skipped` results do not count. Disabled triggers never appear. API: `GET /api/triggers/attention`.

A trigger appears only if its task is not denied for you by [ACL](/operations/authorization/). If the server cannot load your ACL data, `GET /api/workspaces/{ws}/triggers` now returns an error. Before, it listed every trigger.

**Limitations.**

- The missed rule uses the trigger's current cron and timezone. After you change either, the mark can be wrong until the trigger fires once on the new schedule.
- A trigger that you re-enable after `enabled: false` (or a workspace that you switch back from `triggers: false`) shows the fire times it skipped as missed, until it fires again. The database does not record when it was enabled.
- Job retention (`retention.job_days`) can delete the last run of a rarely firing trigger. The trigger then shows `never` and no mark.
- A trigger that you rename in YAML is a new trigger. It shows `never` until it fires.
- The Dashboard does not list triggers of a workspace that is in load error. They appear again after the workspace loads, with the runs that were missed during the outage.
- A failed run that you already handled by hand still shows as failed until the trigger runs again.

Run status needs two indexes on `job`. See [Migration 053](/operations/migration-053/) for the manual pre-run on a large database.

### Concurrency policy

Control what happens when a trigger fires while a previous run is still active:

```yaml
triggers:
  hourly-etl:
    type: scheduler
    cron: "0 * * * *"
    task: etl-pipeline
    concurrency: skip        # skip | allow | cancel_previous
```

| Policy | Behavior |
|--------|----------|
| `allow` | (default) Always create a new job, even if previous runs are still active. |
| `skip` | Skip the trigger if there is already an active job from this trigger. Creates a job record with `status: skipped` for visibility (no steps are executed). |
| `cancel_previous` | Cancel all active jobs from this trigger, then create a new job. |

The concurrency check uses the trigger's `source_type` and `source_id` (`"{workspace}/{trigger_name}"`) to identify related jobs.

### Timezone support

Scheduler triggers support optional timezone specification to run cron expressions in a specific timezone rather than UTC:

```yaml
triggers:
  nightly-build:
    type: scheduler
    cron: "0 2 * * *"
    timezone: "Europe/Copenhagen"    # Runs at 2:00 AM Copenhagen time
    task: build
```

- **`timezone` field**: Optional IANA timezone name (e.g., `"America/New_York"`, `"Australia/Sydney"`, `"Europe/London"`)
- **Default**: UTC when not specified
- **DST handling**: Daylight Saving Time transitions are handled automatically:
  - Spring-forward gaps (time jumps ahead): the trigger fires at the first valid time after the gap
  - Fall-back overlaps (time repeats): the trigger fires once at the first occurrence
- **Timezone names**: Use standard IANA names from the [IANA Time Zone Database](https://www.iana.org/time-zones) (case-sensitive)

Common timezone examples:
- `"America/New_York"` — Eastern Time (ET)
- `"America/Los_Angeles"` — Pacific Time (PT)
- `"Europe/London"` — Greenwich Mean Time (GMT)
- `"Europe/Paris"` — Central European Time (CET)
- `"Asia/Tokyo"` — Japan Standard Time (JST)
- `"Australia/Sydney"` — Australian Eastern Time (AEST)

### Scheduler fields

| Field | Required | Description |
|-------|----------|-------------|
| `type` | Yes | `scheduler` |
| `cron` | Yes | Cron expression (5 or 6 fields) |
| `task` | Yes | Name of the task to execute. May be another workspace's task (`ws.task`) |
| `ref` | No | Branch, tag or full commit SHA of the task's owner workspace to run the task from — see [Git Refs](/guides/git-refs/) |
| `input` | No | Input values passed to the task |
| `timezone` | No | IANA timezone name (default: `"UTC"`). Example: `"Europe/Copenhagen"` |
| `concurrency` | No | What to do when previous runs are active: `allow` (default), `skip`, `cancel_previous` |
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
- With `force_refresh: true`, the caller is authenticated against the
  definition the server has loaded **before** the workspace is reloaded, so
  an unauthenticated caller can never trigger a git fetch. After the reload
  the webhook is matched again (gone → `404`) and the caller is
  authenticated again against the refreshed `secret`; the job is created
  from the refreshed definition, so a changed `task` or `ref` takes effect.
  A secret that was rotated in git is therefore accepted only once the
  server has loaded it (its watcher poll, or an earlier refresh). A reload
  that leaves the workspace unavailable answers `500`.
- An unknown task, a bad or missing `ref` and similar configuration errors
  answer `400`; a transient problem (for example, the git server is
  unreachable) answers `500`.

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
| `task` | Yes | Name of the task to execute. May be another workspace's task (`ws.task`) |
| `ref` | No | Branch, tag or full commit SHA of the task's owner workspace to run the task from — see [Git Refs](/guides/git-refs/) |
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

The `output` of a sync response, and of the job-status poll below, is
masked like job detail: workspace secrets (and the secrets of any
[pinned commit](/guides/git-refs/#secrets-and-redaction) the job is connected
to) appear as `••••••`. If those secrets cannot be loaded at the moment, the
answer is `503` with `{"job_id": "...", "error": "redaction set unavailable,
retry"}` — the job exists; poll it again later.

### Checking async job status

When using async webhooks, the response includes a `job_id`. You can check the job status using the same webhook endpoint — no API key or JWT required:

```bash
# Trigger the webhook (async)
curl -X POST http://localhost:8080/hooks/my-webhook?secret=whsec_abc123 \
  -H "Content-Type: application/json" \
  -d '{"ref": "main"}'
# Response: {"job_id":"abc-123","trigger":"my-webhook","task":"deploy"}

# Check job status (same secret)
curl http://localhost:8080/hooks/my-webhook/jobs/abc-123?secret=whsec_abc123
# Response: {"job_id":"abc-123","trigger":"my-webhook","task":"deploy","status":"running","output":null}

# Wait for completion (blocks until done or timeout)
curl "http://localhost:8080/hooks/my-webhook/jobs/abc-123?secret=whsec_abc123&wait=true&timeout=60"
# Response: {"job_id":"abc-123","trigger":"my-webhook","task":"deploy","status":"completed","output":{...}}
```

The status endpoint only returns jobs created by that specific webhook trigger — it cannot be used to query arbitrary jobs.

See [Webhook API](/reference/webhook-api/) for the full endpoint reference.

### Webhook name uniqueness

Webhook names should be unique across all workspaces. If the same name appears in multiple workspaces, the first match wins at dispatch time.

### Last run

A webhook trigger shows its last run (the newest job it created, with the result) on the workspace page, the task page and the Dashboard. It never shows a missed mark, because a webhook has no schedule. See [Last run and missed runs](#last-run-and-missed-runs).

## Event source triggers

For long-running queue consumers that create jobs from external events, see the dedicated [Event Sources](/stroem/guides/event-sources/) guide.

Event sources reference a consumer task and a target task:

```yaml
triggers:
  sqs-events:
    type: event_source
    task: sqs-consumer         # consumer task (runs continuously)
    target_task: process-order # where OUTPUT: lines create jobs
    restart_policy: always
    max_in_flight: 10
```

The consumer task runs as a regular job. Each `OUTPUT: ` line emitted by the consumer is parsed as JSON and creates a new job for the target task. This pattern is ideal for queue-driven architectures where you need continuous consumption with backpressure support.

An event source trigger shows a last run too, but never a missed mark. Its last run is the newest **consumer** job, not a job that the consumer emitted. The server finds consumer jobs by the `_fingerprint` marker in the job input. A running consumer shows "running since" its start time. See [Last run and missed runs](#last-run-and-missed-runs).
