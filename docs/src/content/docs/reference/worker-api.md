---
title: Worker API
description: Worker-to-server communication endpoint reference
---

All worker endpoints require authentication via the `Authorization` header:

```
Authorization: Bearer <worker_token>
```

The token is configured in `server-config.yaml` (`worker_token` field) and must match the `worker_token` in `worker-config.yaml`.

## Register Worker

```
POST /worker/register
```

Registers a worker and returns a unique worker ID. Called once on worker startup.

**Request body:**

```json
{
  "name": "worker-1",
  "capabilities": ["shell"],
  "tags": ["shell", "docker"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Worker display name |
| `capabilities` | string[] | Legacy capability list (used if `tags` not set) |
| `tags` | string[] | Worker tags for step routing (overrides `capabilities`) |

**Response:**

```json
{
  "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890"
}
```

## Heartbeat

```
POST /worker/heartbeat
```

Updates the worker's last-seen timestamp. Called periodically (every 30s). Also reactivates workers that were marked inactive.

**Request body:**

```json
{
  "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890"
}
```

## Claim Step

```
POST /worker/jobs/claim
```

Claims the next ready step that matches the worker's tags. Uses `SELECT FOR UPDATE SKIP LOCKED` for concurrency safety.

**Request body:**

```json
{
  "worker_id": "w1w2w3w4-...",
  "capabilities": ["shell"],
  "tags": ["shell", "docker"]
}
```

A step is only claimed if all of its `required_tags` are present in the worker's effective tag set.

**Response (step available):**

```json
{
  "job_id": "a1b2c3d4-...",
  "workspace": "default",
  "step_name": "say-hello",
  "action_name": "greet",
  "action_type": "shell",
  "action_image": null,
  "runner": "local",
  "action_spec": {
    "cmd": "echo Hello World",
    "env": {}
  },
  "input": { "name": "World" }
}
```

The `action_spec` contains the fully resolved action definition with templates already rendered. The `runner` field indicates how to execute: `local`, `docker`, `pod`, or `none` (Type 1 container).

**Response (no work):**

```json
{
  "job_id": null,
  "step_name": null,
  "action_spec": null
}
```

## Report Step Start

```
POST /worker/jobs/{id}/steps/{step}/start
```

Marks a step as actively running.

**Request body:**

```json
{
  "worker_id": "w1w2w3w4-..."
}
```

## Report Step Complete

```
POST /worker/jobs/{id}/steps/{step}/complete
```

Reports step completion or failure. Triggers the orchestrator to promote dependent steps.

**Request body (success):**

```json
{
  "output": { "greeting": "Hello World" },
  "exit_code": 0,
  "error": null
}
```

**Request body (failure):**

```json
{
  "output": null,
  "exit_code": 1,
  "error": "Command exited with code 1"
}
```

When a step completes, the orchestrator checks downstream dependencies. When a step fails, dependent steps are skipped (unless they have `continue_on_failure: true`).

## Push Logs

```
POST /worker/jobs/{id}/logs
```

Appends structured log lines to the job's JSONL log file. Called periodically (~1s) during step execution.

**Request body:**

```json
{
  "step_name": "say-hello",
  "lines": [
    {"ts": "2025-02-12T10:56:45.123Z", "stream": "stdout", "line": "Hello World"},
    {"ts": "2025-02-12T10:56:45.456Z", "stream": "stderr", "line": "warning: unused var"}
  ]
}
```

The server appends each line as a JSONL entry and broadcasts via WebSocket for live streaming.

## Download Workspace Tarball

```
GET /worker/workspace/{ws}.tar.gz
```

Downloads a workspace as a gzipped tar archive.

| Parameter | Description |
|-----------|-------------|
| `ws` | Workspace name |

**Headers:**
- `If-None-Match` — Revision ETag for conditional fetch

**Response headers:**
- `Content-Type: application/gzip`
- `X-Revision: {revision}`
- `ETag: "{revision}"`

| Status | Description |
|--------|-------------|
| `200` | Tarball returned |
| `304` | Not Modified (workspace unchanged) |
| `404` | Workspace not found |

## Complete Job (Local Mode)

```
POST /worker/jobs/{id}/complete
```

Marks an entire job as completed. Used in local execution mode where the worker handles the full DAG.

**Request body:**

```json
{
  "output": { "result": "success" }
}
```
