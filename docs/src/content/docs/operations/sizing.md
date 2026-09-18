---
title: Sizing
description: CPU, memory and probe settings for servers with many workspaces.
---

The server loads every configured workspace before it starts serving
requests, and keeps each git workspace's checkout and parsed configuration
in memory. Size it for the number of workspaces you run.

## Measured baseline

On a production server with **9 git workspaces**, `limits.cpu: 500m` and
`limits.memory: 512Mi`:

| Signal | Value |
|---|---|
| Workspace load at boot | 51 s |
| Memory in use | 95 % of the 512 Mi limit |
| Tokio worker threads | 1 (a sub-core CPU quota rounds down to one) |

Extrapolating linearly, 50 workspaces would need about 4.7 minutes to boot at
that quota — beyond a 5-minute startup probe. This is an estimate; measure your
own deployment.

## Recommended starting point (≈50 git workspaces)

| Setting | Value |
|---|---|
| `server.resources.requests` | `cpu: 1`, `memory: 1Gi` |
| `server.resources.limits` | `cpu: 2`, `memory: 2Gi` |
| `server.startupProbe` | `/healthz`, `failureThreshold: 30`, `periodSeconds: 10` |

These are the chart defaults. After changing them, check:

- the `Loaded N workspace(s) in …` log line at startup;
- `container_memory_working_set_bytes` for the server pod;
- `stroem_workspace_load_permits_available` and
  `stroem_workspace_load_overdue` (see [Metrics](/operations/metrics/)).

The server never runs fewer than 4 tokio worker threads, whatever the CPU
quota. Git clones live in the container's writable layer under `/tmp`; budget
node ephemeral storage for the sum of your repositories.
