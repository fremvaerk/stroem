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
