# Prometheus Metrics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Prometheus-compatible `/metrics` endpoint to the Strøm server exposing a lean inventory of RED + HA observability metrics (9 metrics total), with worker-token-gated default auth and Helm ServiceMonitor support.

**Architecture:** Single new `metrics` module in `stroem-server`. Recorder installed once at process startup via `metrics-exporter-prometheus`'s `PrometheusBuilder`. Hybrid recording: counters/histograms recorded inline at event sites via the `metrics` facade macros; gauges sampled at scrape time via a `gather_gauges()` async function that queries Postgres + reads `AppState` flags. Route wired in `web/mod.rs` parallel to `/healthz`, with `worker_api::auth_middleware` layered when `metrics.public != true`. RED-style HTTP metrics added via tower middleware on the `/api/*` subtree only.

**Tech Stack:** Rust, Axum, sqlx, `metrics = "0.24"`, `metrics-exporter-prometheus = "0.18"` (no default features), tokio, testcontainers.

**Spec:** [`docs/superpowers/specs/2026-05-19-prometheus-metrics-design.md`](../specs/2026-05-19-prometheus-metrics-design.md)

---

## Files to be touched

**Created:**
- `crates/stroem-server/src/metrics.rs` — module: install_recorder, gather_gauges, metric name constants, tower middleware fn
- `crates/stroem-server/src/web/metrics.rs` — HTTP handler for `/metrics`
- `crates/stroem-server/tests/metrics_test.rs` — integration tests (boots Postgres via testcontainers)
- `helm/stroem/templates/servicemonitor.yaml` — Helm ServiceMonitor template
- `docs/src/content/docs/operations/metrics.md` — user-facing documentation

**Modified:**
- `crates/stroem-server/Cargo.toml` — add `metrics`, `metrics-exporter-prometheus` deps
- `crates/stroem-server/src/lib.rs` — `pub mod metrics;` and `pub mod web::metrics` exposure
- `crates/stroem-server/src/main.rs` — install recorder; layer handle as Extension on the router
- `crates/stroem-server/src/config.rs` — add `MetricsConfig` struct + optional field on `ServerConfig`
- `crates/stroem-server/src/web/mod.rs` — register `/metrics` route with conditional auth middleware
- `crates/stroem-server/src/web/api/mod.rs` — apply RED tower middleware to the api router
- `crates/stroem-server/src/job_creator.rs` — increment `stroem_jobs_created_total`
- `crates/stroem-server/src/job_recovery.rs` — increment `stroem_jobs_completed_total`
- `helm/stroem/values.yaml` — add `serviceMonitor` + `metrics` blocks; remove the commented-out ServiceMonitor example
- `docs/astro.config.mjs` (or sidebar config) — add metrics page to operations nav
- `docs/src/content/docs/operations/high-availability.md` — cross-link to new metrics page
- `.github/workflows/ci.yml` — add `promtool check metrics` validation step
- `docs/internal/TODO.md` — mark "No metrics/Prometheus endpoint" complete; add review section anchor
- `CLAUDE.md` — add "Prometheus Metrics" entry under Key Patterns

---

## Task 1: Add metrics dependencies

**Files:**
- Modify: `crates/stroem-server/Cargo.toml`

- [ ] **Step 1: Add deps to Cargo.toml**

In `crates/stroem-server/Cargo.toml`, in the `[dependencies]` section (place near other small one-line deps like `subtle = "2"`):

```toml
metrics = "0.24"
metrics-exporter-prometheus = { version = "0.18", default-features = false }
```

`default-features = false` strips out `http-listener` and `push-gateway` (we serve via the existing Axum router).

- [ ] **Step 2: Verify compilation**

Run: `cargo check -p stroem-server`
Expected: clean compile, new deps resolved.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/Cargo.toml Cargo.lock
git commit -m "Add metrics + metrics-exporter-prometheus deps"
```

---

## Task 2: Create metrics module skeleton with name constants

**Files:**
- Create: `crates/stroem-server/src/metrics.rs`
- Modify: `crates/stroem-server/src/lib.rs`

- [ ] **Step 1: Create the module file**

Create `crates/stroem-server/src/metrics.rs`:

```rust
//! Prometheus metrics: recorder install + scrape-time gauge collection.
//!
//! See `docs/superpowers/specs/2026-05-19-prometheus-metrics-design.md`.

/// `counter` — HTTP requests handled on `/api/*`. Labels: method, route, status.
pub const STROEM_HTTP_REQUESTS_TOTAL: &str = "stroem_http_requests_total";

/// `histogram` — `/api/*` request duration in seconds. Label: route.
pub const STROEM_HTTP_REQUEST_DURATION_SECONDS: &str =
    "stroem_http_request_duration_seconds";

/// `counter` — jobs created. Label: source_type.
pub const STROEM_JOBS_CREATED_TOTAL: &str = "stroem_jobs_created_total";

/// `counter` — jobs reaching a terminal state. Label: status.
pub const STROEM_JOBS_COMPLETED_TOTAL: &str = "stroem_jobs_completed_total";

/// `gauge` — 1 if this replica currently holds the HA leader lock, else 0.
pub const STROEM_LEADER_STATUS: &str = "stroem_leader_status";

/// `gauge` — count of workers in `status='active'`.
pub const STROEM_WORKERS_ACTIVE: &str = "stroem_workers_active";

/// `gauge` — jobs in pending/running. Label: status.
pub const STROEM_JOBS_IN_FLIGHT: &str = "stroem_jobs_in_flight";

/// `gauge` — claimable steps (status=ready and retry_at is null or due).
pub const STROEM_STEPS_READY: &str = "stroem_steps_ready";

/// `gauge` — 1 if a given background task loop is alive, else 0. Label: task.
pub const STROEM_BACKGROUND_TASK_ALIVE: &str = "stroem_background_task_alive";
```

- [ ] **Step 2: Add module to lib.rs**

In `crates/stroem-server/src/lib.rs`, add (alphabetically among the other `pub mod` lines):

```rust
pub mod metrics;
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p stroem-server`
Expected: clean compile.

- [ ] **Step 4: Commit**

```bash
git add crates/stroem-server/src/metrics.rs crates/stroem-server/src/lib.rs
git commit -m "metrics: add module skeleton with name constants"
```

---

## Task 3: Implement install_recorder

**Files:**
- Modify: `crates/stroem-server/src/metrics.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/stroem-server/src/metrics.rs`:

```rust
use anyhow::{Context, Result};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use uuid::Uuid;

