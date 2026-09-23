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
| `read.tail_default_bytes` | No | Tail size when a request names none. Default `262144` (256 KiB). |
| `read.tail_max_bytes` | No | Largest `tail_bytes` a request may ask for. Default `4194304` (4 MiB). |
| `read.tail_scan_max_bytes` | No | How far back a step tail scans before giving up. Default `67108864` (64 MiB). |
| `read.max_line_bytes` | No | Longest single line a filtered read will carry; longer lines are skipped with a warning and set `truncated`. Default `1048576` (1 MiB). |
| `read.merge_max_bytes` | No | Sum of local length + archive decompressed length under which a terminal full read still merges in memory. Default `16777216` (16 MiB). |
| `read.merge_max_lines` | No | Most lines a union merge (tail or full) will hold; above it a single source is served. Default `131072`. |

Env overrides follow the existing convention: `STROEM__LOG_STORAGE__READ__TAIL_DEFAULT_BYTES`, etc.

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

`.jsonl` → legacy `.log` → (finished jobs only) the archive; a missing file is never an error.

### S3 credentials

S3 credentials use the standard AWS credential chain: environment variables, IAM role, or `~/.aws/credentials`.

## Server events

Server-side errors (hook failures, orchestration errors, recovery timeouts) are written to the log file with `step: "_server"` and `stream: "stderr"`. These are visible in the UI's "Server Events" panel on the job detail page.

Retrieve server events via API:

```bash
curl -s http://localhost:8080/api/jobs/JOB_ID/steps/_server/logs | jq -r .logs
```

To fetch a whole log, ask for the stream — it has no JSON envelope, so do not pipe it through `jq .logs`:

```bash
curl -s -H "Authorization: Bearer $TOKEN" "$STROEM/api/jobs/$JOB/logs?full=true" > job.jsonl
```

## Reading logs

Every log read is bounded. A read returns either a **tail** — the newest whole lines, 256 KiB by default — or the **full** log as a stream. The UI, `stroem-api logs`, the MCP `get_job_logs` tool and the WebSocket backfill all start from the tail.

| Job | Tail | Full |
|---|---|---|
| running | local file | local file |
| finished, archive configured | union of the local and archived tails (`merged`) | union in memory while local + archive ≤ `merge_max_bytes` and ≤ `merge_max_lines`; otherwise the local file, or the archive when there is no local file |

Neither source is guaranteed complete on its own (mirroring gaps, lines written after the archive upload), which is why finished jobs merge them when the caps allow. `X-Stroem-Log-Source` says which source answered. A full stream that fails part-way ends early; there is no fallback once the response has started.

**Memory.** The limits above bound what one read can allocate. The largest read a client can trigger (a 4 MiB tail of a finished job with very short lines) is about 69 MiB; a normal UI poll is under 6 MiB. On a server with a 512 Mi memory limit set `log_storage.read.tail_max_bytes: 1048576`.

**Behaviour change.** Scripts reading `logs` from `/api/jobs/{id}/logs` now receive the tail; check `truncated`, or use `?full=true`. Older `stroem-api` binaries print the tail without a note. WebSocket clients receive a tail as the first frame.

## WebSocket streaming

Real-time log streaming is available via WebSocket:

```
GET /api/jobs/{id}/logs/stream
```

On connect, the server sends the default tail (`read_tail`, see [Reading logs](#reading-logs)) as backfill, then streams new log chunks as they arrive from workers.

```bash
websocat ws://localhost:8080/api/jobs/JOB_ID/logs/stream
```
