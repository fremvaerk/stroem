---
title: Log Storage
description: Local log files, archive backends, and log streaming
---

Strøm stores job logs as structured JSONL files. Logs can be stored locally and optionally archived to a pluggable backend (S3 or local filesystem).

## Configuration

### Archive backend (recommended)

```yaml
log_storage:
  local_dir: "/var/stroem/logs"
  archive:
    type: s3                         # "s3" or "local"
    bucket: "my-stroem-logs"         # S3 only
    region: "eu-west-1"              # S3 only
    prefix: "logs/"                  # optional key prefix, default ""
    endpoint: "http://minio:9000"    # optional — for S3-compatible storage
    # path: "/mnt/archive"           # local only — directory for archive files
```

### Legacy S3 config (still supported)

```yaml
log_storage:
  local_dir: "/var/stroem/logs"
  s3:                              # legacy format — use archive instead
    bucket: "my-stroem-logs"
    region: "eu-west-1"
    prefix: "logs/"
    endpoint: "http://minio:9000"
```

If both `archive` and `s3` are set, `archive` takes precedence.

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `local_dir` | No | Directory for local JSONL log files (default: `/tmp/stroem/logs`) |
| `archive.type` | Archive only | Backend type: `"s3"` or `"local"` |
| `archive.bucket` | S3 only | S3 bucket name |
| `archive.region` | S3 only | AWS region |
| `archive.prefix` | No | Key prefix for archive objects (default: `""`) |
| `archive.endpoint` | No | Custom endpoint for S3-compatible storage (MinIO, LocalStack) |
| `archive.path` | Local only | Directory for local archive files |

## Log format

Each log line is a JSON object in JSONL format:

```json
{"ts":"2025-02-12T10:56:45.123Z","stream":"stdout","step":"say-hello","line":"Hello World"}
```

| Field | Description |
|-------|-------------|
| `ts` | ISO 8601 timestamp |
| `stream` | `"stdout"` or `"stderr"` |
| `step` | Step name that produced this line |
| `line` | The log line content |

## Log archival

When an archive backend is configured, logs are uploaded when a job reaches a terminal state (completed/failed). The upload happens **after** hooks fire, so server events from hook execution are included in the archive.

### Archive key structure

```
{prefix}{workspace}/{task}/YYYY/MM/DD/YYYY-MM-DDTHH-MM-SS_{job_id}.jsonl.gz
```

All timestamps in the key are UTC. Files are gzip-compressed.

For the **local** archive backend, this key maps to subdirectories under the configured `path`.

### Read fallback

When reading logs, the server tries sources in order:
1. Local JSONL file (live buffer)
2. Legacy `.log` file (pre-JSONL format)
3. Archive backend (if configured)

### S3 credentials

S3 credentials use the standard AWS credential chain: environment variables, IAM role, or `~/.aws/credentials`.

## Server events

Server-side errors (hook failures, orchestration errors, recovery timeouts) are written to the log file with `step: "_server"` and `stream: "stderr"`. These are visible in the UI's "Server Events" panel on the job detail page.

Retrieve server events via API:

```bash
curl -s http://localhost:8080/api/jobs/JOB_ID/steps/_server/logs | jq -r .logs
```

## WebSocket streaming

Real-time log streaming is available via WebSocket:

```
GET /api/jobs/{id}/logs/stream
```

On connect, the server sends existing log content (backfill), then streams new log chunks as they arrive from workers.

```bash
websocat ws://localhost:8080/api/jobs/JOB_ID/logs/stream
```
