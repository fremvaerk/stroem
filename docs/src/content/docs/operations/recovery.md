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
4. **Unmatched step detection**: Steps stuck in `ready` state beyond `unmatched_step_timeout_secs` are checked against active workers. If no active worker has the required capability *and* tags to claim the step, it is failed with a clear error message.

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

The `unmatched_step_timeout_secs` grace period prevents false positives when workers are temporarily restarting or scaling up. A step is only failed if it has been ready for longer than this timeout **and** no active worker matches on both routing dimensions.

### Capability + tag matching (Phase 4, post-041)

Migration 041 split the pre-041 single `tags` axis into two:

* **`required_ability`** on each step (derived from action type + runner). One of: `"script"`, `"docker"`, `"kubernetes"`, `"agent"`. Empty for `task`/`approval` (server-dispatched, excluded from Phase 4).
* **`required_tags`** on each step — only the user-declared action tags. **No longer contains an ability token prefix.**

| Action | Runner | Required ability |
|--------|--------|------------------|
| `script` | `local` (default) | `script` |
| `script` | `docker` | `docker` |
| `script` | `pod` | `kubernetes` |
| `docker` | — | `docker` |
| `pod` | — | `kubernetes` |
| `agent` | — | `agent` |
| `task`, `approval` | — | — (server-dispatched, excluded) |

Workers declare capabilities and (optional) reservation tags in `worker-config.yaml`:

```yaml
capabilities:                # runners this worker supports (required)
  - script
  - docker
  - kubernetes
tags: []      # empty = permissive; add labels to reserve this worker
              # for steps that explicitly request them
```

Phase 4 uses two SQL containment checks in AND:

```sql
worker.capabilities @> to_jsonb(step.required_ability)   -- ability matches
AND worker.tags <@ step.required_tags                    -- worker's taints requested
```

That is, worker's capabilities must **contain** the step's ability, AND the worker's tags must be a **subset of** the step's tags. If no active worker satisfies both after `unmatched_step_timeout_secs`, the step is failed with:

```
No active worker with required capability/tags to run this step
```

`task`/`agent`/`approval`/`event_source` steps are excluded from Phase 4 — they are dispatched server-side, not claimed by workers.

## Recovery strategy

When a worker dies mid-step or a step has no matching worker, the step is **failed, not retried**:

- The step may have partially executed (side effects, partial writes)
- Retrying non-idempotent steps could cause data corruption
- Users can re-trigger the task manually or via `on_error` hooks

## Worker reactivation

If a worker comes back online after being marked inactive, it is automatically reactivated on its next heartbeat. It can then claim new steps normally.

## Data retention

Data retention is configured separately from recovery, in its own `retention` section. See [Retention](/operations/retention/) for details.

## Recovery visibility

Recovery events are logged as server events on affected jobs, visible in:
- The "Server Events" panel on the job detail page in the UI
- The `_server` step logs via API: `GET /api/jobs/{id}/steps/_server/logs`