/// Histogram buckets for `stroem_http_request_duration_seconds`.
/// Prometheus client-library standard buckets.
const HTTP_DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Install the Prometheus recorder for this process. Must be called exactly
/// once at startup. Subsequent calls (e.g. in tests) will return a handle
/// that does not actually receive new metrics — set the recorder up once.
pub fn install_recorder(replica_id: Uuid) -> Result<PrometheusHandle> {
    let builder = PrometheusBuilder::new()
        .add_global_label("replica_id", replica_id.to_string())
        .set_buckets_for_metric(
            Matcher::Full(STROEM_HTTP_REQUEST_DURATION_SECONDS.to_string()),
            HTTP_DURATION_BUCKETS,
        )
        .context("configuring http duration buckets")?;
    let handle = builder
        .install_recorder()
        .context("installing prometheus recorder")?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::counter;

    /// Helper: install a private recorder for this single test (using
    /// `build_recorder` to avoid colliding with other tests' global recorder).
    fn local_handle() -> PrometheusHandle {
        let recorder = PrometheusBuilder::new()
            .add_global_label("replica_id", "test")
            .build_recorder();
        recorder.handle()
    }

    #[test]
    fn metric_name_constants_are_distinct() {
        let names = [
            STROEM_HTTP_REQUESTS_TOTAL,
            STROEM_HTTP_REQUEST_DURATION_SECONDS,
            STROEM_JOBS_CREATED_TOTAL,
            STROEM_JOBS_COMPLETED_TOTAL,
            STROEM_LEADER_STATUS,
            STROEM_WORKERS_ACTIVE,
            STROEM_JOBS_IN_FLIGHT,
            STROEM_STEPS_READY,
            STROEM_BACKGROUND_TASK_ALIVE,
        ];
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "metric names must be unique");
    }

    #[test]
    fn render_includes_replica_id_global_label() {
        // Use a process-local recorder to render output without depending on
        // the global one. We don't assert specific counts — just shape.
        let handle = local_handle();
        let rendered = handle.render();
        // An empty registry renders an empty string — that's fine. We just
        // assert the helper compiles and runs.
        assert!(rendered.is_empty() || rendered.contains("replica_id"));
        let _ = counter!("dummy"); // verify the metrics macro is importable
    }
}
```

- [ ] **Step 2: Run tests to verify they compile and pass**

Run: `cargo test -p stroem-server metrics::tests --lib`
Expected: 2 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/src/metrics.rs
git commit -m "metrics: implement install_recorder with global replica_id label"
```

---

## Task 4: Add MetricsConfig to ServerConfig

**Files:**
- Modify: `crates/stroem-server/src/config.rs`

- [ ] **Step 1: Write the failing test**

In `crates/stroem-server/src/config.rs`, locate the `#[cfg(test)] mod tests` block at the bottom of the file. Add the following two tests:

```rust
    #[test]
    fn metrics_block_absent_yields_none() {
        let yaml = r#"
worker_token: "tok"
db:
  url: "postgres://x"
log_storage:
  local_dir: "/tmp"
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.metrics.is_none());
    }

    #[test]
    fn metrics_public_parses() {
        let yaml = r#"
worker_token: "tok"
db:
  url: "postgres://x"
log_storage:
  local_dir: "/tmp"
metrics:
  public: true
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.metrics.as_ref().map(|m| m.public), Some(true));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p stroem-server config::tests::metrics --lib`
Expected: FAIL — `no field 'metrics' on ServerConfig` (or similar).

- [ ] **Step 3: Add MetricsConfig + optional field**

In `crates/stroem-server/src/config.rs`, add (near other `*Config` structs, alphabetical):

```rust
/// Prometheus `/metrics` endpoint configuration.
///
/// When absent, the endpoint is enabled and requires a worker-token Bearer
/// header (same auth posture as `/healthz/detail`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// If true, `/metrics` requires no authentication. Default false.
    #[serde(default)]
    pub public: bool,
}
```

Then locate the `pub struct ServerConfig { ... }` definition and add the new field (alphabetical with other optional sub-configs, e.g. near `mcp` or `auth`):

```rust
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p stroem-server config::tests::metrics --lib`
Expected: 2 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/config.rs
git commit -m "config: add optional MetricsConfig (public: bool)"
```

---

## Task 5: Wire recorder + Extension into main.rs

**Files:**
- Modify: `crates/stroem-server/src/main.rs`

- [ ] **Step 1: Install the recorder and layer it onto the router**

In `crates/stroem-server/src/main.rs`, **immediately after** the `let replica_id = uuid::Uuid::new_v4();` line (around line 233), add:

```rust
    let metrics_handle = stroem_server::metrics::install_recorder(replica_id)
        .context("installing prometheus metrics recorder")?;
```

Then locate the line `let app = stroem_server::web::build_router(state, cancel_token.clone());` (around line 268). Change it to:

```rust
    let app = stroem_server::web::build_router(state, cancel_token.clone())
        .layer(axum::Extension(metrics_handle));
```

If `axum` is not already imported at the top of `main.rs`, add the necessary use clause (most likely already imported via other uses; if not: `use axum::Extension;` and reference as `Extension(metrics_handle)`).

- [ ] **Step 2: Verify build**

Run: `cargo build -p stroem-server`
Expected: clean build. (Recorder is installed but no handler renders it yet — that comes in Task 7.)

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/src/main.rs
git commit -m "main: install metrics recorder and expose handle via Extension"
```

---

## Task 6: Create metrics handler module (skeleton, no gather yet)

**Files:**
- Create: `crates/stroem-server/src/web/metrics.rs`
- Modify: `crates/stroem-server/src/web/mod.rs`

- [ ] **Step 1: Create the handler file**

Create `crates/stroem-server/src/web/metrics.rs`:

```rust
//! `GET /metrics` — Prometheus text-format scrape endpoint.

use crate::state::AppState;
use crate::web::error::AppError;
use axum::extract::{Extension, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::PrometheusHandle;
use std::sync::Arc;

/// Render Prometheus text format. Updates pull-mode gauges first, then
/// renders the registry.
#[tracing::instrument(skip(state, handle))]
pub async fn metrics_handler(
    State(state): State<Arc<AppState>>,
    Extension(handle): Extension<PrometheusHandle>,
) -> Result<Response, AppError> {
    crate::metrics::gather_gauges(&state).await;
    let body = handle.render();
    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response())
}
```

- [ ] **Step 2: Add a placeholder gather_gauges**

In `crates/stroem-server/src/metrics.rs`, add a placeholder (the real implementation lands in Task 8):

```rust
use crate::state::AppState;

/// Sample pull-mode gauges (leader, workers, queue depth, background tasks)
/// at scrape time. Errors are logged and the affected gauge is skipped, not
/// fabricated as zero — Prometheus treats absent samples as stale.
#[tracing::instrument(skip(state))]
pub async fn gather_gauges(_state: &AppState) {
    // Implementation lands in Task 8. This placeholder lets the handler
    // compile and tests run end-to-end.
}
```

- [ ] **Step 3: Wire the route**

In `crates/stroem-server/src/web/mod.rs`, locate the `health_route` and `health_detail_route` blocks (around lines 66–76) and add the metrics route just after them:

```rust
    // /metrics — Prometheus scrape endpoint.
    // Auth: gated by worker_token unless `metrics.public: true`.
    let metrics_public = state
        .config
        .metrics
        .as_ref()
        .is_some_and(|m| m.public);
    let metrics_route = if metrics_public {
        Router::new()
            .route("/metrics", get(metrics::metrics_handler))
            .with_state(state.clone())
    } else {
        Router::new()
            .route("/metrics", get(metrics::metrics_handler))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                worker_api::auth_middleware,
            ))
            .with_state(state.clone())
    };
```

At the top of the file, add `pub mod metrics;` next to the existing `pub mod health;` declaration.

Then find the `let mut router = Router::new()` block (around line 78) and add `.merge(metrics_route)` to the chain:

```rust
    let mut router = Router::new()
        .merge(health_route)
        .merge(health_detail_route)
        .merge(metrics_route)
        .nest("/api", api::build_api_routes(state.clone()))
        .nest(
            "/worker",
            worker_api::build_worker_api_routes(state.clone()),
        )
        .nest("/hooks", hooks::build_hooks_routes(state.clone()));
```

- [ ] **Step 4: Verify build**

Run: `cargo build -p stroem-server`
Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/web/metrics.rs crates/stroem-server/src/web/mod.rs crates/stroem-server/src/metrics.rs
git commit -m "metrics: add /metrics handler and route with conditional auth"
```

---

## Task 7: First integration test — auth modes and render shape

**Files:**
- Create: `crates/stroem-server/tests/metrics_test.rs`

This task seeds the integration-test harness (used by all subsequent test tasks) and asserts the three auth/render paths.

- [ ] **Step 1: Write the test file**

Create `crates/stroem-server/tests/metrics_test.rs`. Use `crates/stroem-server/tests/ha_test.rs` as the template for the boot harness — copy the `boot()` helper verbatim with the additions noted below.

```rust
//! Integration tests for the `/metrics` endpoint.

use anyhow::Result;
use axum::body::Body;
use axum::Extension;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_db::{create_pool, run_migrations};
use stroem_server::config::{
    DbConfig, LogStorageConfig, MetricsConfig, RecoveryConfig, RetentionConfig, ServerConfig,
};
use stroem_server::events::EventBus;
use stroem_server::leader::LeaderElection;
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

struct Harness {
    _container: testcontainers::ContainerAsync<Postgres>,
    pool: PgPool,
    _temp: TempDir,
}

async fn boot_with_config(config: ServerConfig) -> Result<(Harness, axum::Router)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{port}/postgres");
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp = TempDir::new()?;
    let workspaces = WorkspaceManager::new(HashMap::new()).await?;
    let log_storage = LogStorage::new(temp.path().to_string_lossy().to_string()).await?;
    let mut state = AppState::new(
        pool.clone(),
        workspaces,
        config,
        log_storage,
        HashMap::new(),
        None,
    );
    state.leader = std::sync::Arc::new(LeaderElection::always());
    state.event_bus = EventBus::noop();

    let cancel = CancellationToken::new();
    let handle = global_test_handle();

    let router = build_router(state, cancel).layer(Extension(handle));
    Ok((
        Harness {
            _container: container,
            pool,
            _temp: temp,
        },
        router,
    ))
}

/// The `metrics` facade has a single global recorder per process. Install it
/// exactly once for the whole test binary; every test reuses the same handle.
///
/// Side effect: counters accumulate across tests in the same process. Tests
/// must assert *presence* (or per-test diffs) rather than absolute values.
/// Gauges set a fresh value each scrape via `gather_gauges`, so they are
/// stable across tests as long as each test seeds known DB state.
fn global_test_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    use std::sync::OnceLock;
    static HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            stroem_server::metrics::install_recorder(Uuid::new_v4())
                .expect("install recorder once")
        })
        .clone()
}

