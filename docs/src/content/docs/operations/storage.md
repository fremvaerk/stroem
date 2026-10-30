---
title: Artifact Storage
description: Configure where job artifacts are stored — S3, local disk, or inherited from log storage.
---

Strøm stores three kinds of opaque blobs: **log archives**, **task / global state snapshots**, and **per-job artifacts**. All three sit behind the same `BlobArchive` trait, so any one of them can run on S3, local disk, or share a backend with the others.

This page covers artifact storage specifically. For logs see [Log Storage](/operations/log-storage/); for state see the [Task State guide](/guides/task-state/).

## Configuration

### Inherit the log-storage backend (default)

The simplest setup — omit `artifact_storage.archive` and Strøm reuses whatever `log_storage` is already configured. Artifact keys land under a separate prefix (default `artifacts/`) so they never collide with log objects.

```yaml
log_storage:
  archive:
    type: s3
    bucket: "my-stroem-logs"
    region: "eu-west-1"
    prefix: "logs/"

artifact_storage:
  prefix: "artifacts/"    # optional, default "artifacts/"
  max_file_bytes: 104857600   # 100 MiB, default
  max_job_bytes:  1073741824  # 1 GiB,  default
```

### Dedicated artifact backend

Override `archive` when you want artifacts on a different bucket/region — for example, a colder tier than logs, or a different account.

```yaml
artifact_storage:
  prefix: "artifacts/"
  max_file_bytes: 104857600
  max_job_bytes:  1073741824
  archive:
    type: s3
    bucket: "my-stroem-artifacts"
    region: "eu-west-1"
    prefix: "v1/"                  # additional prefix inside the bucket
    endpoint: "http://minio:9000"  # optional, for S3-compatible storage
    # path: "/mnt/artifacts"       # local backend only
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `prefix` | No | Key prefix prepended to every artifact object (default: `"artifacts/"`) |
| `max_file_bytes` | No | Per-file size cap, in bytes. Worker uploads larger than this are rejected with HTTP 413 (default: 100 MiB) |
| `max_job_bytes`  | No | Sum-of-all-artifacts cap per job, in bytes (default: 1 GiB) |
| `archive` | No | `BlobArchive` config — same shape as `log_storage.archive`. Omit to inherit. |

### Storage key layout

```
{archive_prefix}{artifact_prefix}{workspace}/{job_id}/{step_name}/{artifact_name}
```

`artifact_name` may contain `/` — recursive uploads from `/artifacts/reports/q1.html` keep the slash in the key.

## Caps, defaults, retries

- **Per-file cap** rejects oversized uploads before they hit the backend. Increase only if you trust the workload — the server holds each artifact upload in memory as a whole before it stores it. Downloads stream from the S3 and local backends.
- **Per-job cap** is checked at every upload using `SUM(size_bytes) WHERE job_id = ?`, accounting for replacements (uploading the same `name` twice subtracts the old size from the projection).
- **Upload retry**: workers retry each artifact 3× with exponential backoff (250ms → 500ms → 1000ms, capped at 4s) before failing the step.
- **Cleanup on failure**: if the third attempt still fails, the worker calls `DELETE /worker/jobs/{id}/steps/{step}/artifacts` to drop already-uploaded blobs for that step, and the step is demoted to `failed`.

## Retention

Artifacts have no independent TTL. They live exactly as long as the `job` row does and are deleted by the existing [`retention.job_days` sweep](/operations/retention/). Deletion is two-phase: the sweep first removes blobs via `BlobArchive::delete_prefix`, then deletes the row (the FK is `RESTRICT`, so the order is enforced).

## Input files

Files that people and programs upload for [`file` input fields](/guides/file-inputs/) use the same `BlobArchive` backends. Configure them with the `input_files` block:

```yaml
input_files:
  max_file_bytes: 104857600      # 100 MiB, default
  upload_ttl_secs: 86400         # unused uploads, 24 h
  upload_url_ttl_secs: 900       # MCP upload URLs, 15 min
  mcp_inline_max_bytes: 1048576  # decoded, 1 MiB
  prefix: "inputs/"
  # archive: { … }               # optional, same shape as log_storage.archive
```

| Field | Default | Description |
|-------|---------|-------------|
| `max_file_bytes` | 100 MiB | Largest accepted upload. A larger body gets `413` |
| `upload_ttl_secs` | `86400` | Age after which an upload that no job uses is deleted |
| `upload_url_ttl_secs` | `900` | Lifetime of an MCP upload URL (single use) |
| `mcp_inline_max_bytes` | `1048576` | Largest inline MCP file, decoded. Must not be more than 1468006 (1.4 MiB), so the base64 form fits the `/mcp` body limit |
| `prefix` | `"inputs/"` | Key prefix for input file blobs |
| `archive` | — | Dedicated backend. Omit to inherit |

All values must be positive. Environment variables use the `STROEM__INPUT_FILES__…` form, for example `STROEM__INPUT_FILES__MAX_FILE_BYTES`.

**Backend fallback:** `input_files.archive` if set, else the artifact archive, else the log archive. With no archive configured, every upload fails with `file storage is not configured` (`503` on the REST and upload-URL routes, a tool error for an MCP inline file) and the worker download route answers `503`, so no job can get a file; a task whose `file` fields are all optional still runs without one. The server logs `input_files: no archive configured; file uploads answer 503` once at startup, and the [orphan sweep](/operations/retention/#input-files) skips files.

**Key layout:** `{archive_prefix}{prefix}{workspace}/{file_id}`. A blob is shared by every job that uses the file.

Uploads and downloads stream: the server does not hold a whole file in memory. On S3, a file larger than one 8 MiB part is sent as a multipart upload, with at most one part in memory. A multipart upload declares a CRC32 checksum unless the AWS SDK is set to `request_checksum_calculation = when_required` (for example `AWS_REQUEST_CHECKSUM_CALCULATION=when_required`, which some S3-compatible stores need); then it declares none.

## Credentials

S3 credentials use the standard AWS credential chain: environment variables, IAM role, or `~/.aws/credentials`. No artifact-specific credential settings are needed.
