---
title: Prometheus Metrics
description: Scrape Strøm server metrics for capacity planning, HA monitoring, and alerting.
---

Strøm exposes a Prometheus-compatible `/metrics` endpoint on every server replica.

## Endpoint

- **URL:** `https://<your-server>/metrics`
- **Method:** `GET`
- **Content-Type:** `text/plain; version=0.0.4`
- **Default auth:** `Authorization: Bearer <worker_token>` (same token as `/healthz/detail`)
- **Public mode:** set `metrics: { public: true }` in `server-config.yaml` to disable auth

When running multiple replicas (see [High Availability](./high-availability)),
every metric carries a `replica_id` label so per-pod series don't collide.

## Metric Reference

### Counters

| Name | Labels | Description |
|---|---|---|
| `stroem_http_requests_total` | `method`, `route`, `status` | Requests served on `/api/*`. `route` uses the matched Axum pattern (e.g. `/api/jobs/{id}`), not raw URIs. |
| `stroem_jobs_created_total` | `source_type` | Jobs created. `source_type` is one of `api`, `user`, `scheduler`, `webhook`, `event_source`, `hook`, `task`, `retry`, `rerun`, `restart`. |
| `stroem_jobs_completed_total` | `status` | Jobs reaching terminal state. `status` is `completed`, `failed`, `cancelled`, or `skipped`. |

### Histogram

| Name | Labels | Buckets |
|---|---|---|
| `stroem_http_request_duration_seconds` | `route` | `0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10` seconds |

### Gauges (sampled at scrape time)

| Name | Labels | Source |
|---|---|---|
| `stroem_leader_status` | — | `1` if this replica holds the HA leader lock, else `0` |
| `stroem_workers_active` | — | Count of workers with `status = 'active'` |
| `stroem_jobs_in_flight` | `status` | Pending and running jobs (both buckets always emitted) |
| `stroem_steps_ready` | — | Claimable steps (`status = 'ready'`, `retry_at` past or null) |
| `stroem_background_task_alive` | `task` | `1` if the named loop is running. Labels: `scheduler`, `recovery`, `event_source` |

## Prometheus Scrape Config

### Default (Bearer auth)

```yaml
scrape_configs:
  - job_name: stroem
    metrics_path: /metrics
    scheme: https
    bearer_token_file: /var/run/secrets/stroem/worker-token
    static_configs:
      - targets: ['stroem.internal:443']
```

### Public mode (no auth)

```yaml
scrape_configs:
  - job_name: stroem
    metrics_path: /metrics
    static_configs:
      - targets: ['stroem.internal:80']
```

### Helm + Prometheus Operator

Set in `values.yaml`:

```yaml
serviceMonitor:
  enabled: true
  bearerTokenSecret:
    name: stroem-worker-token  # the secret backing STROEM__WORKER_TOKEN
    key: token
```

## Starter Alerts

```yaml
groups:
  - name: stroem
    rules:
      - alert: StroemNoLeader
        expr: max(stroem_leader_status) == 0
        for: 1m
        annotations:
          summary: "No Strøm replica is the HA leader"

      - alert: StroemNoActiveWorkers
        expr: max(stroem_workers_active) < 1
        for: 2m
        annotations:
          summary: "No active Strøm workers — jobs will not be claimed"

      - alert: StroemQueueGrowing
        expr: |
          rate(stroem_steps_ready[5m])
          > rate(stroem_jobs_completed_total[5m])
        for: 10m
        annotations:
          summary: "Strøm step queue growing faster than completion rate"
```

## Notes

- Gauges that fail to sample (DB timeout, transient error) are **not** fabricated as zero — the sample is skipped, which Prometheus correctly treats as stale. Don't write alerts that assume "missing means zero".
- Per-workspace and per-task labels are deliberately **not** present in v1 (cardinality risk). Future opt-in is on the roadmap.
- `/worker/*` polling traffic and `/hooks/*` are deliberately **excluded** from `stroem_http_requests_total` to keep RED signals focused on user-facing API latency.
