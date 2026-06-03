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

- **Per-file cap** rejects oversized uploads before they hit the backend. Increase only if you trust the workload — large blobs are expensive to read into memory on download.
- **Per-job cap** is checked at every upload using `SUM(size_bytes) WHERE job_id = ?`, accounting for replacements (uploading the same `name` twice subtracts the old size from the projection).
- **Upload retry**: workers retry each artifact 3× with exponential backoff (250ms → 500ms → 1000ms, capped at 4s) before failing the step.
- **Cleanup on failure**: if the third attempt still fails, the worker calls `DELETE /worker/jobs/{id}/steps/{step}/artifacts` to drop already-uploaded blobs for that step, and the step is demoted to `failed`.

## Retention

Artifacts have no independent TTL. They live exactly as long as the `job` row does and are deleted by the existing [`retention.job_days` sweep](/operations/retention/). Deletion is two-phase: the sweep first removes blobs via `BlobArchive::delete_prefix`, then deletes the row (the FK is `RESTRICT`, so the order is enforced).

## Credentials

S3 credentials use the standard AWS credential chain: environment variables, IAM role, or `~/.aws/credentials`. No artifact-specific credential settings are needed.
