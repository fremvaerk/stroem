# Prometheus Metrics Endpoint — Design

**Date:** 2026-05-19
**Status:** Approved
**Author:** Anatolii Lapytskyi
**Related:** Closes "No metrics/Prometheus endpoint — capacity planning blind" in `docs/internal/TODO.md`. Natural follow-up to multi-replica HA (v0.15.11).

## Goal

Expose a Prometheus-scrape-compatible `/metrics` endpoint on each Strøm server replica so operators can monitor HA health, request flow, and job throughput. v1 deliberately small: a lean inventory that answers "is HA working?" and "are jobs flowing?". Worker-side metrics and per-task cardinality are explicitly deferred.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Scope | Server only | Smallest surface; worker metrics is a follow-up once we know what operators want |
| Auth | Configurable, default `worker_token` Bearer | Matches the post-HA-review precedent that public endpoints leak nothing |
| Library | `metrics = "0.24"` + `metrics-exporter-prometheus = "0.18"` | Facade pattern keeps recording sites clean; swap backends later if needed |
| Inventory | Lean (9 metrics) | Covers RED + HA essentials; cardinality-safe by construction |
| Default state | Enabled | Zero-config scraping works out of the box, matches `/healthz` |
| Recording strategy | Hybrid: counters/histograms inline, gauges pulled at scrape time | Gauges always reflect truth; counters can't be reconstructed from snapshots |

## Architecture

### Modules

**`crates/stroem-server/src/metrics.rs`** (new) — installation + gather logic:
- `install_recorder(replica_id: Uuid) -> Result<PrometheusHandle>` — called once from `main.rs` at startup. Installs the global `metrics` facade recorder with `replica_id` as a global label. Configures histogram buckets for `http_request_duration_seconds`.
- `gather_gauges(state: &AppState)` — async, runs the pull-mode DB queries via `tokio::try_join!`. Each query is bounded by `tokio::time::timeout(Duration::from_secs(2), …)`. On error/timeout the corresponding gauge is left unset for this scrape (no fabricated zeros).
- `pub const` metric names (`STROEM_HTTP_REQUESTS_TOTAL`, etc.) — single source of truth shared between recording sites and tests.

**`crates/stroem-server/src/web/metrics.rs`** (new) — HTTP handler:
- `pub async fn metrics_handler(State(state), Extension(handle)) -> Result<Response, AppError>` — calls `gather_gauges(&state).await`, then `handle.render()`, returns body with `Content-Type: text/plain; version=0.0.4`.

**`crates/stroem-server/src/web/mod.rs`** (modified) — route wiring parallel to the `/healthz` split:
- If `config.metrics.public == true` → registered without auth (same shape as `/healthz`).
- Else (default) → wrapped in `worker_api::auth_middleware` (same shape as `/healthz/detail`).

**`crates/stroem-server/src/web/api/mod.rs`** (modified) — add a tower middleware layer that records `stroem_http_requests_total` and `stroem_http_request_duration_seconds` for every `/api/*` request using the matched Axum route pattern (not raw URI) for the `route` label.

**HTTP middleware scope:** the RED middleware wraps **only** the `/api/*` tree. The `/worker/*` path is the worker→server polling firehose (high RPS, internal signal) and would swamp user-facing latency stats; `/hooks/*` and `/mcp/*` are noisy external surfaces with their own semantics. Operators who need those can extend the layer later — out of scope for v1.

### Config

New optional field on `ServerConfig`:

```yaml
metrics:
  public: false  # default: requires worker_token Bearer header
```

`MetricsConfig { public: bool }` lives in `crates/stroem-server/src/config.rs`. Absent block = defaults (enabled, auth required). No `enabled: bool` flag since we agreed metrics are always-on.

### Dependencies

Added to `crates/stroem-server/Cargo.toml`:

```toml
metrics = "0.24"
metrics-exporter-prometheus = { version = "0.18", default-features = false }
```

`default-features = false` drops `http-listener` and `push-gateway` (we serve via existing Axum router, not a separate hyper server).

## Metric Inventory

All metrics carry a `replica_id` global label so multi-replica scrapes don't collide.

### Inline (recorded at event sites)

| Name | Type | Labels | Recording site |
|---|---|---|---|
| `stroem_http_requests_total` | counter | `method`, `route`, `status` | tower middleware on `/api/*` |
| `stroem_http_request_duration_seconds` | histogram | `route` | same middleware |
| `stroem_jobs_created_total` | counter | `source_type` | `job_creator.rs::create_job_for_task_inner` |
| `stroem_jobs_completed_total` | counter | `status` | `job_recovery.rs::handle_job_terminal` |

