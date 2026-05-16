---
title: API
description: Public REST API reference
---

Base URL: `http://localhost:8080` (configurable via `server-config.yaml`)

Task, job, and log endpoints do not require authentication unless the server is configured with an `auth` section. When ACL is configured, list endpoints (tasks, jobs, workspaces) are filtered based on user permissions. See [Authorization](/operations/authorization/) for details.

## List Workspaces

```
GET /api/workspaces
```

Returns all configured workspaces with task and action counts.

**Response:**

```json
[
  {
    "name": "default",
    "tasks_count": 3,
    "actions_count": 5,
    "triggers_count": 1,
    "revision": "abc123"
  }
]
```

## Refresh Workspace

```
POST /api/workspaces/{ws}/refresh
```

Force-reload a workspace from its source (git fetch or folder re-hash), bypassing the normal poll interval. Use this from CI pipelines to ensure the workspace is up-to-date before triggering a task.

**Response:**

```json
{
  "workspace": "default",
  "revision": "abc123def456",
  "refreshed": true
}
```

**Errors:**

| Status | Reason |
|--------|--------|
| 404 | Workspace not found |
| 500 | Reload failed (e.g., git fetch error) |

**Example — GitHub Actions workflow:**

```bash
# Ensure Strøm has the latest code before executing
curl -X POST https://stroem.example.com/api/workspaces/default/refresh \
  -H "Authorization: Bearer $STROEM_API_KEY"

# Then trigger the task
curl -X POST https://stroem.example.com/api/workspaces/default/tasks/deploy/execute \
  -H "Authorization: Bearer $STROEM_API_KEY"
```

## List Tasks

```
GET /api/workspaces/{ws}/tasks
```

Returns all tasks from the specified workspace.

| Parameter | Description |
|-----------|-------------|
| `ws` | Workspace name |

**Response:**

```json
[
  {
    "name": "hello-world",
    "workspace": "default",
    "mode": "distributed",
    "has_triggers": true
  }
]
```

The `folder` field is included when set on the task. `has_triggers` is `true` when at least one enabled trigger targets this task.

## Get Task Detail

```
GET /api/workspaces/{ws}/tasks/{name}
```

| Parameter | Description |
|-----------|-------------|
| `ws` | Workspace name |
| `name` | Task name |

**Response:**

```json
{
  "name": "hello-world",
  "mode": "distributed",
  "input": {
    "name": { "type": "string", "default": "World" }
  },
  "flow": {
    "say-hello": {
      "action": "greet",
      "input": { "name": "{{ input.name }}" }
    }
  },
  "triggers": [
    {
      "name": "nightly",
      "type": "scheduler",
      "cron": "0 0 2 * * *",
      "task": "hello-world",
      "enabled": true,
      "input": {},
      "next_runs": ["2026-02-19T02:00:00+00:00"]
    }
  ]
}
```

The `triggers` array contains all triggers targeting this task. Scheduler triggers include `next_runs` with the next 5 upcoming fire times.

## Task Duration Stats

```
GET /api/workspaces/{ws}/tasks/{name}/stats?limit=50
```

Aggregated duration percentiles over the most recent **completed** runs of a task. Used by the UI to render the per-task duration insights card, the running-job ETA pill, and per-step p50 comparisons.

| Parameter | Description |
|-----------|-------------|
| `ws` | Workspace name |
| `name` | Task name |
| `limit` (query) | Number of most-recent completed runs to aggregate. Default `50`, clamped to `[1, 500]`. |

Failed, cancelled, and skipped runs are excluded — they would distort the distribution. `for_each` instance step rows (`step[0]`, `step[1]`, …) are also excluded from the per-step breakdown; the placeholder row already spans the whole loop, which is what you want for ETA.

ACL: `View` permission is sufficient.

**Response:**

