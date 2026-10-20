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

The replica that acquires the lock becomes the **leader** and runs all three tasks. Followers run the loops but skip the work each tick (so they're warm and can take over instantly). If the leader's connection drops (pod restart, network blip, DB restart) Postgres releases the lock automatically, and a follower acquires it on its next attempt — within about 5 s (followers try every `leader.lease_secs / 6`, at most 5 s apart; see [Leader settings](#leader-settings)). No external coordinator (etcd, ZooKeeper, Raft) is required.

### When the leader stops working

A session-level advisory lock lives as long as the database session. A leader process that is frozen or crawling — on a node that Kubernetes can no longer reach but whose kernel still answers TCP — would keep that session, and the lock, for as long as the node lives. (This happened in production: a node went silent, its leader pod kept the lock, and no replica was leader for 16.5 hours; every cron trigger in that window was missed.) Three mechanisms bound it:

- **Lease.** The leader session runs with `idle_session_timeout` set to the lease, `leader.lease_secs` (default 30 s; the numbers below are for the defaults). The leader pings its session every 5 s, so a healthy leader is never idle that long; when the process stops talking, PostgreSQL ends the session after 30 s and the lock is free. A follower takes over on its next attempt — about 35 s after the old leader's last answered ping, at most 55 s when the database answers slowly. This assumes a working network: on a degraded one, a ping the leader sent just before it froze can reach the database late and restart the 30 s from its arrival (at most once, and no later than the leader node's TCP retransmission limit, about 15 minutes on Linux). From the lock query on, the leader session uses only single-message (simple-protocol) queries, so a request cut off in transit cannot leave the session waiting without the timer.
- **Validity.** A replica reports itself leader only while its last successful ping was sent less than two thirds of the lease ago (20 s), and a ping that takes longer than 5 s drops the connection. A process that thaws after its session was ended does not act on an old belief, and a leader cut off from the database stops acting as leader within seconds.
- **Self-release.** When the scheduler, recovery sweeper or event-source loop on the leader has been **stalled**, or ran and then **exited** (the `/livez` rules below), for `leader.self_release_after_secs` (default 30 s), the leader releases the lock and does not take it again while that lasts. A follower takes over. A loop that exited never comes back within the process, so that replica stays a follower; its `/livez` fails, and the kubelet restarts it.

The lease needs **PostgreSQL 14 or later**, which Strøm already requires; on an older server the leader logs `Leader lease not enforced: the database does not support idle_session_timeout (PostgreSQL < 14)` once and runs without the lease (validity and self-release still apply).

#### Leader settings

Two settings, in the server config (see [`leader`](/getting-started/configuration/#leader)):

```yaml
leader:
  lease_secs: 30               # 10 to 3600
  self_release_after_secs: 30  # 0 = off, or 1 to 3600
```

Every other value comes from the lease, so the values cannot contradict each other:

| Value | From the lease `L` | Default |
|---|---|---|
| Ping interval and follower retry | `L / 6`, at most 5 s | 5 s |
| Ping timeout | `L / 6`, at most 5 s | 5 s |
| Acquire attempt timeout | `L / 3`, at most 10 s | 10 s |
| Validity | `2 × L / 3` | 20 s |

What the lease trades:

- **A smaller lease** gives a faster takeover of a frozen leader (about the lease plus one attempt). A leader whose process is starved for longer than two thirds of the lease stops acting as leader until its next ping, and one starved for longer than the lease loses the lock. Below 30 s, the capped values scale down too: a database answer slower than `L / 6` drops the leader connection, and each follower opens an attempt connection every `L / 6`.
- **A larger lease** gives a slower takeover of a frozen leader and more room for a starved leader process. It does not make the leader more tolerant of a slow database: a ping slower than 5 s still drops the connection.

Because of the caps, three values stay the same for every lease of 30 s or more:

- after a normal handover (a rolling deploy, a self-release), a follower takes the lock on its next attempt — within about 5 s, at most 25 s;
- a broken leader connection is noticed within 10 s;
- after the leader session ends early, the old leader can still believe it leads for up to 15 s.

Each replica uses its own values, so replicas with different settings (during a configuration rollout) are safe.

**Self-release and a single replica.** With one replica there is no follower to take over: after a release, no leader work runs until the loop recovers or the process restarts. With a liveness probe (the Helm default) the kubelet restarts the pod, as it did before. **Without** a liveness probe (Docker Compose, probes disabled), a loop that exited now stops ALL leader work — scheduler, recovery and event sources — until someone restarts the process, where before only that loop's work stopped. For such a deployment, set `leader.self_release_after_secs: 0`. That turns off the release and the refusal to acquire; the `/livez` rules stay the same.

When the lease ends a leader session, PostgreSQL logs `FATAL: terminating connection due to idle-session timeout` for the `stroem-leader` connection, and the old leader (if it ever runs again) logs `Leader lease expired: PostgreSQL ended the leader session …`. Neither is an error by itself — it is the lease doing its job.

**Who holds the lock.** Every follower reads the lock holder once a minute and logs it at INFO when it changes (and every 15 minutes): `Leader lock held by pid … (client 10.0.1.23, application stroem-leader, backend_start …, state idle, idle 2s)`. The client address is the leader pod's IP. A holder that looks stale — idle for longer than the lease, or not a Strøm leader — is logged at WARN every minute. The leader logs `Acquired leader lock (key=…, backend pid=…, lease=30s)`.

### Cross-replica events (Postgres LISTEN/NOTIFY)

Three things need to propagate across replicas immediately, not via the next poll cycle:

| Channel | Trigger | Receiver action |
|---|---|---|
| `stroem_job_cancelled` | API/UI cancellation on replica A | Replica B inserts into its local `cancelled_jobs` cache so workers polling B see the cancellation. |
| `stroem_workspace_reloaded` | Watcher on replica A detects a new revision | Replica B reloads its workspace config cache (otherwise it waits up to `poll_interval_secs`, default 30s). |
| `stroem_job_log_chunk` | Worker pushes logs to replica A | Replica B **mirrors the chunk into its own local jsonl file** and fans out to its local WebSocket viewers. |

Every payload carries the publishing replica's UUID; receivers drop their own messages to avoid duplicate broadcasts and double-writes.

For `stroem_job_log_chunk`, Postgres caps `NOTIFY` payloads at ~8 KB. Multi-line batches larger than ~3.5 KB are **split on jsonl line boundaries** into multiple NOTIFY events so every replica receives the full bytes — REST log reads (`GET /api/jobs/:id/logs`) served from any pod return the same content. A single jsonl line that is itself larger than ~3.5 KB can't be split and degrades to a signal-only NOTIFY for that one line: receivers learn fresh content exists but the bytes only live on the publishing replica until the job completes and the archive upload runs.

### Health endpoints

Three endpoints, split by audience:

- **`GET /livez`** (unauthenticated, **liveness** probe). Answers "should this process be restarted?". Returns 503 when a background loop (scheduler, recovery sweeper, event-source manager) is **stalled** — still running but it has made no progress for too long (scheduler 5 min, event sources 10 min, recovery 30 min or 5× `sweep_interval_secs`) — or when a loop ran and then exited — on any replica (a follower whose loop exited cannot do that loop's work as leader, and with self-release it never becomes leader again). It does **not** check the database: restarting the server does not fix a database outage. Body is `{"status": "ok" | "stalled" | "stopped"}` — no task name and no leader identity.
- **`GET /healthz`** (unauthenticated, **readiness** probe). Answers "can this replica serve traffic?". Returns 200 if the database is reachable, 503 otherwise. Body is minimal — `{"status": "ok" | "unhealthy", "db": "ok" | "error"}` — and contains **no** leader identity or per-task liveness, so unauthenticated scanners can't preferentially target the leader pod for DoS.
- **`GET /healthz/detail`** (auth required — `Authorization: Bearer <worker_token>`). Returns the full HA diagnostic shape: `status`, `checks.db`, `checks.leader`, `checks.scheduler`, `checks.recovery`, `checks.event_source`. Each task is `"ok"`, `"stalled"` (503 on any replica), or — when the task is not running — `"stopped"` (503) on the leader or when the task ran before, and `"follower"` (200) for a task that never started on a follower.

The split is deliberate: a hung scheduler gets the pod restarted through `/livez`, but does not take a replica that is still serving API, WebSocket and worker traffic out of the Service. The default Helm values point `livenessProbe` at `/livez` and `readinessProbe` at `/healthz`; if you override the probes, keep them on different endpoints. `/livez` requires a server image that has it: an older server answers unknown paths with the UI's `index.html` (HTTP 200), so a newer chart probing an older pinned image silently has no effective liveness check — keep chart and image versions paired.

## Required infrastructure

| Component | Requirement |
|---|---|
| **Postgres** | PostgreSQL 14 or later (the leader lease uses `idle_session_timeout`), reachable from every replica (single primary; RDS Multi-AZ recommended for prod). All HA coordination flows through it. A transaction-pooling proxy (PgBouncer in transaction mode) between Strøm and Postgres breaks the session-level leader lock. |
| **Log archive (S3 / EFS / local PVC mounted RWX)** | Optional but strongly recommended for **finished-job** logs. While a job is running, the NOTIFY listener mirrors chunks onto every replica's local disk, so REST reads work from any pod. After the job ends, the local file is only uploaded to the archive on the replica that ran the job; without an archive, replicas that didn't mirror every chunk (e.g. cold-started after the job ran) return partial or empty logs. With S3 configured, every replica reads finished-job logs from the shared bucket. |
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

## Node failures

The lease covers a leader whose node stops working but is never replaced. Two cluster-side settings shorten such an outage further and are recommended:

- **Karpenter node repair.** With `NodeRepair=false`, Karpenter leaves a `NotReady` node in place; in the production incident the silent node ran for 16 hours. Enable node repair (feature gate `NodeRepair=true`) so unhealthy nodes are replaced.
- **TCP keepalives on the database side** (RDS: `tcp_keepalives_idle`, `tcp_keepalives_interval`, `tcp_keepalives_count`) only close a session whose peer stops answering. A frozen node whose kernel still answers keeps the session alive indefinitely — that is the case the lease exists for. Keepalive tuning is not a substitute.

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

# Verify the other pod takes over. On a prompt network this takes a few
# seconds: a follower tries every lease_secs / 6, at most 5 s apart (see "Leader settings").
sleep 15
kubectl exec <surviving-pod> -- wget -qO- \
  --header="Authorization: Bearer $TOKEN" \
  http://localhost:8080/healthz/detail | jq '.checks.leader'
# Expect: true
```

## Known limitations

- **Single log line > ~3.5 KB** is delivered as a signal-only NOTIFY between replicas (the cap accounts for Postgres' 8 KB NOTIFY hard limit plus JSON-escape inflation on the wrapper). Multi-line batches above the cap are split on jsonl boundaries and delivered fully; only a single line that exceeds the cap on its own can't be split. The viewer on a non-receiving replica sees no live content for that line until refresh — at which point the next-best source applies (disk on the receiving replica, or the archive after job completion). For workloads that emit very large individual lines (e.g. JSON blobs over 3.5 KB), mount a shared log volume (RWX EFS / NFS) and serve all replicas from the same `log_storage.local_dir`.
- **Postgres outage = no scheduler.** Both replicas lose leadership when the DB goes down. This is by design — workflow execution can't proceed without the DB anyway.
- **NOTIFY message loss during listener reconnect.** The PgListener's reconnection window (up to ~2 seconds for transient blips, longer for fatal errors requiring the outer recovery loop) silently drops notifications fired during the gap. Each channel has fallback recovery:
  - `stroem_job_cancelled`: The recovery sweeper re-converges within ~60 seconds (DB as source of truth).
  - `stroem_workspace_reloaded`: The receiving replica's own poll cycle (default 30s) re-converges to the new revision.
  - `stroem_job_log_chunk`: A missed segment is absent from the replica's mirror, so REST reads served from that pod will be short until backfill (manual `getJobLogs` refetch after the live tail catches up, or archive read after job completion). The publishing replica's local file is always complete.
- **Pinned commits for [git refs](/guides/git-refs/) are per replica.** Each replica keeps its own [pin store](/getting-started/configuration/#pin_store) (never share `pin_store.dir` between replicas — the second one refuses to start) and its own branch/tag listing, so two jobs created on different replicas within one `poll_interval_secs` window can resolve a branch to different commits. A step claimed on a replica that has not loaded its commit yet is released and retried while that replica loads it — up to 30 times, each cycle taking up to `pin_store.claim_load_budget_secs` + 10 s + the worker's next poll (a little over 5 minutes in all when the git server fails fast, about 15 minutes or more when it hangs; see [Git Refs → Claim](/guides/git-refs/#claim)); during a git outage, job detail of a job connected to such a commit can answer `503` on that replica only.
- **Missed and duplicate cron fires during a handover.** Followers advance cron triggers without firing them, so a trigger that comes due between the moment the old leader stops firing and the moment the new leader takes over is never fired (after a frozen leader: up to `lease + 2 × acquire timeout + retry`, about 55 s with the default lease on a working network, see [When the leader stops working](#when-the-leader-stops-working); after a stalled or exited scheduler: the stall, or the exit, plus `self_release_after_secs`, plus one attempt). There is no replay of missed fires yet. In rare cases (the new leader takes over within milliseconds of a due time while the old leader is still firing it) one occurrence can fire twice.
- **Overlap after an early session end.** When the leader session ends early — an administrator terminates it, the connection is reset — a follower can take over at once while the old leader still believes it leads for up to half the lease, at most 15 s (never longer than the validity, two thirds of the lease). Work it starts in that window runs alongside the new leader's.
- **Rate limiter (Tower Governor) is per-replica.** Login / refresh / signup limits are enforced per pod, so effective cluster-wide limits are roughly `replicas × per-pod-limit`. Acceptable for current security posture; if you need exact global limits, terminate at the Ingress layer or front the cluster with a CDN-level rate limiter.

## Monitoring leader flips

Each replica exposes `stroem_leader_status` (1 on the leader, 0 on followers)
via the [Metrics endpoint](./metrics) (not exported when `metrics.public` is
`true`). Alert on a cluster with no leader:

```yaml
- alert: StroemNoLeader
  expr: sum(stroem_leader_status) == 0
  for: 5m
```

Rapid changes in `changes(stroem_leader_status[5m])` indicate flapping. When
the alert fires, the follower logs name the lock holder (see
[When the leader stops working](#when-the-leader-stops-working)).