- `source_type` values: `api` / `user` / `scheduler` / `webhook` / `event_source` / `hook` / `task` / `retry` / `rerun` / `restart` (mirrors DB `CHECK` constraint).
- `status` values for `jobs_completed_total`: `completed` / `failed` / `cancelled` / `skipped`.
- Histogram buckets for `http_request_duration_seconds`: `[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10]` (Prometheus standard).
- `route` label uses the matched Axum route pattern (`/api/jobs/:id`), **not** the raw URI — keeps cardinality bounded per route, not per UUID.

### Pull-mode (sampled at scrape time)

| Name | Type | Labels | Source |
|---|---|---|---|
| `stroem_leader_status` | gauge | — | `state.leader.is_leader()` → `0` or `1` |
| `stroem_workers_active` | gauge | — | `SELECT COUNT(*) FROM worker WHERE status = 'active'` |
| `stroem_jobs_in_flight` | gauge | `status` | `SELECT status, COUNT(*) FROM job WHERE status IN ('pending','running') GROUP BY status` |
| `stroem_steps_ready` | gauge | — | `SELECT COUNT(*) FROM job_step WHERE status = 'ready' AND (retry_at IS NULL OR retry_at <= NOW())` |
| `stroem_background_task_alive` | gauge | `task` | `BackgroundTasks` atomic flags loaded directly: `scheduler_alive`, `recovery_alive`, `event_source_alive` (same pattern as `web/health.rs`) — values `0` or `1` |

### Explicitly out of scope for v1

- Per-workspace / per-task labels (cardinality risk — a 500-task workspace × 10 statuses = 5k series per metric)
- NOTIFY publish/receive counters per channel (medium tier)
- Step counters by `action_type` (medium tier)
- Worker-side metrics (separate scope decision)
- Retry attempt distribution histograms
- Tool-call counters for agent steps

## Data Flow

```
Prometheus scrape
        │
        ▼
GET /metrics
        │
        ├─[if metrics.public=false]─► worker_api::auth_middleware
        │                                  │ (401 on missing/invalid Bearer)
        ▼                                  ▼
metrics_handler
        │
        ├──► gather_gauges(&state)
        │       │
        │       ├──► tokio::try_join!(
        │       │       timeout(2s, worker_count_query),
        │       │       timeout(2s, jobs_in_flight_query),
        │       │       timeout(2s, steps_ready_query),
        │       │   )
        │       │
        │       ├──► state.leader.is_leader()              → gauge!(stroem_leader_status)
        │       ├──► state.background_tasks.is_alive(…)    → gauge!(stroem_background_task_alive)
        │       │
        │       └──► on err/timeout: log warn, skip gauge update (do NOT set 0)
        │
        ├──► handle.render()                               → Prometheus text format
        │
        └──► Response { Content-Type: text/plain; version=0.0.4 }


Inline recording (always live, parallel to scrape flow):

   tower middleware → counter!(stroem_http_requests_total, …).increment(1)
                    → histogram!(stroem_http_request_duration_seconds, …).record(d)

   job_creator      → counter!(stroem_jobs_created_total, source_type=…).increment(1)
   job_recovery     → counter!(stroem_jobs_completed_total, status=…).increment(1)
```

## Error Handling

- `gather_gauges` query error or 2s timeout: log at `warn` with query name and error, skip that gauge update. Prometheus correctly treats missing values as stale — fabricated zeros would mask the failure.
- `handle.render()` failure (extremely unlikely with in-memory map): `AppError::Internal` → 500 with generic message via existing handler.
- Recorder install failure at startup: fatal. `main.rs` propagates the error with context; same posture as a failed DB connection.
- Hot-path overhead: counter/histogram recording is lock-free atomic on a sharded recorder. No measurable impact on request latency.

## Helm

**New values** (`helm/stroem/values.yaml`):

```yaml
serviceMonitor:
  enabled: false
  interval: 30s
  scrapeTimeout: 10s
  additionalLabels: {}

metrics:
  public: false
  # When public=false, ServiceMonitor reads the worker token from
  # the same secret that backs STROEM__WORKER_TOKEN.
```

**New template** `helm/stroem/templates/servicemonitor.yaml` — renders only when `serviceMonitor.enabled: true`. Scrapes the existing `http` port at `/metrics`. When `metrics.public: false`, includes `bearerTokenSecret` ref to the worker-token secret.

