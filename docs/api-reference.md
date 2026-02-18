# API Reference

Base URL: `http://localhost:8080` (configurable via `server-config.yaml`)

## Public API

Task, job, and log endpoints do not require authentication. Auth endpoints are available when the server is configured with an `auth` section (see Configuration below).

---

### List Workspaces

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
  },
  {
    "name": "data-team",
    "tasks_count": 2,
    "actions_count": 4,
    "triggers_count": 0,
    "revision": "def456"
  }
]
```

---

### List Tasks

```
GET /api/workspaces/{ws}/tasks
```

Returns all tasks from the specified workspace.

**Path parameters:**
- `ws` -- Workspace name (e.g., `default`)

**Response:**

```json
[
  {
    "name": "hello-world",
    "workspace": "default",
    "mode": "distributed",
    "has_triggers": true
  },
  {
    "name": "deploy-staging",
    "workspace": "default",
    "mode": "distributed",
    "folder": "deploy/staging",
    "has_triggers": false
  }
]
```

The `folder` field is omitted when not set on the task. `has_triggers` is `true` when at least one enabled trigger in the workspace targets this task.

---

### Get Task Detail

```
GET /api/workspaces/{ws}/tasks/{name}
```

**Path parameters:**
- `ws` -- Workspace name (e.g., `default`)
- `name` -- Task name (e.g., `hello-world`)

**Response:**

```json
{
  "name": "hello-world",
  "mode": "distributed",
  "folder": "deploy/staging",
  "input": {
    "name": { "type": "string", "default": "World" }
  },
  "flow": {
    "say-hello": {
      "action": "greet",
      "input": { "name": "{{ input.name }}" }
    },
    "shout-it": {
      "action": "shout",
      "depends_on": ["say-hello"],
      "input": { "message": "{{ say_hello.output.greeting }}" }
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
      "next_runs": [
        "2026-02-19T02:00:00+00:00",
        "2026-02-20T02:00:00+00:00",
        "2026-02-21T02:00:00+00:00",
        "2026-02-22T02:00:00+00:00",
        "2026-02-23T02:00:00+00:00"
      ]
    }
  ]
}
```

The `triggers` array contains all triggers in the workspace that target this task. Each trigger includes a `next_runs` array with the next 5 upcoming fire times (ISO 8601 timestamps) for `scheduler` type triggers; empty for other trigger types.

---

### Execute Task

```
POST /api/workspaces/{ws}/tasks/{name}/execute
```

Creates a new job for the given task in the specified workspace.

**Path parameters:**
- `ws` -- Workspace name
- `name` -- Task name

**Request body:**

```json
{
  "input": {
    "name": "World"
  }
}
```

The `input` object is matched against the task's input definition. Missing fields with defaults use the default value. Missing required fields cause an error.

**Response:**

```json
{
  "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

**Example:**

```bash
curl -X POST http://localhost:8080/api/workspaces/default/tasks/hello-world/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"name": "World"}}'
```

---

### List Triggers

```
GET /api/workspaces/{ws}/triggers
```

Returns all triggers defined in the specified workspace with upcoming fire times for cron-based triggers.

**Path parameters:**
- `ws` -- Workspace name (e.g., `default`)

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
    "next_runs": [
      "2026-02-19T02:00:00+00:00",
      "2026-02-20T02:00:00+00:00",
      "2026-02-21T02:00:00+00:00",
      "2026-02-22T02:00:00+00:00",
      "2026-02-23T02:00:00+00:00"
    ]
  },
  {
    "name": "on-push",
    "type": "webhook",
    "task": "deploy",
    "enabled": true,
    "input": {},
    "next_runs": []
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Trigger name |
| `type` | string | Trigger type (`scheduler`, `webhook`, etc.) |
| `cron` | string or null | Cron expression (only for `scheduler` type) |
| `task` | string | Target task name |
| `enabled` | boolean | Whether the trigger is active |
| `input` | object | Input parameters passed to the task when triggered |
| `next_runs` | string[] | Next 5 upcoming fire times as ISO 8601 timestamps. Populated for `scheduler` triggers with valid cron expressions; empty array for all other types. |

The `cron` field is omitted when not set on the trigger.

**Example:**

```bash
curl http://localhost:8080/api/workspaces/default/triggers
```

---

### List Jobs

```
GET /api/jobs
```

**Query parameters:**
- `workspace` (optional) -- Filter by workspace name
- `task_name` (optional) -- Filter by task name (requires `workspace`)
- `limit` (optional, default: `50`) -- Number of jobs to return
- `offset` (optional, default: `0`) -- Pagination offset

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

**Example:**

```bash
curl "http://localhost:8080/api/jobs?limit=10"
```

---

### Get Job Detail

```
GET /api/jobs/{id}
```

Returns job metadata and all steps with their statuses.

**Path parameters:**
- `id` -- Job ID (UUID)

**Response:**

```json
{
  "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "workspace": "default",
  "task_name": "hello-world",
  "mode": "distributed",
  "input": { "name": "World" },
  "output": null,
  "status": "completed",
  "source_type": "api",
  "source_id": null,
  "created_at": "2025-02-10T12:00:00Z",
  "started_at": "2025-02-10T12:00:01Z",
  "completed_at": "2025-02-10T12:00:03Z",
  "steps": [
    {
      "step_name": "say-hello",
      "action_name": "greet",
      "action_type": "shell",
      "action_image": null,
      "runner": "local",
      "input": { "name": "World" },
      "output": { "greeting": "Hello World" },
      "status": "completed",
      "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890",
      "started_at": "2025-02-10T12:00:01Z",
      "completed_at": "2025-02-10T12:00:02Z",
      "error_message": null
    },
    {
      "step_name": "shout-it",
      "action_name": "shout",
      "action_type": "shell",
      "action_image": null,
      "runner": "local",
      "input": { "message": "Hello World" },
      "output": null,
      "status": "completed",
      "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890",
      "started_at": "2025-02-10T12:00:02Z",
      "completed_at": "2025-02-10T12:00:03Z",
      "error_message": null
    }
  ]
}
```

**Source types:**
- `"api"` -- Job triggered via the REST API without authentication
- `"user"` -- Job triggered by an authenticated user (source_id contains their email)
- `"trigger"` -- Job created by the cron scheduler (source_id contains `"{workspace}/{trigger_name}"`, e.g. `"default/nightly-backup"`)

**Job statuses:** `pending`, `running`, `completed`, `failed`, `cancelled`

**Step statuses:** `pending`, `ready`, `running`, `completed`, `failed`, `skipped`

- `skipped` -- The step was not executed because a dependency failed (or was itself skipped). Steps with `continue_on_failure: true` are promoted instead of skipped when dependencies fail.

---

### Get Job Logs

```
GET /api/jobs/{id}/logs
```

Returns the combined log output from all steps of a job in JSONL format. Each line is a JSON object with `ts`, `stream`, `step`, and `line` fields.

**Path parameters:**
- `id` -- Job ID (UUID)

**Response:**

```json
{
  "logs": "{\"ts\":\"2025-02-12T10:56:45.123Z\",\"stream\":\"stdout\",\"step\":\"say-hello\",\"line\":\"Hello World\"}\n{\"ts\":\"2025-02-12T10:56:45.456Z\",\"stream\":\"stdout\",\"step\":\"say-hello\",\"line\":\"OUTPUT: {\\\"greeting\\\": \\\"Hello World\\\"}\"}\n"
}
```

**JSONL line format:**

| Field | Type | Description |
|-------|------|-------------|
| `ts` | string | ISO 8601 timestamp of when the line was captured |
| `stream` | string | `"stdout"` or `"stderr"` |
| `step` | string | Step name that produced this line |
| `line` | string | The log line content |

**Backward compatibility:** For jobs created before JSONL format was introduced, the `logs` field may contain legacy plain text. Clients should attempt to parse each line as JSON and fall back to rendering plain text for unparseable lines.

**Example:**

```bash
curl -s http://localhost:8080/api/jobs/JOB_ID/logs | jq -r .logs
```

---

### Get Step Logs

```
GET /api/jobs/{id}/steps/{step}/logs
```

Returns the JSONL log output for a specific step within a job. Lines are filtered by the `step` field in each JSON log entry.

**Path parameters:**
- `id` -- Job ID (UUID)
- `step` -- Step name

**Response:**

```json
{
  "logs": "{\"ts\":\"2025-02-12T10:56:45.123Z\",\"stream\":\"stdout\",\"step\":\"say-hello\",\"line\":\"Hello World\"}\n"
}
```

For jobs created before JSONL log format was introduced, this endpoint returns an empty string (legacy plain-text lines are skipped).

**Server events:** Use `_server` as the step name to retrieve server-side log entries (hook failures, orchestration errors, recovery timeouts). These are written automatically by the server when infrastructure errors affect a job.

**Example:**

```bash
# Step logs
curl -s http://localhost:8080/api/jobs/JOB_ID/steps/say-hello/logs | jq -r .logs

# Server events (hook errors, recovery timeouts, etc.)
curl -s http://localhost:8080/api/jobs/JOB_ID/steps/_server/logs | jq -r .logs
```

---

### List Users

```
GET /api/users
```

Returns all registered users with their authentication methods. Never exposes password hashes.

**Query parameters:**
- `limit` (optional, default: `50`) -- Number of users to return
- `offset` (optional, default: `0`) -- Pagination offset

**Response:**

```json
[
  {
    "user_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "name": "Alice",
    "email": "alice@example.com",
    "auth_methods": ["password", "google"],
    "created_at": "2025-02-10T12:00:00Z",
    "last_login_at": "2025-02-18T09:30:00Z"
  }
]
```

Users are sorted by creation time (newest first). `last_login_at` is `null` if the user has never logged in.

**Auth method values:** `password` (has password hash), or OIDC provider IDs (e.g., `google`, `github`)

**Example:**

```bash
curl "http://localhost:8080/api/users?limit=10"
```

---

### Get User Detail

```
GET /api/users/{id}
```

Returns a single user's info including authentication methods.

**Path parameters:**
- `id` -- User ID (UUID)

**Response:**

```json
{
  "user_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "name": "Alice",
  "email": "alice@example.com",
  "auth_methods": ["password", "google"],
  "created_at": "2025-02-10T12:00:00Z",
  "last_login_at": "2025-02-18T09:30:00Z"
}
```

**Error responses:**
- `400` -- Invalid UUID format
- `404` -- User not found

**Example:**

```bash
curl "http://localhost:8080/api/users/a1b2c3d4-e5f6-7890-abcd-ef1234567890"
```

---

### List Workers

```
GET /api/workers
```

Returns all registered workers with their current status and tags.

**Query parameters:**
- `limit` (optional, default: `50`) -- Number of workers to return
- `offset` (optional, default: `0`) -- Pagination offset

**Response:**

```json
[
  {
    "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890",
    "name": "worker-1",
    "status": "active",
    "tags": ["shell", "docker"],
    "last_heartbeat": "2025-02-10T12:05:00Z",
    "registered_at": "2025-02-10T12:00:00Z"
  }
]
```

Workers are sorted by status (active first), then by registration time (newest first).

**Worker statuses:** `active`, `inactive`

**Example:**

```bash
curl "http://localhost:8080/api/workers?limit=10"
```

---

### Get Worker Detail

```
GET /api/workers/{id}
```

Returns a single worker's info along with its recent jobs (up to 50).

**Path parameters:**
- `id` -- Worker ID (UUID)

**Response:**

```json
{
  "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890",
  "name": "worker-1",
  "status": "active",
  "tags": ["shell", "docker"],
  "last_heartbeat": "2025-02-10T12:05:00Z",
  "registered_at": "2025-02-10T12:00:00Z",
  "jobs": [
    {
      "job_id": "a1b2c3d4-...",
      "workspace": "default",
      "task_name": "hello-world",
      "mode": "distributed",
      "status": "completed",
      "source_type": "api",
      "source_id": null,
      "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890",
      "created_at": "2025-02-10T12:10:00Z",
      "started_at": "2025-02-10T12:10:01Z",
      "completed_at": "2025-02-10T12:10:05Z"
    }
  ]
}
```

**Error responses:**
- `400` -- Invalid UUID format
- `404` -- Worker not found

**Example:**

```bash
curl "http://localhost:8080/api/workers/w1w2w3w4-w5w6-7890-abcd-ef1234567890"
```

---

### Stream Job Logs (WebSocket)

```
GET /api/jobs/{id}/logs/stream
```

Opens a WebSocket connection for real-time log streaming. On connect, the server sends any existing log content (backfill), then streams new log chunks as they arrive from workers.

**Path parameters:**
- `id` -- Job ID (UUID)

**Protocol:** WebSocket (upgrade from HTTP)

**Behavior:**
1. Server sends existing JSONL log content as a text message (backfill)
2. Server forwards new JSONL log chunks as text messages in real-time
3. Connection stays open until the client disconnects or the server shuts down
4. Each message contains one or more JSONL lines (one JSON object per line)

**Example (websocat):**

```bash
websocat ws://localhost:8080/api/jobs/JOB_ID/logs/stream
```

**Error responses:**
- `400` -- Invalid job ID format

---

## Auth API

Auth endpoints are only available when the server is configured with an `auth` section. Without auth configuration, these endpoints return `404`.

---

### Login

```
POST /api/auth/login
```

Authenticates with email and password. Returns a JWT access token and a refresh token.

**Request body:**

```json
{
  "email": "admin@stroem.local",
  "password": "admin"
}
```

**Response:**

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIs...",
  "refresh_token": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

**Error responses:**
- `401` -- Invalid email or password
- `404` -- Auth not configured on this server

**Example:**

```bash
curl -X POST http://localhost:8080/api/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email": "admin@stroem.local", "password": "admin"}'
```

---

### Refresh Token

```
POST /api/auth/refresh
```

Exchanges a refresh token for a new access token and refresh token pair. The old refresh token is revoked (rotation).

**Request body:**

```json
{
  "refresh_token": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

**Response:**

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIs...",
  "refresh_token": "f1e2d3c4-b5a6-0987-dcba-0987654321fe"
}
```

**Error responses:**
- `401` -- Invalid or expired refresh token

---

### Logout

```
POST /api/auth/logout
```

Revokes a refresh token.

**Request body:**

```json
{
  "refresh_token": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

**Response:**

```json
{
  "status": "ok"
}
```

---

### Get Current User

```
GET /api/auth/me
```

Returns the authenticated user's information. Requires a valid JWT access token.

**Headers:**

```
Authorization: Bearer <access_token>
```

**Response:**

```json
{
  "user_id": "d1e2f3a4-b5c6-7890-abcd-ef1234567890",
  "name": null,
  "email": "admin@stroem.local",
  "created_at": "2025-02-11T10:00:00Z"
}
```

**Error responses:**
- `401` -- Missing or invalid access token

---

### Server Config

```
GET /api/config
```

Returns server configuration for the UI. This is a public endpoint (no auth required).

**Response:**

```json
{
  "auth_required": true,
  "has_internal_auth": true,
  "oidc_providers": [
    { "id": "google", "display_name": "Google" }
  ]
}
```

- `auth_required` -- Whether authentication is enabled on this server
- `has_internal_auth` -- Whether internal (email/password) authentication is available
- `oidc_providers` -- List of configured OIDC providers (empty if none configured)

---

### OIDC Login Start

```
GET /api/auth/oidc/{provider}
```

Initiates an OIDC Authorization Code + PKCE flow. Generates a PKCE challenge and CSRF state, stores them in a signed HttpOnly cookie, and redirects to the identity provider.

**Response:** `302` redirect to the identity provider's authorization endpoint.

**Error responses:**
- `404` -- Unknown OIDC provider

---

### OIDC Callback

```
GET /api/auth/oidc/{provider}/callback?code=AUTH_CODE&state=CSRF_STATE
```

Handles the callback from the identity provider. Validates state, exchanges the authorization code for tokens, validates the ID token, provisions the user (JIT), and issues internal JWT tokens.

On success: `302` redirect to `/login/callback#access_token=AT&refresh_token=RT`

On error: `302` redirect to `/login/callback#error=URL_ENCODED_MSG`

JIT user provisioning:
1. If an auth_link for this provider+external_id exists → return that user
2. If a user with the same email exists → create auth_link and return that user
3. Otherwise → create a new user (no password) + auth_link

---

## Worker API

All worker endpoints require authentication via the `Authorization` header:

```
Authorization: Bearer <worker_token>
```

The token is configured in `server-config.yaml` (`worker_token` field) and must match the `worker_token` in `worker-config.yaml`.

---

### Register Worker

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
| `tags` | string[] (optional) | Worker tags for step routing. Overrides `capabilities` when set. |

**Response:**

```json
{
  "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890"
}
```

---

### Heartbeat

```
POST /worker/heartbeat
```

Updates the worker's last-seen timestamp. Called periodically.

**Request body:**

```json
{
  "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890"
}
```

**Response:**

```json
{
  "status": "ok"
}
```

---

### Claim Step

```
POST /worker/jobs/claim
```

Claims the next ready step that matches the worker's capabilities. Uses `SELECT FOR UPDATE SKIP LOCKED` for concurrency safety.

**Request body:**

```json
{
  "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890",
  "capabilities": ["shell"],
  "tags": ["shell", "docker"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `worker_id` | string | Worker ID from registration |
| `capabilities` | string[] | Legacy capability list (used if `tags` not set) |
| `tags` | string[] (optional) | Worker tags for step matching. Overrides `capabilities` when set. |

A step is only claimed if all of its `required_tags` are present in the worker's effective tag set.

**Response (step available):**

```json
{
  "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "workspace": "default",
  "step_name": "say-hello",
  "action_name": "greet",
  "action_type": "shell",
  "action_image": null,
  "runner": "local",
  "action_spec": {
    "cmd": "echo Hello World && echo 'OUTPUT: {\"greeting\": \"Hello World\"}'",
    "env": {}
  },
  "input": {
    "name": "World"
  }
}
```

The `action_spec` contains the fully resolved action definition with templates already rendered against the job context. The `workspace` field tells the worker which workspace tarball to download. The `runner` field indicates how to execute the step: `local` (ShellRunner), `docker`/`pod` (container runner with workspace), or `none` (Type 1 container, image runs standalone).

**Response (no work available):**

```json
{
  "job_id": null,
  "workspace": null,
  "step_name": null,
  "action_name": null,
  "action_type": null,
  "action_image": null,
  "runner": null,
  "action_spec": null,
  "input": null
}
```

---

### Report Step Start

```
POST /worker/jobs/{id}/steps/{step}/start
```

Marks a step as actively running.

**Path parameters:**
- `id` -- Job ID (UUID)
- `step` -- Step name

**Request body:**

```json
{
  "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890"
}
```

**Response:**

```json
{
  "status": "ok"
}
```

---

### Report Step Complete

```
POST /worker/jobs/{id}/steps/{step}/complete
```

Reports step completion or failure. Triggers the orchestrator to promote dependent steps.

**Path parameters:**
- `id` -- Job ID (UUID)
- `step` -- Step name

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

**Response:**

```json
{
  "status": "ok"
}
```

When a step completes successfully, the orchestrator checks if any downstream steps now have all their dependencies met and promotes them to `ready` status. When a step fails, downstream steps that depend on it are marked `skipped` (unless they have `continue_on_failure: true`, in which case they are promoted). When all steps are in a terminal state, the job is marked `completed` or `failed` depending on the failures: if every failed step has `continue_on_failure: true`, the job is `completed` (with tolerable failures); if any failed step does not have the flag, the job is `failed`.

---

### Push Logs

```
POST /worker/jobs/{id}/logs
```

Appends structured log lines to the job's JSONL log file. Called periodically (~1s) during step execution.

**Path parameters:**
- `id` -- Job ID (UUID)

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

| Field | Type | Description |
|-------|------|-------------|
| `step_name` | string (optional) | Step name to attach to each log line |
| `lines` | array | Array of log line objects |
| `lines[].ts` | string | ISO 8601 timestamp |
| `lines[].stream` | string | `"stdout"` or `"stderr"` |
| `lines[].line` | string | The log line content |

The server converts each line into a JSONL entry with a `step` field added (from `step_name`), appends to the job's `.jsonl` file, and broadcasts via WebSocket.

**Response:**

```json
{
  "status": "ok"
}
```

---

### Download Workspace Tarball

```
GET /worker/workspace/{ws}.tar.gz
```

Downloads a workspace as a gzipped tar archive. Workers use this to get workspace files before executing steps.

**Path parameters:**
- `ws` -- Workspace name (e.g., `default`)

**Headers:**
- `If-None-Match` (optional) -- Revision ETag for conditional fetch. If the workspace hasn't changed, the server returns `304 Not Modified`.

**Response headers:**
- `Content-Type: application/gzip`
- `X-Revision: {revision}` -- The current workspace revision (content hash for folder sources, commit OID for git sources)
- `ETag: "{revision}"`

**Response:** Binary gzipped tar archive of the workspace directory.

**Status codes:**
- `200` -- Tarball returned
- `304` -- Not Modified (workspace unchanged since the revision in `If-None-Match`)
- `404` -- Workspace not found

**Example:**

```bash
# Download workspace tarball
curl -o workspace.tar.gz \
  -H "Authorization: Bearer <worker_token>" \
  http://localhost:8080/worker/workspace/default.tar.gz

# Conditional fetch (only download if changed)
curl -H "Authorization: Bearer <worker_token>" \
  -H 'If-None-Match: "abc123"' \
  http://localhost:8080/worker/workspace/default.tar.gz
```

---

### Complete Job (Local Mode)

```
POST /worker/jobs/{id}/complete
```

Marks an entire job as completed. Used in local execution mode where the worker handles the full DAG.

**Path parameters:**
- `id` -- Job ID (UUID)

**Request body:**

```json
{
  "output": { "result": "success" }
}
```

**Response:**

```json
{
  "status": "ok"
}
```

---

## Error Responses

All endpoints return errors in a consistent format:

```json
{
  "error": "Description of what went wrong"
}
```

Common HTTP status codes:
- `400` -- Bad request (invalid input, missing fields)
- `401` -- Unauthorized (missing or invalid token)
- `404` -- Not found (unknown task, job, or step)
- `500` -- Internal server error

---

## Configuration

### Auth (optional)

Add an `auth` section to `server-config.yaml` to enable authentication:

```yaml
auth:
  jwt_secret: "your-jwt-secret"
  refresh_secret: "your-refresh-secret"
  providers:
    internal:
      provider_type: internal
  initial_user:
    email: admin@stroem.local
    password: admin
```

- **jwt_secret**: Secret used to sign JWT access tokens (15-minute TTL)
- **refresh_secret**: Secret used for refresh token operations (30-day TTL, rotation on use)
- **providers**: Authentication providers. Currently only `internal` (email/password) is supported. OIDC is planned.
- **initial_user** (optional): Seeds an initial user on server startup if one doesn't already exist

Without the `auth` section, existing API routes continue to work without authentication and auth endpoints return `404`.

### Log Storage

Log storage controls where job logs are stored:

```yaml
log_storage:
  local_dir: "/var/stroem/logs"
  s3:                              # optional — omit to disable
    bucket: "my-stroem-logs"
    region: "eu-west-1"
    prefix: "logs/"               # optional key prefix, default ""
    endpoint: "http://minio:9000" # optional — for S3-compatible storage
```

- **local_dir**: Directory for local JSONL log files (always used for active jobs)
- **s3** (optional): When configured, logs are uploaded to S3 when a job reaches a terminal state (completed/failed). Log read endpoints try local files first and fall back to S3 if the local file is missing.
  - **bucket**: S3 bucket name
  - **region**: AWS region
  - **prefix**: Key prefix for S3 objects (e.g. `"logs/"` produces keys like `logs/{job_id}.jsonl`)
  - **endpoint**: Custom endpoint URL for S3-compatible storage (MinIO, LocalStack, etc.)

Credentials use the standard AWS credential chain (environment variables, IAM role, `~/.aws/credentials`).

The S3 feature requires building with `--features s3`:

```bash
cargo build -p stroem-server --features s3
```

### Workspaces

Workspaces define where workflow files are loaded from. The `workspaces` map supports multiple named workspaces with different source types:

```yaml
workspaces:
  default:
    type: folder
    path: ./workspace
  data-team:
    type: git
    url: https://github.com/org/data-workflows.git
    ref: main
    poll_interval_secs: 60
    auth:
      type: token
      token: "ghp_xxx"
```

**Folder source**: Loads workflow files from a local directory path. Computes a content hash as the revision for tarball caching. Supports file watching for hot-reload.

**Git source**: Clones a git repository and loads workflow files from it. Polls for changes at the configured interval. Supports SSH key and token authentication.

Workers download workspace files as tarballs from the server and cache them locally. The tarball endpoint uses ETag-based caching so workers only re-download when the workspace changes.
