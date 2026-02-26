---
title: API
description: Public REST API reference
---

Base URL: `http://localhost:8080` (configurable via `server-config.yaml`)

Task, job, and log endpoints do not require authentication unless the server is configured with an `auth` section.

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
[
  {
    "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "workspace": "default",
    "task_name": "hello-world",
    "mode": "distributed",
    "status": "completed",
    "source_type": "api",
    "source_id": null,
    "created_at": "2025-02-10T12:00:00Z",
    "started_at": "2025-02-10T12:00:01Z",
    "completed_at": "2025-02-10T12:00:03Z"
  }
]
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
  "input": { "name": "World" },
  "steps": [
    {
      "step_name": "say-hello",
      "action_name": "greet",
      "action_type": "shell",
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

**Step statuses:** `pending`, `ready`, `running`, `completed`, `failed`, `skipped`

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

Returns all registered users with their authentication methods. Supports `limit` and `offset` query parameters.

## Get User Detail

```
GET /api/users/{id}
```

Returns a single user's info including authentication methods.

## List Workers

```
GET /api/workers
```

Returns all registered workers with status and tags. Supports `limit` and `offset` query parameters.

## Get Worker Detail

```
GET /api/workers/{id}
```

Returns a single worker's info along with its recent jobs (up to 50).

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
| `500` | Internal server error |