**Removed**: the commented-out `ServiceMonitor` example in `values.yaml` lines 261–277 (now first-class).

## Documentation

- New page `docs/src/content/docs/operations/metrics.md` containing:
  - Endpoint URL + auth modes (default and `public: true`)
  - Full metric reference table (mirrors §Metric Inventory)
  - Example Prometheus `scrape_config` for both auth modes
  - 3 starter alerting recipes: (a) no leader across replicas for >1m, (b) `stroem_workers_active < 1` for >2m, (c) `stroem_steps_ready` growing faster than `rate(stroem_jobs_completed_total[5m])` for >10m
  - Reference to Helm `serviceMonitor.enabled`
- Cross-link from `docs/src/content/docs/operations/high-availability.md`: "Monitor leader flips via `stroem_leader_status` — see the [Metrics](./metrics) guide."
- `docs/internal/TODO.md`: mark `[x]` "No metrics/Prometheus endpoint" and add a "Review: Prometheus Metrics (2026-05-19)" section after implementation.
- `CLAUDE.md`: add a "Metrics" entry under Key Patterns describing the recorder install path and where to add new metrics.

## Testing

### Unit (in-module)

- `metric_name_constants_match_spec` — guard against silent rename drift
- `install_recorder_returns_handle` — happy path
- `gather_gauges_skips_on_db_error` — pool returns error, function returns Ok, gauge is unset
- `gather_gauges_respects_timeout` — slow query (mocked) is bounded by 2s

### Integration — `crates/stroem-server/tests/metrics_test.rs` (new)

- `metrics_public_endpoint_no_auth` — `public: true` → 200 without Bearer
- `metrics_default_requires_worker_token` — no Bearer → 401
- `metrics_with_valid_token_returns_text_format` — 200 + `text/plain; version=0.0.4`, body starts with `# HELP`
- `jobs_created_counter_increments_after_creation` — create job via test fixture, scrape, assert delta on `stroem_jobs_created_total{source_type="api"}`
- `jobs_completed_counter_increments_after_completion` — drive a job to `completed`, scrape, assert delta on `stroem_jobs_completed_total{status="completed"}`
- `leader_status_reflects_leadership` — two test replicas; assert `stroem_leader_status` is `1` on leader, `0` on follower
- `steps_ready_gauge_reflects_db_state` — insert ready steps, scrape, assert gauge value matches
- `jobs_in_flight_gauge_groups_by_status` — insert pending + running jobs, assert two series with correct values
- `gather_gauges_survives_db_error` — drop pool mid-scrape, render still succeeds (inline counters still present, gauges absent)
- `route_label_is_matched_pattern_not_raw_uri` — hit `/api/jobs/{uuid}` twice with different UUIDs, assert single series with `route="/api/jobs/:id"`
- `background_task_alive_drops_to_zero_on_guard_drop` — drop scheduler's `AliveGuard`, scrape, assert `stroem_background_task_alive{task="scheduler"}` is `0`

### Validation

- CI step pipes one scraped response through `promtool check metrics -` (Prometheus's own linter) to catch malformed exposition output. Added to `.github/workflows/ci.yml` as a small bash step in the integration-test job.

## Out of Scope (explicit non-goals)

- Worker-side `/metrics` endpoint (own scope decision)
- Push-gateway / OpenTelemetry export (use facade swap later if needed)
- Per-workspace / per-task labels (cardinality)
- Tracing exporters (separate feature)
- Grafana dashboard JSON shipped in-tree (docs link to community examples for now)
- Metric retention / aggregation server-side (Prometheus's job)

## Open Questions

None — all decisions made during brainstorming.

## Build Sequence (preview, written-out in implementation plan)

1. Add deps to `stroem-server/Cargo.toml`; introduce `metrics.rs` module with `install_recorder` + metric name consts (no recording yet)
2. Wire `install_recorder` into `main.rs`; add `MetricsConfig` to `config.rs`
3. Add `web/metrics.rs` handler + route wiring in `web/mod.rs` (with both auth modes)
4. Add tower middleware for HTTP RED metrics on `/api/*`
5. Add recording sites in `job_creator.rs` and `job_recovery.rs`
6. Implement `gather_gauges` with `tokio::try_join!` + per-query timeouts
7. Integration tests
8. Helm `serviceMonitor` template + values
9. Docs page + cross-links + CLAUDE.md update
10. CI `promtool` validation step
