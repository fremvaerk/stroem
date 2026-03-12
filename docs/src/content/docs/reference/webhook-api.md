---
title: Webhook API
description: Webhook trigger HTTP endpoint reference
---

Webhook endpoints are served at `/hooks/` (outside `/api/`) and do **not** require user authentication. Per-trigger authentication is handled via an optional `secret` field.

## Trigger Webhook

```
POST /hooks/{name}
GET  /hooks/{name}
```

Fires a webhook trigger by name, creating a new job for the associated task.

| Parameter | Description |
|-----------|-------------|
| `name` | Webhook trigger name (as defined in trigger YAML) |

### Authentication

If the webhook trigger has a `secret` configured, the caller must provide it via:
- Query parameter: `?secret=whsec_abc123`
- Authorization header: `Authorization: Bearer whsec_abc123`

Webhooks without a `secret` are public and accept any request.

### Request body (POST)

The body is included in the job input. JSON bodies (with `Content-Type: application/json`) are parsed; other content types are passed as a raw string.

### Response

```json
{
  "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "trigger": "github-push",
  "task": "ci-pipeline"
}
```

### Input structure

The webhook handler builds a structured input object:

```json
{
  "body": { "ref": "refs/heads/main", "commits": [] },
  "headers": { "content-type": "application/json", "x-github-event": "push" },
  "method": "POST",
  "query": { "env": "production" },
  "environment": "staging"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `body` | object/string/null | Parsed JSON, raw string, or `null` (GET) |
| `headers` | object | All request headers with lowercase keys |
| `method` | string | `"GET"` or `"POST"` |
| `query` | object | Query parameters (excluding `secret`) |

Trigger YAML `input` defaults merge at the top level. Reserved keys (`body`, `headers`, `method`, `query`) are not overwritten by defaults.

### Error responses

| Status | Description |
|--------|-------------|
| `401` | Missing or invalid secret |
| `404` | Webhook not found or disabled |

### Examples

```bash
# POST with JSON body and secret
curl -X POST http://localhost:8080/hooks/github-push?secret=whsec_abc123 \
  -H "Content-Type: application/json" \
  -d '{"ref": "refs/heads/main", "commits": []}'

# POST with secret via header
curl -X POST http://localhost:8080/hooks/github-push \
  -H "Authorization: Bearer whsec_abc123" \
  -H "Content-Type: application/json" \
  -d '{"ref": "refs/heads/main"}'

# GET (public webhook, no secret)
curl http://localhost:8080/hooks/health-check

# GET with query parameters
curl "http://localhost:8080/hooks/health-check?env=production&region=eu"
```

### Webhook name uniqueness

If multiple workspaces define a webhook with the same name, the first match wins at dispatch time.

## Check Job Status

```
GET /hooks/{name}/jobs/{job_id}
```

Check the status of a job that was created by this webhook trigger. Uses the same authentication as the trigger itself.

| Parameter | Description |
|-----------|-------------|
| `name` | Webhook trigger name |
| `job_id` | Job ID returned from the trigger response |

### Authentication

Same as the trigger endpoint — provide the `secret` via query parameter or `Authorization: Bearer` header if the webhook has a secret configured.

### Query parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `secret` | — | Webhook secret (if configured) |
| `wait` | `false` | Set to `true` to block until the job reaches a terminal state |
| `timeout` | `30` | Maximum wait time in seconds when `wait=true` (max: 300) |

### Response

```json
{
  "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "trigger": "github-push",
  "task": "ci-pipeline",
  "status": "completed",
  "output": { "result": "ok" },
  "created_at": "2026-03-12T10:30:00+00:00",
  "completed_at": "2026-03-12T10:30:05+00:00"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `job_id` | string | The job identifier |
| `trigger` | string | Webhook trigger name |
| `task` | string | Target task name |
| `status` | string | `pending`, `running`, `completed`, `failed`, or `cancelled` |
| `output` | object/null | Job output (only populated for completed jobs) |
| `created_at` | string | ISO 8601 timestamp of job creation |
| `completed_at` | string/null | ISO 8601 timestamp of job completion (null if still running) |

**HTTP status codes:**
- `200 OK` — Job found (or wait completed with terminal status)
- `202 Accepted` — Wait timed out, job still running
- `400 Bad Request` — Invalid job ID format
- `401 Unauthorized` — Missing or invalid secret
- `404 Not Found` — Webhook not found, or job was not created by this webhook

### Security

This endpoint only returns jobs with `source_type = "webhook"` and a matching `source_id` for the trigger. It cannot be used to query arbitrary jobs.

### Examples

```bash
# Check status (secret via query param)
curl "http://localhost:8080/hooks/github-push/jobs/a1b2c3d4?secret=whsec_abc123"

# Check status (secret via header)
curl http://localhost:8080/hooks/github-push/jobs/a1b2c3d4 \
  -H "Authorization: Bearer whsec_abc123"

# Wait for completion with 60s timeout
curl "http://localhost:8080/hooks/github-push/jobs/a1b2c3d4?secret=whsec_abc123&wait=true&timeout=60"

# Public webhook (no secret needed)
curl http://localhost:8080/hooks/health-check/jobs/a1b2c3d4
```