```json
{
  "window": 50,
  "task": {
    "sample_size": 47,
    "avg_ms": 5230.4,
    "p50_ms": 4800.1,
    "p95_ms": 12100.8,
    "min_ms": 2100.0,
    "max_ms": 18000.0,
    "recent": [
      { "job_id": "...", "duration_ms": 4900.0, "completed_at": "2026-05-14T09:31:00Z" }
    ]
  },
  "steps": [
    {
      "step_name": "build",
      "sample_size": 47,
      "avg_ms": 1900.0,
      "p50_ms": 1820.0,
      "p95_ms": 4100.0,
      "min_ms": 800.0,
      "max_ms": 6200.0
    }
  ]
}
```

All percentile and aggregate fields are `null` when `sample_size` is `0`. Clients should treat percentiles as untrustworthy below ~5 samples and surface "insufficient data" instead.

## Execute Task

```
POST /api/workspaces/{ws}/tasks/{name}/execute
```

Creates a new job for the given task.

| Parameter | Description |
|-----------|-------------|
| `ws` | Workspace name |
| `name` | Task name |

**Request body:**

```json
{
  "input": { "name": "World" }
}
```

**Response:**

```json
{
  "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

## List Triggers

```
GET /api/workspaces/{ws}/triggers
```

Returns all triggers in the workspace with upcoming fire times for cron-based triggers.

**Response:**

```json
[
  {
    "name": "nightly",
    "type": "scheduler",
    "cron": "0 0 2 * * *",
    "task": "nightly-backup",
    "enabled": true,
    "input": { "env": "production" },
    "next_runs": ["2026-02-19T02:00:00+00:00"]
  }
]
```

## List Jobs

```
GET /api/jobs
```

| Query parameter | Default | Description |
|----------------|---------|-------------|
| `workspace` | — | Filter by workspace name |
| `task_name` | — | Filter by task name (requires `workspace`) |
| `limit` | `50` | Number of jobs to return |
| `offset` | `0` | Pagination offset |

**Response:**

```json
{
  "items": [
    {
      "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "workspace": "default",
      "task_name": "hello-world",
      "mode": "distributed",
      "status": "completed",
      "source_type": "api",
      "source_id": null,
      "revision": "abc123def456",
      "created_at": "2025-02-10T12:00:00Z",
      "started_at": "2025-02-10T12:00:01Z",
      "completed_at": "2025-02-10T12:00:03Z"
    }
  ],
  "total": 1
}
```

**Source types:** `"api"`, `"user"`, `"trigger"`, `"webhook"`, `"hook"`, `"task"`

**Job statuses:** `pending`, `running`, `completed`, `failed`, `cancelled`

## Get Job Detail

```
GET /api/jobs/{id}
```

Returns job metadata and all steps with statuses.

| Parameter | Description |
|-----------|-------------|
| `id` | Job ID (UUID) |

**Response:**

```json
{
  "job_id": "a1b2c3d4-...",
  "workspace": "default",
  "task_name": "hello-world",
  "status": "completed",
  "revision": "abc123def456",
  "input": { "name": "World" },
  "steps": [
    {
      "step_name": "say-hello",
      "action_name": "greet",
      "action_type": "script",
      "runner": "local",
      "input": { "name": "World" },
      "output": { "greeting": "Hello World" },
      "status": "completed",
      "worker_id": "w1w2w3w4-...",
      "started_at": "2025-02-10T12:00:01Z",
      "completed_at": "2025-02-10T12:00:02Z",
      "error_message": null
    }
  ]
}
```

**Step statuses:** `pending`, `ready`, `running`, `completed`, `failed`, `skipped`, `cancelled`

## Cancel Job

```
POST /api/jobs/{id}/cancel
```

Cancels a pending or running job. Running steps are actively killed (processes terminated, containers stopped, pods deleted). Pending steps are marked as cancelled. Child jobs are recursively cancelled. Fires `on_error` hooks.

| Parameter | Description |
|-----------|-------------|
| `id` | Job ID (UUID) |

**Response (200):**

```json
{
  "status": "cancelled"
}
```

**Error responses:**

| Status | Description |
|--------|-------------|
| `404` | Job not found |
| `409` | Job is already in a terminal state (completed, failed, or cancelled) |

**CLI:**

```bash
stroem cancel <job_id>
```

## Get Job Logs

```
GET /api/jobs/{id}/logs
```

Returns combined log output from all steps in JSONL format.

**Response:**

```json
{
  "logs": "{\"ts\":\"...\",\"stream\":\"stdout\",\"step\":\"say-hello\",\"line\":\"Hello World\"}\n"
}
```

## Get Step Logs

```
GET /api/jobs/{id}/steps/{step}/logs
```

Returns JSONL log output for a specific step. Use `_server` as the step name for server-side events.

## Stream Job Logs (WebSocket)

```
GET /api/jobs/{id}/logs/stream
```

Opens a WebSocket connection for real-time log streaming. Sends existing content (backfill) on connect, then streams new chunks.

```bash
websocat ws://localhost:8080/api/jobs/JOB_ID/logs/stream
```

## List Users

```
GET /api/users
```

Returns registered users with their authentication methods. Supports `limit` and `offset` query parameters. Response: `{ "items": [...], "total": N }`.

## Get User Detail

```
GET /api/users/{id}
```

Returns a single user's info including authentication methods and groups.

## User Management (Admin Only)

The following endpoints require admin privileges (`is_admin: true`). Non-admin users receive a `403` Forbidden response.

### List Users

```
GET /api/users
```

Returns registered users with pagination.

| Query parameter | Default | Description |
|----------------|---------|-------------|
| `limit` | `20` | Number of users to return |
| `offset` | `0` | Pagination offset |

**Response:**

```json
{
  "items": [
    {
      "id": "user-uuid-...",
      "email": "alice@example.com",
      "is_admin": true,
      "groups": ["devops", "admin"]
    }
  ],
  "total": 1
}
```

### Get User Detail

```
GET /api/users/{id}
```

Returns a single user's info including admin flag and group memberships.

**Response:**

```json
{
  "id": "user-uuid-...",
  "email": "alice@example.com",
  "is_admin": true,
  "groups": ["devops"]
}
```

### Set Admin Flag

```
PUT /api/users/{id}/admin
```

Grants or revokes admin privileges.

**Request body:**

```json
{
  "is_admin": true
}
```

**Response (200):**

```json
{
  "is_admin": true
}
```

### Set User Groups

```
PUT /api/users/{id}/groups
```

Updates the user's group memberships.

**Request body:**

```json
{
  "groups": ["devops", "qa"]
}
```

**Response (200):**

```json
{
  "groups": ["devops", "qa"]
}
```

### List Groups

```
GET /api/groups
```

Returns all distinct group names in use.

**Response:**

```json
["admin", "devops", "qa"]
```

## List Workers

```
GET /api/workers
```

Returns registered workers with status and tags. Supports `limit` and `offset` query parameters. Response: `{ "items": [...], "total": N }`.

## Get Worker Detail

```
GET /api/workers/{id}
```

Returns a single worker's info along with its recent steps (up to 50).

**Response:**

```json
{
  "steps": {
    "items": [
      {
        "job_id": "...",
        "workspace": "...",
        "task_name": "...",
        "job_status": "...",
        "step_name": "...",
        "action_type": "...",
        "status": "...",
        "started_at": "...",
        "completed_at": "...",
        "error_message": null
      }
    ],
    "total": 42
  }
}
```

## Error responses

All endpoints return errors in a consistent format:

```json
{
  "error": "Description of what went wrong"
}
```

| Status | Description |
|--------|-------------|
| `400` | Bad request (invalid input, missing fields) |
| `401` | Unauthorized (missing or invalid token) |
| `404` | Not found (unknown task, job, or step) |
| `409` | Conflict (e.g., cancelling an already-terminal job) |
| `500` | Internal server error |
