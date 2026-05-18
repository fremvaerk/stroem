---
title: High Availability
description: Running multiple Strøm server replicas with leader election and cross-replica event propagation
---

Strøm can run with 2 or more `stroem-server` replicas behind a load balancer for high availability — typical use case is EKS / GKE on Spot or Preemptible nodes that can be terminated at any time. The data plane (workers) is already stateless and crash-resilient; this page covers what the control plane needs.

## How it works

### Leader election (Postgres advisory lock)

Three background tasks must run on **exactly one** replica at a time, otherwise they'd duplicate work:

- The **scheduler** (would double-fire cron triggers)
- The **event source manager** (would create duplicate consumer jobs)
- The **recovery sweeper** (would double-log timeouts)

At startup each replica opens a dedicated Postgres connection and runs:

```sql
SELECT pg_try_advisory_lock(0x5354524D4C445201);  -- "STRMLDR" + version 0x01
```

The replica that acquires the lock becomes the **leader** and runs all three tasks. Followers run the loops but skip the work each tick (so they're warm and can take over instantly). If the leader's connection drops (pod restart, network blip, DB restart) Postgres releases the lock automatically — within ~5 s a follower acquires it. No external coordinator (etcd, ZooKeeper, Raft) is required.

### Cross-replica events (Postgres LISTEN/NOTIFY)

Three things need to propagate across replicas immediately, not via the next poll cycle:

| Channel | Trigger | Receiver action |
|---|---|---|
| `stroem_job_cancelled` | API/UI cancellation on replica A | Replica B inserts into its local `cancelled_jobs` cache so workers polling B see the cancellation. |
| `stroem_workspace_reloaded` | Watcher on replica A detects a new revision | Replica B reloads its workspace config cache (otherwise it waits up to `poll_interval_secs`, default 30s). |
| `stroem_job_log_chunk` | Worker pushes logs to replica A | Replica B fans out to its local WebSocket viewers. |

Every payload carries the publishing replica's UUID; receivers drop their own messages to avoid duplicate broadcasts.

For `stroem_job_log_chunk`, Postgres caps `NOTIFY` payloads at ~8 KB. Lines larger than ~7 KB degrade to a signal-only message — peer replicas know new content exists but can't render it live. Reload the browser tab to re-backfill from the archive once the job completes. In practice, individual log lines that large are rare; if your workloads emit them routinely, see [Known limitations](#known-limitations) below.

### Health endpoints

Two endpoints, split by audience:

- **`GET /healthz`** (unauthenticated, k8s-probe compatible). Returns 200 if the process is up and the database is reachable, 503 otherwise. Body is minimal — `{"status": "ok" | "unhealthy", "db": "ok" | "error"}` — and contains **no** leader identity or per-task liveness, so unauthenticated scanners can't preferentially target the leader pod for DoS.
- **`GET /healthz/detail`** (auth required — `Authorization: Bearer <worker_token>`). Returns the full HA diagnostic shape: `status`, `checks.db`, `checks.leader`, `checks.scheduler`, `checks.recovery`, `checks.event_source`. On the leader, scheduler/recovery/event-source liveness is failure-eligible (503 if a guard dropped). On followers, those fields report `"follower"` and the endpoint returns 200.

This lets Kubernetes liveness probes pass on follower pods (they're doing real work — serving API/WebSocket traffic) without masking a stuck scheduler on the leader, while keeping cluster topology out of unauthenticated responses.

## Required infrastructure

| Component | Requirement |
|---|---|
| **Postgres** | Reachable from every replica (single primary; RDS Multi-AZ recommended for prod). All HA coordination flows through it. |
| **Log archive (S3 / EFS / local PVC mounted RWX)** | Optional but strongly recommended. Without an archive, finished-job logs only exist on the replica that received them; reading from any other pod returns empty. With S3 configured, every replica uploads and reads from the shared bucket. |
| **Shared `auth.jwt_secret`** | If auth is enabled, the same secret value must be set on every replica. Otherwise refresh tokens minted on one pod won't validate on another. Same for `auth.refresh_secret`. The bundled Helm chart's `_validations.tpl` aborts `helm template`/`helm install` when `server.replicas > 1` and auth is enabled but no JWT secret is supplied — guard against accidental split-brain. The server also logs a `HA: auth.jwt_secret loaded (len=N)` message at startup so you can confirm the pods loaded a secret (the value itself is not logged). |
| **PodDisruptionBudget** | The bundled Helm chart enables a PDB with `minAvailable: 1` by default (`server.podDisruptionBudget.enabled: true`). |

### Postgres connection sizing

The HA model maintains always-on connections outside the connection pool:
- **1 leader-election connection** (shared between replicas; only one active at a time)
- **1 LISTEN/NOTIFY listener connection** per replica

Within the pool, each replica uses `min_connections: 5, max_connections: 20`.

**Per-replica worst-case**: 1 (leader) + 1 (listener) + 20 (pool max) = **22 connections per replica**

For capacity planning, multiply by your replica count:
- **2 replicas**: 44 max connections
- **5 replicas**: 110 max connections
- **10 replicas**: 220 max connections

Most RDS small instances cap at ~683 connections; scaling beyond ~30 replicas requires a db.t4g.medium or larger. Monitor `postgresql.connections` CloudWatch metric; if it approaches the instance limit, add more replicas or scale the instance up.

### Advisory lock namespace protection

`pg_try_advisory_lock` operates in a **global per-database namespace** shared with all applications using the same Postgres instance. A co-tenant application (or attacker with database access) could hold the Strøm leader lock indefinitely, preventing leader election on all replicas.

**Mitigation**: Use a **dedicated Postgres database or role for Strøm**. Do not run other applications against the same database — they share the global advisory-lock namespace and could block the leader key. This is a liveness-only attack (no data leak or privilege escalation), but it permanently starves the control plane.

## Helm configuration

The default `values.yaml` ships an HA-ready server section:

```yaml
server:
  replicas: 2
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxSurge: 1
      maxUnavailable: 0
  terminationGracePeriodSeconds: 60       # drain in-flight HTTP/WS
  lifecycle:
    preStop:
      exec:
        command: ["/bin/sh", "-c", "sleep 10"]  # drain LB endpoints first
  topologySpreadConstraints:
    - maxSkew: 1
      topologyKey: kubernetes.io/hostname
      whenUnsatisfiable: ScheduleAnyway
      labelSelector:
        matchLabels:
          app.kubernetes.io/component: server
  podDisruptionBudget:
    enabled: true
    minAvailable: 1
```

When auth is enabled, set the JWT secrets via `extraSecretEnv` so every pod loads the same values:

```yaml
server:
  extraSecretEnv:
    STROEM__AUTH__JWT_SECRET: "share-this-across-all-replicas"
    STROEM__AUTH__REFRESH_SECRET: "and-this-one-too"
```

## EKS Spot considerations

- Run the `stroem-server` Deployment on Spot — the HA model survives pod eviction. The 60-second `terminationGracePeriodSeconds` drains in-flight HTTP and WebSocket connections; the 10-second `preStop sleep` gives the Service controller time to remove the pod from the load-balancer's target group first.
- Spread replicas across nodes (and ideally across availability zones) with `topologySpreadConstraints`. The defaults spread by `kubernetes.io/hostname`; for AZ spread add a second entry with `topologyKey: topology.kubernetes.io/zone`.
- Workers can also run on Spot — the recovery sweeper already handles dead workers (stale heartbeat → steps failed → orchestration cascades).

## Failover rehearsal

```bash
# Find the current leader (use /healthz/detail with worker token; the
# unauthenticated /healthz endpoint deliberately omits leader identity).
TOKEN="$(kubectl get secret stroem-server-env -o jsonpath='{.data.STROEM__WORKER_TOKEN}' | base64 -d)"
for pod in $(kubectl get pods -l app.kubernetes.io/component=server -o name); do
  echo "=== $pod ==="
  kubectl exec "$pod" -- wget -qO- \
    --header="Authorization: Bearer $TOKEN" \
    http://localhost:8080/healthz/detail | jq '.checks.leader'
done

# Kill the leader
kubectl delete pod <leader-pod> --grace-period=60

# Verify the other pod takes over within ~10s
sleep 15
kubectl exec <surviving-pod> -- wget -qO- \
  --header="Authorization: Bearer $TOKEN" \
  http://localhost:8080/healthz/detail | jq '.checks.leader'
# Expect: true
```

## Known limitations

- **Single log line > ~7 KB** is delivered as a signal-only NOTIFY between replicas. The viewer on a non-receiving replica sees no live content until refresh — at which point the next-best source applies (disk on the receiving replica, or the archive after job completion). For workloads that emit large individual lines (e.g. JSON blobs over 7 KB), mount a shared log volume (RWX EFS / NFS) and serve all replicas from the same `log_storage.local_dir`.
- **Postgres outage = no scheduler.** Both replicas lose leadership when the DB goes down. This is by design — workflow execution can't proceed without the DB anyway.
- **NOTIFY message loss during listener reconnect.** The PgListener's reconnection window (up to ~2 seconds for transient blips, longer for fatal errors requiring the outer recovery loop) silently drops notifications fired during the gap. Each channel has fallback recovery:
  - `stroem_job_cancelled`: The recovery sweeper re-converges within ~60 seconds (DB as source of truth).
  - `stroem_workspace_reloaded`: The receiving replica's own poll cycle (default 30s) re-converges to the new revision.
  - `stroem_job_log_chunk`: A missed line is lost from live tail; backfill from disk or archive recovers it after job completion.
- **Rate limiter (Tower Governor) is per-replica.** Login / refresh / signup limits are enforced per pod, so effective cluster-wide limits are roughly `replicas × per-pod-limit`. Acceptable for current security posture; if you need exact global limits, terminate at the Ingress layer or front the cluster with a CDN-level rate limiter.
