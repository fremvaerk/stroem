---
title: Retention
description: Automatic cleanup of old workers, jobs, and logs
---

Strøm can automatically clean up old data — inactive workers, completed jobs, and their log files. Retention is configured via the `retention` section in `server-config.yaml` and is **disabled by default**. The one exception is the [input-file sweep](#input-files), which always runs.

## Configuration

```yaml
retention:
  worker_hours: 2          # Delete inactive workers older than 2h (optional)
  job_days: 30             # Delete terminal jobs and logs 30d after they finished (optional)
  interval_secs: 3600      # How often cleanup runs, in seconds (default: 3600)
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `worker_hours` | integer | disabled | Hours after which inactive workers are deleted |
| `job_days` | integer | disabled | Days after a job **finished** before it and its logs are deleted |
| `interval_secs` | integer | `3600` | How often the cleanup runs (rate-limited independently of the recovery sweep) |

## Worker retention (`worker_hours`)

Deletes workers with `status = 'inactive'` whose last heartbeat is older than the configured threshold. Active workers are never deleted. Workers that come back online are automatically reactivated on their next heartbeat — retention only removes workers that are truly gone.

## Job retention (`job_days`)

For each terminal job (`completed`, `failed`, `cancelled`, or `skipped`) that finished more than `job_days` ago:
1. Deletes local log files (`.jsonl` and legacy `.log`)
2. Deletes the archive log (S3 or local archive, if configured)
3. Deletes the job and its steps from the database (steps are cascade-deleted via FK constraint)

Pending and running jobs are never affected. Jobs are processed in batches of 1000 per cleanup cycle.

The age is counted from when the job **finished** (`completed_at`; a terminal job without one falls back to when it was created). A job that ran for longer than `job_days` is therefore kept for `job_days` after it ended — it is never deleted while its hooks, task retry and log archive are still being written.

## Input files

When job retention deletes a job, it deletes the job's links to its [input files](/guides/file-inputs/) in the same transaction. It does not delete a file then, because other jobs (a Re-run, a restart) can use it.

The same leader-only cleanup tick also deletes **orphans**: a file that no job uses and that is older than `input_files.upload_ttl_secs` (24 hours by default). The sweep marks the row as being deleted, removes the blob, then removes the row. If the blob delete fails, the next sweep tries again. A job that is created at the same moment cannot attach a file that the sweep has marked: creation fails with `Input field '…': the file cannot be used`. The sweep also deletes expired MCP upload URLs.

This sweep needs no configuration. It runs on every cleanup tick (`interval_secs`, once per hour by default), also without a `retention` section and without `job_days` or `worker_hours`. It handles at most 1000 files per tick. When no blob backend for input files is configured (see [Storage](/operations/storage/#input-files)), it deletes only the expired upload URLs.

## How it works

Retention cleanup runs as a phase of the recovery sweeper background task, but is rate-limited independently via `interval_secs` (default: once per hour) so that frequent recovery sweeps don't trigger expensive deletion scans on every cycle.
