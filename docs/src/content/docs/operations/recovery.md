---
title: Recovery
description: Worker heartbeat monitoring and automatic failure recovery
---

If a worker dies mid-step (crash, OOM, network partition), the server automatically detects the failure and recovers stuck jobs.

## How it works

1. Workers send heartbeats every 30 seconds
2. The recovery sweeper runs on a configurable interval (default: 60s)
3. Workers whose last heartbeat exceeds the timeout are marked as `inactive`
4. Running steps assigned to inactive workers are failed with a timeout error
5. The orchestrator cascades the failure: dependent steps are skipped, the job is marked failed
6. If the failed step was part of a child job (`type: task`), the failure propagates to the parent

## Configuration

Add an optional `recovery` section to `server-config.yaml`:

```yaml
recovery:
  heartbeat_timeout_secs: 120   # Seconds without heartbeat before stale (default: 120)
  sweep_interval_secs: 60       # How often the sweeper runs (default: 60)
```

When the `recovery` section is omitted, recovery runs with defaults (120s timeout, 60s interval). There is no way to disable it — it's always active.

The default timeout of 120 seconds means a worker must miss 4 consecutive heartbeats (sent every 30s) before being considered stale.

## Recovery strategy

When a worker dies mid-step, the step is **failed, not retried**:

- The step may have partially executed (side effects, partial writes)
- Retrying non-idempotent steps could cause data corruption
- Users can re-trigger the task manually or via `on_error` hooks

## Worker reactivation

If a worker comes back online after being marked inactive, it is automatically reactivated on its next heartbeat. It can then claim new steps normally.

## Recovery visibility

Recovery events are logged as server events on affected jobs, visible in:
- The "Server Events" panel on the job detail page in the UI
- The `_server` step logs via API: `GET /api/jobs/{id}/steps/_server/logs`
