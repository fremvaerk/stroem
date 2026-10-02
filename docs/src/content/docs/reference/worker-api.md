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
  "capabilities": ["script", "docker"],
  "tags": []
}
```

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Worker display name |
| `capabilities` | string[] | Runners the worker supports (`"script"`, `"docker"`, `"kubernetes"`, `"agent"`). Required. |
| `tags` | string[] | Reservation labels (optional, default `[]`). Non-empty tags reserve the worker for steps that explicitly request them. |

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

Claims the next ready step matching the worker on both routing dimensions. Uses `SELECT FOR UPDATE SKIP LOCKED` for concurrency safety.

**Request body:**

```json
{
  "worker_id": "w1w2w3w4-...",
  "capabilities": ["script", "docker"],
  "tags": []
}
```

A step is claimed only when BOTH:
1. The step's `required_ability` is present in the worker's `capabilities`, AND
2. The worker's `tags` are a subset of the step's `required_tags` (empty worker tags accept anything; non-empty tags reserve the worker for steps that explicitly requested them).

**Response (step available):**

```json
{
  "job_id": "a1b2c3d4-...",
  "workspace": "default",
  "step_name": "say-hello",
  "action_name": "greet",
  "action_type": "script",
  "action_image": null,
  "runner": "local",
  "action_spec": {
    "script": "echo Hello World",
    "env": {}
  },
  "input": { "name": "World" }
}
```

The `action_spec` contains the fully resolved action definition with templates already rendered. The `runner` field indicates how to execute: `local`, `docker`, `pod`, or `none` (for `type: docker`/`type: pod` actions).

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

When a step completes, the orchestrator checks downstream dependencies. When a step fails, dependent steps are skipped `unreachable` unless the failed step itself has `continue_on_failure: true` — the flag is read from the dependency, not from the dependents.

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

## Upload Step Artifact

```
POST /worker/jobs/{id}/steps/{step}/artifacts/{name}
```

Uploads a single file produced by a successful step. The worker scans `/artifacts/` after the step exits and POSTs each file here. `name` is the file's path relative to `/artifacts/` and may contain `/` (greedy match — `reports/q1.html` is one upload).

**Request:**

- `Content-Type` — the type the worker sniffed via `infer` (defaults to `application/octet-stream`).
- Body — raw file bytes.

**Response:**

```json
{
  "id": "0190f...",
  "name": "report.html",
  "size_bytes": 12453,
  "content_type": "text/html"
}
```

| Status | Description |
|--------|-------------|
| `201` | Artifact stored. Repeat uploads with the same `name` replace the existing row (last writer wins). |
| `404` | `job` row doesn't exist. |
| `413` | Per-file or per-job size cap exceeded (`artifact_storage.max_file_bytes` / `max_job_bytes`). |

## Delete Step Artifacts

```
DELETE /worker/jobs/{id}/steps/{step}/artifacts
```

Removes every artifact uploaded for `step` on this job — both the `job_artifact` rows and the underlying blobs. The worker calls this after an upload attempt fails terminally, so a partial upload doesn't leave orphan blobs from a step that gets demoted to `failed`.

| Status | Description |
|--------|-------------|
| `204` | Cleared (also returned if nothing was there). |
| `404` | `job` row doesn't exist. |

## Download Workspace Tarball

```
GET /worker/workspace/{ws}.tar.gz
GET /worker/workspace/{ws}.tar.gz?revision={revision}
```

Downloads a workspace as a gzipped tar archive.

| Parameter | Description |
|-----------|-------------|
| `ws` | Workspace name |
| `revision` | Optional. The revision the claim response named (`revision`); the worker always sends it for a claimed step. Without it, the current revision is served |

**Headers:**
- `If-None-Match` — Revision ETag for conditional fetch

**Response headers:**
- `Content-Type: application/gzip`
- `X-Revision: {revision}`
- `ETag: "{revision}"`

How a requested `revision` is served:

- **The current revision of a healthy workspace**: built from the server's
  working copy, as without `revision`. For a git workspace this includes
  its `.git` directory.
- **Any other revision of a git workspace** — an older commit, a commit a
  [git ref](/guides/git-refs/) pinned, or any commit while the workspace's
  default branch fails to load: built from a clean checkout of that commit,
  which has **no `.git` directory**, and cached. This does not depend on the
  workspace's live load being healthy.
- **A folder workspace**: only revisions still in the server's tarball cache,
  or the current one, can be served; any other is `404`.

| Status | Description |
|--------|-------------|
| `200` | Tarball returned |
| `304` | Not Modified (workspace unchanged) |
| `404` | Workspace not found, the commit does not exist in the git repository, or a folder workspace's revision is no longer available |
| `503` | The commit exists but cannot be fetched right now (the git server is unreachable from this replica). Carries `Retry-After: 5`. The worker tries again every 5 seconds, up to 12 attempts in all (about a minute), then fails the step |

## Download State Snapshot

```
GET /worker/state/{ws}/{task}?job_id={job_id}
GET /worker/global-state/{ws}?job_id={job_id}
```

Downloads the latest task-state (or global-state) snapshot as a gzipped tar
archive, with an `X-Snapshot-Id` header. `204` when there is no snapshot yet,
`404` when state storage is not configured.

| Parameter | Description |
|-----------|-------------|
| `job_id` | Optional. The claimed job. With it, the server reads the **job's own** state coordinates — its workspace, its task and, for a job on a [git ref](/guides/git-refs/), its ref — and ignores `{ws}` / `{task}`. `404` if the job does not exist |

Without `job_id` (a worker older than the server), the path's `{ws}` /
`{task}` and the default (unpinned) partition are used. A cross-workspace
step then reads the action owner's coordinates instead of its own job's, and
a pinned job reads the unpinned partition — upgrade workers together with
the server.

## Upload State Snapshot

```
POST /worker/state/{ws}/{task}/{job_id}?has_json={bool}
POST /worker/global-state/{ws}/{job_id}?has_json={bool}
```

Stores a new snapshot (gzipped tarball body, at most 50 MB) and prunes old
ones. The server writes it to the **job's** workspace, task and ref; the
`{ws}` / `{task}` path segments are not used. `has_json` tells the server
the tarball contains a `state.json` sidecar.

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
