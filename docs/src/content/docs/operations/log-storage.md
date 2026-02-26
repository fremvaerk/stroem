---
title: Log Storage
description: Local log files, S3 archival, and log streaming
---

Strøm stores job logs as structured JSONL files. Logs can be stored locally and optionally archived to S3.

## Configuration

```yaml
log_storage:
  local_dir: "/var/stroem/logs"
  s3:                              # optional — omit to disable
    bucket: "my-stroem-logs"
    region: "eu-west-1"
    prefix: "logs/"               # optional key prefix, default ""
    endpoint: "http://minio:9000" # optional — for S3-compatible storage
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `local_dir` | No | Directory for local JSONL log files (default: `/tmp/stroem/logs`) |
| `s3.bucket` | S3 only | S3 bucket name |
| `s3.region` | S3 only | AWS region |
| `s3.prefix` | No | Key prefix for S3 objects (default: `""`) |
| `s3.endpoint` | No | Custom endpoint for S3-compatible storage (MinIO, LocalStack) |

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

## S3 archival

When S3 is configured, logs are uploaded to S3 when a job reaches a terminal state (completed/failed). The upload happens **after** hooks fire, so server events from hook execution are included in the archive.

### S3 key structure

```
{prefix}{workspace}/{task}/YYYY/MM/DD/YYYY-MM-DDTHH-MM-SS_{job_id}.jsonl.gz
```

All timestamps in the key are UTC. Files are gzip-compressed.

### Read fallback

When reading logs, the server tries sources in order:
1. Local JSONL file
2. Legacy `.log` file (pre-JSONL format)
3. S3 (if configured)

### Credentials

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
