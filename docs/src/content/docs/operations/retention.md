---
title: Retention
description: Automatic cleanup of old workers, jobs, and logs
---

Strøm can automatically clean up old data — inactive workers, completed jobs, and their log files. Retention is configured via the `retention` section in `server-config.yaml` and is **disabled by default**.

## Configuration

```yaml
retention:
  worker_hours: 2          # Delete inactive workers older than 2h (optional)
  job_days: 30             # Delete terminal jobs and logs older than 30d (optional)
  interval_secs: 3600      # How often cleanup runs, in seconds (default: 3600)
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `worker_hours` | integer | disabled | Hours after which inactive workers are deleted |
| `job_days` | integer | disabled | Days after which terminal jobs and their logs are deleted |
| `interval_secs` | integer | `3600` | How often the cleanup runs (rate-limited independently of the recovery sweep) |

## Worker retention (`worker_hours`)

Deletes workers with `status = 'inactive'` whose last heartbeat is older than the configured threshold. Active workers are never deleted. Workers that come back online are automatically reactivated on their next heartbeat — retention only removes workers that are truly gone.

## Job retention (`job_days`)

For each terminal job (`completed`, `failed`, `cancelled`, or `skipped`) older than the configured threshold:
1. Deletes local log files (`.jsonl` and legacy `.log`)
2. Deletes the archive log (S3 or local archive, if configured)
3. Deletes the job and its steps from the database (steps are cascade-deleted via FK constraint)

Pending and running jobs are never affected. Jobs are processed in batches of 1000 per cleanup cycle.

## How it works

Retention cleanup runs as a phase of the recovery sweeper background task, but is rate-limited independently via `interval_secs` (default: once per hour) so that frequent recovery sweeps don't trigger expensive deletion scans on every cycle.
