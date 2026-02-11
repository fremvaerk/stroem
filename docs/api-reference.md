# API Reference

Base URL: `http://localhost:8080` (configurable via `server-config.yaml`)

## Public API

No authentication required in Phase 1 (MVP).

---

### List Tasks

```
GET /api/tasks
```

Returns all tasks loaded from workspace workflow files.

**Response:**

```json
[
  {
    "name": "hello-world",
    "mode": "distributed"
  },
  {
    "name": "deploy-pipeline",
    "mode": "distributed"
  }
]
```

---

### Get Task Detail

```
GET /api/tasks/{name}
```

**Path parameters:**
- `name` -- Task name (e.g., `hello-world`)

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
    },
    "shout-it": {
      "action": "shout",
      "depends_on": ["say-hello"],
      "input": { "message": "{{ say_hello.output.greeting }}" }
    }
  }
}
```

---

### Execute Task

```
POST /api/tasks/{name}/execute
```

Creates a new job for the given task.

**Path parameters:**
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
curl -X POST http://localhost:8080/api/tasks/hello-world/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"name": "World"}}'
```

---

### List Jobs

```
GET /api/jobs
```

**Query parameters:**
- `workspace` (optional) -- Filter by workspace name
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
      "status": "completed",
      "worker_id": "w1w2w3w4-w5w6-7890-abcd-ef1234567890",
      "started_at": "2025-02-10T12:00:02Z",
      "completed_at": "2025-02-10T12:00:03Z",
      "error_message": null
    }
  ]
}
```

**Job statuses:** `pending`, `running`, `completed`, `failed`, `cancelled`

**Step statuses:** `pending`, `ready`, `running`, `completed`, `failed`, `skipped`

---

### Get Job Logs

```
GET /api/jobs/{id}/logs
```

Returns the combined log output from all steps of a job.

**Path parameters:**
- `id` -- Job ID (UUID)

**Response:**

```json
{
  "logs": "Hello World\nOUTPUT: {\"greeting\": \"Hello World\"}\nHELLO WORLD\n"
}
```

**Example:**

```bash
curl -s http://localhost:8080/api/jobs/JOB_ID/logs | jq -r .logs
```

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
  "capabilities": ["shell"]
}
```

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
  "capabilities": ["shell"]
}
```

**Response (step available):**

```json
{
  "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "step_name": "say-hello",
  "action_name": "greet",
  "action_type": "shell",
  "action_image": null,
  "action_spec": {
    "cmd": "echo Hello World && echo 'OUTPUT: {\"greeting\": \"Hello World\"}'",
    "env": {}
  },
  "input": {
    "name": "World"
  }
}
```

The `action_spec` contains the fully resolved action definition with templates already rendered against the job context.

**Response (no work available):**

```json
{
  "job_id": null,
  "step_name": null,
  "action_name": null,
  "action_type": null,
  "action_image": null,
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

When a step completes successfully, the orchestrator checks if any downstream steps now have all their dependencies met and promotes them to `ready` status. When all steps are in a terminal state, the job is marked `completed` (or `failed` if any step failed).

---

### Push Logs

```
POST /worker/jobs/{id}/logs
```

Appends a chunk of log output to the job's log file. Called periodically (~1s) during step execution.

**Path parameters:**
- `id` -- Job ID (UUID)

**Request body:**

```json
{
  "chunk": "Hello World\nOUTPUT: {\"greeting\": \"Hello World\"}\n"
}
```

**Response:**

```json
{
  "status": "ok"
}
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
- `401` -- Unauthorized (missing or invalid worker token)
- `404` -- Not found (unknown task, job, or step)
- `500` -- Internal server error
