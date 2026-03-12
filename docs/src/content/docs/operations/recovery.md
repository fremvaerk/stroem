---
title: Recovery
description: Worker heartbeat monitoring, timeout enforcement, and unmatched step detection
---

If a worker dies mid-step (crash, OOM, network partition), or a step sits in `ready` state with no worker able to claim it, the server automatically detects the problem and recovers stuck jobs.

## How it works

The recovery sweeper runs on a configurable interval (default: 60s) and performs four phases:

1. **Stale worker detection**: Workers whose last heartbeat exceeds the timeout are marked `inactive`. Running steps assigned to inactive workers are failed.
2. **Step timeout enforcement**: Running steps that have exceeded their configured `timeout` are failed.
3. **Job timeout enforcement**: Running jobs that have exceeded their configured `timeout` are cancelled.
4. **Unmatched step detection**: Steps stuck in `ready` state beyond `unmatched_step_timeout_secs` are checked against active workers. If no active worker has the required tags to claim the step, it is failed with a clear error message.

After each failure, the orchestrator cascades: dependent steps are skipped, the job is marked failed, and parent jobs are notified.

## Configuration

Add an optional `recovery` section to `server-config.yaml`:

```yaml
recovery:
  heartbeat_timeout_secs: 120        # Seconds without heartbeat before stale (default: 120)
  sweep_interval_secs: 60            # How often the sweeper runs (default: 60)
  unmatched_step_timeout_secs: 30    # Seconds a ready step waits before checking for matching workers (default: 30)
```

When the `recovery` section is omitted, recovery runs with defaults. There is no way to disable it — it's always active.

The default heartbeat timeout of 120 seconds means a worker must miss 4 consecutive heartbeats (sent every 30s) before being considered stale.

The `unmatched_step_timeout_secs` grace period prevents false positives when workers are temporarily restarting or scaling up. A step is only failed if it has been ready for longer than this timeout **and** no active worker has matching tags.

### Tag-based worker matching (Phase 4)

Steps compute a set of `required_tags` based on their action type and runner:
- `type: script` with `runner: local` → `["script"]`
- `type: script` with `runner: docker` → `["script", "docker"]`
- `type: script` with `runner: pod` → `["script", "kubernetes"]`
- `type: docker` → `["docker"]`
- `type: pod` → `["kubernetes"]`
- `type: task` → excluded from Phase 4 (dispatched server-side)
- Steps with explicit `tags: [...]` include those as well

Workers declare their capabilities in `worker-config.yaml`:
```yaml
tags:
  - script
  - docker
  - kubernetes
```

Phase 4 uses a SQL containment check: `required_tags <@ worker_tags` (PostgreSQL). A step is claimable by a worker only if **all required tags are present** in the worker's tag set. If no active worker satisfies this requirement after `unmatched_step_timeout_secs` seconds, the step is failed with error message:

```
No active worker with required tags to run this step
```

`type: task` steps are explicitly excluded from Phase 4 — they are dispatched by the server itself via `handle_task_steps()`, not claimed by workers.

## Recovery strategy

When a worker dies mid-step or a step has no matching worker, the step is **failed, not retried**:

- The step may have partially executed (side effects, partial writes)
- Retrying non-idempotent steps could cause data corruption
- Users can re-trigger the task manually or via `on_error` hooks

## Worker reactivation

If a worker comes back online after being marked inactive, it is automatically reactivated on its next heartbeat. It can then claim new steps normally.

## Recovery visibility

Recovery events are logged as server events on affected jobs, visible in:
- The "Server Events" panel on the job detail page in the UI
- The `_server` step logs via API: `GET /api/jobs/{id}/steps/_server/logs`