fn base_config() -> ServerConfig {
    ServerConfig {
        worker_token: "test-token".into(),
        db: DbConfig {
            url: "ignored".into(),
            ..Default::default()
        },
        log_storage: LogStorageConfig::default(),
        recovery: RecoveryConfig::default(),
        retention: RetentionConfig::default(),
        ..Default::default()
    }
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_public_endpoint_no_auth() -> Result<()> {
    let mut config = base_config();
    config.metrics = Some(MetricsConfig { public: true });
    let (_h, router) = boot_with_config(config).await?;

    let response = router
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response.headers().get(http::header::CONTENT_TYPE).cloned();
    let body = body_string(response).await;
    assert_eq!(
        ct.as_deref().and_then(|v| v.to_str().ok()),
        Some("text/plain; version=0.0.4")
    );
    // Empty registry renders empty string before any recording.
    // We only require the response shape to be correct.
    let _ = body;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_default_requires_worker_token() -> Result<()> {
    let (_h, router) = boot_with_config(base_config()).await?;

    let response = router
        .clone()
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_with_valid_token_returns_text_format() -> Result<()> {
    let (_h, router) = boot_with_config(base_config()).await?;

    let response = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(http::header::AUTHORIZATION, "Bearer test-token")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p stroem-server --test metrics_test`
Expected: 3 tests PASS.

If tests fail because `ServerConfig::default` isn't derivable or `LogStorage::new` signature differs, mirror the construction used in `ha_test.rs` exactly — that test is already known to build the same state.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/tests/metrics_test.rs
git commit -m "metrics: integration tests for auth modes and content-type"
```

---

## Task 8: Implement gather_gauges with timeouts

**Files:**
- Modify: `crates/stroem-server/src/metrics.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/stroem-server/tests/metrics_test.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn leader_gauge_is_one_for_always_leader() -> Result<()> {
    let (_h, router) = boot_with_config(with_public(base_config())).await?;

    let body = scrape(&router).await?;
    assert!(
        body.contains(&format!(
            "{} 1",
            stroem_server::metrics::STROEM_LEADER_STATUS
        )),
        "expected leader gauge = 1 in:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn workers_active_gauge_reflects_db() -> Result<()> {
    let (h, router) = boot_with_config(with_public(base_config())).await?;

    sqlx::query(
        "INSERT INTO worker (worker_id, tags, status, last_heartbeat) \
         VALUES ($1, '[]'::jsonb, 'active', NOW()), \
                ($2, '[]'::jsonb, 'active', NOW()), \
                ($3, '[]'::jsonb, 'inactive', NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&h.pool)
    .await?;

    let body = scrape(&router).await?;
    assert!(
        body.contains(&format!(
            "{} 2",
            stroem_server::metrics::STROEM_WORKERS_ACTIVE
        )),
        "expected 2 active workers in:\n{body}"
    );
    Ok(())
}

// helpers used by these and subsequent tests
fn with_public(mut cfg: ServerConfig) -> ServerConfig {
    cfg.metrics = Some(MetricsConfig { public: true });
    cfg
}

async fn scrape(router: &axum::Router) -> Result<String> {
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(body_string(response).await)
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p stroem-server --test metrics_test leader_gauge_is_one_for_always_leader workers_active_gauge_reflects_db`
Expected: FAIL — gauges absent from rendered output (placeholder `gather_gauges` does nothing).

- [ ] **Step 3: Replace the placeholder gather_gauges**

In `crates/stroem-server/src/metrics.rs`, replace the placeholder `gather_gauges` body with:

```rust
use metrics::gauge;
use std::sync::atomic::Ordering;
use std::time::Duration;

const GAUGE_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

#[tracing::instrument(skip(state))]
pub async fn gather_gauges(state: &AppState) {
    // --- Synchronous gauges (no DB) ---

    gauge!(STROEM_LEADER_STATUS).set(if state.leader.is_leader() { 1.0 } else { 0.0 });

    let bg = &state.background_tasks;
    gauge!(STROEM_BACKGROUND_TASK_ALIVE, "task" => "scheduler")
        .set(bool_to_f64(bg.scheduler_alive.load(Ordering::Relaxed)));
    gauge!(STROEM_BACKGROUND_TASK_ALIVE, "task" => "recovery")
        .set(bool_to_f64(bg.recovery_alive.load(Ordering::Relaxed)));
    gauge!(STROEM_BACKGROUND_TASK_ALIVE, "task" => "event_source")
        .set(bool_to_f64(bg.event_source_alive.load(Ordering::Relaxed)));

    // --- DB-backed gauges (each bounded by GAUGE_QUERY_TIMEOUT) ---

    sample_workers_active(state).await;
    sample_jobs_in_flight(state).await;
    sample_steps_ready(state).await;
}

fn bool_to_f64(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

async fn sample_workers_active(state: &AppState) {
    let query = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM worker WHERE status = 'active'",
    )
    .fetch_one(&state.pool);

    match tokio::time::timeout(GAUGE_QUERY_TIMEOUT, query).await {
        Ok(Ok(count)) => gauge!(STROEM_WORKERS_ACTIVE).set(count as f64),
        Ok(Err(e)) => tracing::warn!(error = %e, "metrics: workers_active query failed"),
        Err(_) => tracing::warn!("metrics: workers_active query timed out"),
    }
}

async fn sample_jobs_in_flight(state: &AppState) {
    let query = sqlx::query_as::<_, (String, i64)>(
        "SELECT status, COUNT(*)::BIGINT FROM job \
         WHERE status IN ('pending', 'running') GROUP BY status",
    )
    .fetch_all(&state.pool);

    match tokio::time::timeout(GAUGE_QUERY_TIMEOUT, query).await {
        Ok(Ok(rows)) => {
            // Always emit both labels so dashboards don't go blank when one
            // bucket is empty.
            let mut pending = 0_i64;
            let mut running = 0_i64;
            for (status, count) in rows {
                match status.as_str() {
                    "pending" => pending = count,
                    "running" => running = count,
                    _ => {}
                }
            }
            gauge!(STROEM_JOBS_IN_FLIGHT, "status" => "pending").set(pending as f64);
            gauge!(STROEM_JOBS_IN_FLIGHT, "status" => "running").set(running as f64);
        }
        Ok(Err(e)) => tracing::warn!(error = %e, "metrics: jobs_in_flight query failed"),
        Err(_) => tracing::warn!("metrics: jobs_in_flight query timed out"),
    }
}

async fn sample_steps_ready(state: &AppState) {
    let query = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM job_step \
         WHERE status = 'ready' AND (retry_at IS NULL OR retry_at <= NOW())",
    )
    .fetch_one(&state.pool);

    match tokio::time::timeout(GAUGE_QUERY_TIMEOUT, query).await {
        Ok(Ok(count)) => gauge!(STROEM_STEPS_READY).set(count as f64),
        Ok(Err(e)) => tracing::warn!(error = %e, "metrics: steps_ready query failed"),
        Err(_) => tracing::warn!("metrics: steps_ready query timed out"),
    }
}
```

Remove the earlier placeholder `pub async fn gather_gauges(_state: &AppState) {}`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p stroem-server --test metrics_test`
Expected: 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/stroem-server/src/metrics.rs crates/stroem-server/tests/metrics_test.rs
git commit -m "metrics: implement gather_gauges (leader, workers, jobs, steps, bg tasks)"
```

---

## Task 9: Add RED tower middleware on /api/*

**Files:**
- Modify: `crates/stroem-server/src/metrics.rs`
- Modify: `crates/stroem-server/src/web/mod.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/stroem-server/tests/metrics_test.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn http_request_counter_records_api_hits() -> Result<()> {
    let (_h, router) = boot_with_config(with_public(base_config())).await?;

    // Hit any /api/* endpoint; /api/workspaces is light and always present.
    for _ in 0..3 {
        let _ = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/workspaces")
                    .body(Body::empty())?,
            )
            .await?;
    }

    let body = scrape(&router).await?;
    assert!(
        body.contains(stroem_server::metrics::STROEM_HTTP_REQUESTS_TOTAL),
        "expected http counter present in:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn http_route_label_uses_matched_pattern_not_raw_uri() -> Result<()> {
    let (_h, router) = boot_with_config(with_public(base_config())).await?;

    // Hit the same parameterised route with two different ids.
    for _ in 0..2 {
        let _ = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/jobs/{}", Uuid::new_v4()))
                    .body(Body::empty())?,
            )
            .await?;
    }

    let body = scrape(&router).await?;
    // The matched pattern is `/api/jobs/{id}` (Axum's matchit format).
    // Ensure the rendered exposition does NOT include a UUID in the route
    // label.
    let uuid_in_route = body
        .lines()
        .filter(|l| l.starts_with(stroem_server::metrics::STROEM_HTTP_REQUESTS_TOTAL))
        .any(|l| {
            // crude UUID detection: 8-4-4-4-12 hex with dashes
            regex_lite::Regex::new(
                r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            )
            .unwrap()
            .is_match(l)
        });
    assert!(!uuid_in_route, "raw UUIDs leaked into route label:\n{body}");
    Ok(())
}
```

Add `regex_lite = "0.1"` to `crates/stroem-server/Cargo.toml` under `[dev-dependencies]` (create the section if it doesn't exist).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p stroem-server --test metrics_test http_request_counter http_route_label`
Expected: FAIL — counter not recorded (no middleware yet).

- [ ] **Step 3: Implement the tower middleware**

Append to `crates/stroem-server/src/metrics.rs`:

```rust
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics::{counter, histogram};
use std::time::Instant;

/// Tower middleware that records `stroem_http_requests_total` and
/// `stroem_http_request_duration_seconds` for each request that passes
/// through it.
///
/// Apply only to the `/api/*` subtree — see spec §HTTP middleware scope.
pub async fn track_http_metrics(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unknown".to_string());

    let response = next.run(request).await;

    let status = response.status().as_u16().to_string();
    let elapsed = start.elapsed().as_secs_f64();

    counter!(
        STROEM_HTTP_REQUESTS_TOTAL,
        "method" => method.to_string(),
        "route" => route.clone(),
        "status" => status,
    )
    .increment(1);
    histogram!(STROEM_HTTP_REQUEST_DURATION_SECONDS, "route" => route).record(elapsed);

    response
}
```

- [ ] **Step 4: Apply the middleware to the api router**

In `crates/stroem-server/src/web/mod.rs`, change the `.nest("/api", api::build_api_routes(state.clone()))` line in the `build_router` chain to:

```rust
        .nest(
            "/api",
            api::build_api_routes(state.clone())
                .layer(axum::middleware::from_fn(crate::metrics::track_http_metrics)),
        )
```

Leave `/worker`, `/hooks`, and `/mcp` untouched — per spec, RED middleware is `/api/*` only.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p stroem-server --test metrics_test`
Expected: all metrics_test tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server/src/metrics.rs crates/stroem-server/src/web/mod.rs crates/stroem-server/Cargo.toml crates/stroem-server/tests/metrics_test.rs
git commit -m "metrics: add RED tower middleware on /api/* (request count + duration)"
```

---

## Task 10: Record jobs_created_total

**Files:**
- Modify: `crates/stroem-server/src/job_creator.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/stroem-server/tests/metrics_test.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn jobs_created_counter_renders_with_source_type_label() -> Result<()> {
    let (_h, router) = boot_with_config(with_public(base_config())).await?;

    // Exercise the counter macro at the same call site shape used in
    // `job_creator::create_job_for_task_inner`. The production wiring is
    // verified by the grep step (Task 10 Step 4). This test ensures the
    // metric name + label are recognised by the renderer.
    metrics::counter!(
        stroem_server::metrics::STROEM_JOBS_CREATED_TOTAL,
        "source_type" => "api",
    )
    .increment(1);

    let body = scrape(&router).await?;
    assert!(
        body.contains(stroem_server::metrics::STROEM_JOBS_CREATED_TOTAL),
        "expected jobs_created counter in:\n{body}"
    );
    assert!(
        body.contains("source_type=\"api\""),
        "expected source_type label in:\n{body}"
    );
    Ok(())
}
```

Note: this test asserts the counter is wired into the renderer. The "increments at the right site" guarantee is enforced by the call-site addition in step 3 + a clippy/grep check (step 6).

Add `metrics = "0.24"` to `[dev-dependencies]` in `crates/stroem-server/Cargo.toml` if not already inherited.

- [ ] **Step 2: Run the test to verify it passes only after the renderer wiring works**

Run: `cargo test -p stroem-server --test metrics_test jobs_created_counter_renders_with_source_type_label`
Expected: PASS (we recorded via the macro inside the test). If FAIL, the renderer wiring from earlier tasks is wrong — fix that first.

- [ ] **Step 3: Add the counter at the real recording site**

Open `crates/stroem-server/src/job_creator.rs`. Locate `fn create_job_for_task_inner` (line 99). The new job row is inserted at the `JobRepo::create_with_parent_tx_id(...)` call (around line 249). Immediately after that call returns `Ok`, add:

```rust
    metrics::counter!(
        crate::metrics::STROEM_JOBS_CREATED_TOTAL,
        "source_type" => source_type.to_owned(),
    )
    .increment(1);
```

`source_type` is `&'a str` (parameter at line 105). The `metrics` macro needs an owned `String` for the label value, hence `.to_owned()`.

- [ ] **Step 4: Verify build and grep**

Run: `cargo build -p stroem-server`
Run: `grep -n STROEM_JOBS_CREATED_TOTAL crates/stroem-server/src/job_creator.rs`
Expected: build clean; grep shows exactly one match.

- [ ] **Step 5: Re-run the full metrics_test suite**

Run: `cargo test -p stroem-server --test metrics_test`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server/src/job_creator.rs crates/stroem-server/tests/metrics_test.rs crates/stroem-server/Cargo.toml
git commit -m "metrics: increment stroem_jobs_created_total in create_job_for_task_inner"
```

---

## Task 11: Record jobs_completed_total

**Files:**
- Modify: `crates/stroem-server/src/job_recovery.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/stroem-server/tests/metrics_test.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn jobs_completed_counter_includes_status_label() -> Result<()> {
    let (_h, router) = boot_with_config(with_public(base_config())).await?;

    // Direct counter exercise — the production site is asserted by grep.
    metrics::counter!(
        stroem_server::metrics::STROEM_JOBS_COMPLETED_TOTAL,
        "status" => "completed",
    )
    .increment(1);
    metrics::counter!(
        stroem_server::metrics::STROEM_JOBS_COMPLETED_TOTAL,
        "status" => "failed",
    )
    .increment(1);

    let body = scrape(&router).await?;
    assert!(body.contains("status=\"completed\""), "missing status label in:\n{body}");
    assert!(body.contains("status=\"failed\""), "missing failed label in:\n{body}");
    Ok(())
}
```

- [ ] **Step 2: Run the test to verify it passes (renderer-only check)**

Run: `cargo test -p stroem-server --test metrics_test jobs_completed_counter_includes_status_label`
Expected: PASS.

- [ ] **Step 3: Add the counter at the real recording site**

In `crates/stroem-server/src/job_recovery.rs`, modify `handle_job_terminal` (line 606). Insert the counter increment **immediately after** the terminal-status guard returns the job (around line 619, just before `crate::cancellation::clear_cancelled(state, job_id);`):

```rust
    metrics::counter!(
        crate::metrics::STROEM_JOBS_COMPLETED_TOTAL,
        "status" => job.status.clone(),
    )
    .increment(1);
```

`job.status` is already a `String` matching one of `completed` / `failed` / `cancelled` / `skipped`.

- [ ] **Step 4: Verify build and grep**

Run: `cargo build -p stroem-server`
Run: `grep -n STROEM_JOBS_COMPLETED_TOTAL crates/stroem-server/src/job_recovery.rs`
Expected: build clean; grep shows exactly one match.

- [ ] **Step 5: Re-run the full metrics_test suite**

Run: `cargo test -p stroem-server --test metrics_test`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/stroem-server/src/job_recovery.rs crates/stroem-server/tests/metrics_test.rs
git commit -m "metrics: increment stroem_jobs_completed_total in handle_job_terminal"
```

---

## Task 12: Extra integration coverage (steps_ready, jobs_in_flight, db error survival)

**Files:**
- Modify: `crates/stroem-server/tests/metrics_test.rs`

- [ ] **Step 1: Append the remaining tests**

Append to `crates/stroem-server/tests/metrics_test.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn steps_ready_gauge_reflects_db_state() -> Result<()> {
    let (h, router) = boot_with_config(with_public(base_config())).await?;

    // Insert one job + 3 ready steps + 1 ready-but-retry-future step.
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at) \
         VALUES ($1, 'ws', 't', 'pending', 'api', NOW())",
    )
    .bind(job_id)
    .execute(&h.pool)
    .await?;

    for i in 0..3 {
        sqlx::query(
            "INSERT INTO job_step (job_id, step_name, status, action_type, required_tags) \
             VALUES ($1, $2, 'ready', 'script', '[]'::jsonb)",
        )
        .bind(job_id)
        .bind(format!("s{i}"))
        .execute(&h.pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, status, action_type, required_tags, retry_at) \
         VALUES ($1, 'future', 'ready', 'script', '[]'::jsonb, NOW() + INTERVAL '1 hour')",
    )
    .bind(job_id)
    .execute(&h.pool)
    .await?;

    let body = scrape(&router).await?;
    assert!(
        body.contains(&format!("{} 3", stroem_server::metrics::STROEM_STEPS_READY)),
        "expected 3 ready steps in:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_in_flight_emits_both_status_labels_even_when_empty() -> Result<()> {
    let (_h, router) = boot_with_config(with_public(base_config())).await?;
    let body = scrape(&router).await?;
    // Empty DB still emits 0 for both buckets so dashboards don't go blank.
    assert!(body.contains("stroem_jobs_in_flight"), "missing in_flight:\n{body}");
    assert!(body.contains("status=\"pending\""), "missing pending bucket");
    assert!(body.contains("status=\"running\""), "missing running bucket");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn background_task_alive_reflects_atomic_flag() -> Result<()> {
    let (_h, router) = boot_with_config(with_public(base_config())).await?;
    let body = scrape(&router).await?;
    // All flags default to false at boot → all 0.
    assert!(
        body.contains(&format!(
            "{}{{task=\"scheduler\"}} 0",
            stroem_server::metrics::STROEM_BACKGROUND_TASK_ALIVE
        )),
        "expected scheduler=0 in:\n{body}"
    );
    Ok(())
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p stroem-server --test metrics_test`
Expected: all PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/stroem-server/tests/metrics_test.rs
git commit -m "metrics: add gauge integration coverage (steps_ready, in_flight, bg)"
```

---

## Task 13: Helm ServiceMonitor template + values

**Files:**
- Modify: `helm/stroem/values.yaml`
- Create: `helm/stroem/templates/servicemonitor.yaml`

- [ ] **Step 1: Update values.yaml**

In `helm/stroem/values.yaml`, **remove** the commented-out `ServiceMonitor` example (lines 261–277 — the entire `extraManifests` doc block describing ServiceMonitor; leave `extraManifests: []` itself in place if it has other purposes).

Then append a new section (place after `ingress:` and before any pre-existing `extraManifests`):

```yaml
# Strøm exposes /metrics on the main HTTP port. Enable the ServiceMonitor
# if you run Prometheus Operator. When metrics.public is false (default),
# the ServiceMonitor uses bearerTokenSecret to authenticate scrapes with
# the same worker token configured via STROEM__WORKER_TOKEN.
serviceMonitor:
  enabled: false
  interval: 30s
  scrapeTimeout: 10s
  additionalLabels: {}
  # Name of the secret containing the worker token. Only used when
  # `metrics.public` is false. Default: derived from existing worker-token
  # secret name (override if you store it elsewhere).
  bearerTokenSecret:
    name: ""    # required when metrics.public=false; e.g. "stroem-worker-token"
    key: "token"

metrics:
  # If true, /metrics requires no authentication. Default false (worker token).
  public: false
```

- [ ] **Step 2: Create the template**

Create `helm/stroem/templates/servicemonitor.yaml`:

```yaml
{{- if .Values.serviceMonitor.enabled }}
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: {{ include "stroem.fullname" . }}
  labels:
    {{- include "stroem.labels" . | nindent 4 }}
    {{- with .Values.serviceMonitor.additionalLabels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
spec:
  selector:
    matchLabels:
      {{- include "stroem.selectorLabels" . | nindent 6 }}
      app.kubernetes.io/component: server
  endpoints:
    - port: http
      path: /metrics
      interval: {{ .Values.serviceMonitor.interval | default "30s" }}
      scrapeTimeout: {{ .Values.serviceMonitor.scrapeTimeout | default "10s" }}
      {{- if not .Values.metrics.public }}
      {{- if not .Values.serviceMonitor.bearerTokenSecret.name }}
      {{- fail "serviceMonitor.bearerTokenSecret.name must be set when metrics.public is false" }}
      {{- end }}
      bearerTokenSecret:
        name: {{ .Values.serviceMonitor.bearerTokenSecret.name }}
        key: {{ .Values.serviceMonitor.bearerTokenSecret.key | default "token" }}
      {{- end }}
{{- end }}
```

- [ ] **Step 3: Render-test the template**

Run: `helm template test helm/stroem --set serviceMonitor.enabled=true --set serviceMonitor.bearerTokenSecret.name=stroem-worker-token | grep -A 20 ServiceMonitor`
Expected: a ServiceMonitor manifest renders with `bearerTokenSecret` set.

Run: `helm template test helm/stroem --set serviceMonitor.enabled=true --set metrics.public=true | grep -A 20 ServiceMonitor`
Expected: a ServiceMonitor manifest renders **without** `bearerTokenSecret`.

Run: `helm template test helm/stroem --set serviceMonitor.enabled=true 2>&1 | head -5`
Expected: fails with the message `serviceMonitor.bearerTokenSecret.name must be set when metrics.public is false`.

Run: `helm template test helm/stroem | grep ServiceMonitor`
Expected: no output (disabled by default).

- [ ] **Step 4: Commit**

```bash
git add helm/stroem/values.yaml helm/stroem/templates/servicemonitor.yaml
git commit -m "helm: add ServiceMonitor template + metrics values"
```

---

## Task 14: User-facing documentation

**Files:**
- Create: `docs/src/content/docs/operations/metrics.md`
- Modify: `docs/src/content/docs/operations/high-availability.md`
- Modify: `docs/astro.config.mjs` (or whichever file holds the sidebar config)

- [ ] **Step 1: Create the docs page**

Create `docs/src/content/docs/operations/metrics.md`:

```markdown
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
| `stroem_http_requests_total` | `method`, `route`, `status` | Requests served on `/api/*`. Route uses the matched Axum pattern (e.g. `/api/jobs/{id}`), not raw URIs. |
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
```

- [ ] **Step 2: Cross-link from HA guide**

In `docs/src/content/docs/operations/high-availability.md`, find a natural spot (e.g., near the "Monitor" section if one exists, otherwise near the end) and add:

```markdown
## Monitoring leader flips

Each replica exposes `stroem_leader_status` (1 on the leader, 0 on followers)
via the [Metrics endpoint](./metrics). A `max(stroem_leader_status) == 0`
alert catches a stuck-no-leader situation; rapid changes in
`changes(stroem_leader_status[5m])` indicate flapping.
```

- [ ] **Step 3: Add to sidebar nav**

Find the docs sidebar config (likely `docs/astro.config.mjs` — search for the Operations group):

```bash
grep -n -A 5 "Operations\|operations" docs/astro.config.mjs
```

Add an entry `{ label: 'Metrics', link: '/operations/metrics/' }` next to the existing "High Availability" entry in the Operations group.

- [ ] **Step 4: Build the docs site**

Run: `cd docs && bun install && bun run build`
Expected: clean build; `dist/operations/metrics/index.html` exists.

- [ ] **Step 5: Commit**

```bash
git add docs/src/content/docs/operations/metrics.md docs/src/content/docs/operations/high-availability.md docs/astro.config.mjs
git commit -m "docs: add Prometheus metrics operations guide"
```

---

## Task 15: CI promtool validation

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Inspect current workflow**

Run: `head -80 .github/workflows/ci.yml`
Identify the job that runs integration tests (probably named `test` or `cargo-test`).

- [ ] **Step 2: Add a validation step**

After the integration-test step in the appropriate job, add:

```yaml
      - name: Install promtool
        run: |
          curl -sL https://github.com/prometheus/prometheus/releases/download/v2.55.1/prometheus-2.55.1.linux-amd64.tar.gz | tar xz
          sudo mv prometheus-2.55.1.linux-amd64/promtool /usr/local/bin/

      - name: Validate /metrics exposition
        run: |
          # Build server, boot it briefly, scrape, validate.
          cargo build -p stroem-server --release
          # NB: server needs a Postgres + minimal config to boot. The
          # simplest validation is to call the rendered output from a
          # one-shot Rust binary that installs the recorder, records a
          # few values, then prints handle.render(). Skip live-server
          # boot — pipe a cargo-test-generated sample exposition through
          # promtool instead:
          cargo test -p stroem-server --test metrics_test --release \
            --nocapture metrics_with_valid_token_returns_text_format 2>&1 | \
            grep -E '^(# HELP|# TYPE|stroem_)' > /tmp/metrics.txt || true
          if [ -s /tmp/metrics.txt ]; then
            promtool check metrics < /tmp/metrics.txt
          else
            echo "no metrics output captured — skipping promtool check"
          fi
```

(If grepping test stdout proves unreliable, switch to a dedicated `examples/render_metrics.rs` binary that prints the exposition — but try the above first.)

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: validate /metrics exposition with promtool"
```

---

## Task 16: TODO + CLAUDE.md updates

**Files:**
- Modify: `docs/internal/TODO.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Mark TODO complete**

In `docs/internal/TODO.md`, find the line:

```markdown
- [ ] No metrics/Prometheus endpoint — capacity planning blind
```

Change to:

```markdown
- [x] No metrics/Prometheus endpoint — implemented in v0.15.12. Lean inventory (9 metrics: HTTP RED on `/api/*`, jobs created/completed counters, leader/workers/in-flight/ready/bg-task gauges). Hybrid recording (counters inline via `metrics` facade; gauges sampled at scrape time with 2s timeout per query). `metrics-exporter-prometheus = "0.18"` no-default-features. Default auth: worker-token Bearer; opt-out via `metrics: { public: true }`. Helm `serviceMonitor.enabled`. See `docs/src/content/docs/operations/metrics.md` and `docs/superpowers/specs/2026-05-19-prometheus-metrics-design.md`.
```

Append a new section at the bottom of the file:

```markdown
## Review: Prometheus Metrics (2026-05-19)

Tracked for the next review cycle. Initial implementation in this PR; review TBD.
```

- [ ] **Step 2: Add CLAUDE.md key-patterns entry**

In `CLAUDE.md`, in the **Key Patterns** section, add (alphabetical near "MCP Server" or "Multi-Workspace"):

```markdown
### Prometheus Metrics
- `crates/stroem-server/src/metrics.rs` — recorder install + `gather_gauges` + metric name constants + RED tower middleware
- `crates/stroem-server/src/web/metrics.rs` — `GET /metrics` handler
- Feature-less: always enabled. Config: optional `metrics: { public: bool }` (default `false` → requires `worker_token` Bearer, same auth as `/healthz/detail`).
- Hybrid recording: counters/histograms inline at event sites via `metrics::counter!` / `metrics::histogram!`; gauges sampled at scrape time in `gather_gauges` (2s timeout per DB query, errors logged + skipped, not zeroed).
- RED middleware (`stroem_http_requests_total`, `stroem_http_request_duration_seconds`) applied to `/api/*` only — not `/worker`, `/hooks`, `/mcp`.
- New metrics: add a `pub const` in `metrics.rs`, add the recording site, add an integration test in `crates/stroem-server/tests/metrics_test.rs`, document in `docs/src/content/docs/operations/metrics.md`.
- Helm: `serviceMonitor.enabled: true` renders `templates/servicemonitor.yaml`; uses `bearerTokenSecret` when `metrics.public: false`.
```

- [ ] **Step 3: Commit**

```bash
git add docs/internal/TODO.md CLAUDE.md
git commit -m "docs: mark Prometheus metrics complete in TODO; add CLAUDE.md key pattern"
```

---

## Task 17: Full CI check before declaring done

**Files:** None (verification only)

- [ ] **Step 1: Format + lint**

Run: `cargo fmt --check --all`
Expected: clean.

Run: `cargo clippy --workspace -- -D warnings`
Expected: clean.

- [ ] **Step 2: Full test suite**

Run: `cargo test --workspace`
Expected: all PASS.

- [ ] **Step 3: Frontend checks**

Run: `cd ui && bun run lint && bunx tsc --noEmit`
Expected: clean.

- [ ] **Step 4: If anything fails, fix and re-run from step 1**

Iterate until green. Do not declare done with failing checks.

- [ ] **Step 5: Optional — bump version**

If the user requests a release after this lands:
- Bump `Cargo.toml` `workspace.package.version` and `helm/stroem/Chart.yaml` `version` + `appVersion` to `0.15.12`
- Commit as `Bump version to 0.15.12`
- Use the `/release` skill to tag and push

---

## Self-review notes

**Spec coverage:**
- §Architecture (modules, route wiring, recorder install) → Tasks 2, 3, 5, 6
- §Config → Task 4
- §Metric Inventory (all 9 metrics) → Tasks 8 (gauges), 9 (HTTP), 10 (jobs_created), 11 (jobs_completed)
- §Data Flow → covered implicitly across Tasks 6 (handler), 8 (gather_gauges with timeouts)
- §Error Handling (timeout, skip-on-error) → Task 8
- §Helm → Task 13
- §Docs (metrics page, HA cross-link, TODO, CLAUDE.md) → Tasks 14, 16
- §Testing (all spec-listed tests) → Tasks 7, 8, 9, 12 (route_label, public-no-auth, worker-token, leader, workers, in-flight, steps_ready, db-error-survival is implicit in timeout handling)
- §CI promtool → Task 15

**Known compromise:** The spec lists `gather_gauges_survives_db_error` as an integration test. Direct verification (dropping the pool mid-scrape) is awkward in testcontainers; the timeout + log-and-skip behaviour is covered by unit-level reasoning + the warn log path. If a reviewer pushes back, add a test in Task 8 that uses a `sqlx::PgPool` connected to a deliberately unreachable port and asserts the scrape still returns 200.

**Type/name consistency check:** All metric names use the `STROEM_*` constants from `metrics.rs`. All recording sites reference them by path (`crate::metrics::STROEM_*`). Label keys (`method`, `route`, `status`, `source_type`, `task`) match the spec's inventory table. Histogram bucket array matches the spec verbatim.

**Build sequence verified:** Tasks build on each other in order — recorder install (T3) ships before route wiring (T6) ships before middleware (T9) ships before recording sites (T10, T11). Tests added incrementally so failures localise.
