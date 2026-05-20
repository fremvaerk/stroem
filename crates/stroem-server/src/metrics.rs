//! Prometheus metrics: recorder install + scrape-time gauge collection.
//!
//! See `docs/superpowers/specs/2026-05-19-prometheus-metrics-design.md`.

/// `counter` — HTTP requests handled on `/api/*`. Labels: method, route, status.
pub const STROEM_HTTP_REQUESTS_TOTAL: &str = "stroem_http_requests_total";

/// `histogram` — `/api/*` request duration in seconds. Label: route.
pub const STROEM_HTTP_REQUEST_DURATION_SECONDS: &str = "stroem_http_request_duration_seconds";

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

use anyhow::{Context, Result};
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use std::sync::atomic::Ordering;
use std::time::Duration;
use stroem_common::models::job::JobStatus;
use uuid::Uuid;

const GAUGE_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

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

use crate::state::AppState;

/// Sample pull-mode gauges (leader, workers, queue depth, background tasks)
/// at scrape time. Errors are logged and the affected gauge is skipped, not
/// fabricated as zero — Prometheus treats absent samples as stale.
#[tracing::instrument(skip(state))]
pub async fn gather_gauges(state: &AppState) {
    // --- Synchronous gauges (no DB) ---

    // Leader-pod identity is sensitive: omit in public mode to avoid
    // fingerprinting which replica holds the HA lock from an unauthenticated
    // scrape endpoint (mirrors the decision made for /healthz).
    let metrics_public = state.config.metrics.as_ref().is_some_and(|m| m.public);
    if !metrics_public {
        gauge!(STROEM_LEADER_STATUS).set(f64::from(state.leader.is_leader()));
    }

    let bg = &state.background_tasks;
    gauge!(STROEM_BACKGROUND_TASK_ALIVE, "task" => "scheduler")
        .set(f64::from(bg.scheduler_alive.load(Ordering::Relaxed)));
    gauge!(STROEM_BACKGROUND_TASK_ALIVE, "task" => "recovery")
        .set(f64::from(bg.recovery_alive.load(Ordering::Relaxed)));
    gauge!(STROEM_BACKGROUND_TASK_ALIVE, "task" => "event_source")
        .set(f64::from(bg.event_source_alive.load(Ordering::Relaxed)));

    // --- DB-backed gauges (each bounded by GAUGE_QUERY_TIMEOUT) ---

    tokio::join!(
        sample_workers_active(state),
        sample_jobs_in_flight(state),
        sample_steps_ready(state),
    );
}

async fn sample_workers_active(state: &AppState) {
    let query =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM worker WHERE status = 'active'")
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
                if status == JobStatus::Pending.as_ref() {
                    pending = count;
                } else if status == JobStatus::Running.as_ref() {
                    running = count;
                } else {
                    tracing::warn!(
                        status = status.as_str(),
                        "metrics: sample_jobs_in_flight got unexpected status — query/match drift?"
                    );
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

/// Tower middleware that records `stroem_http_requests_total` and
/// `stroem_http_request_duration_seconds` for each request that passes
/// through it.
///
/// Apply only to the `/api/*` subtree — see spec §HTTP middleware scope.
/// `/worker/*`, `/hooks/*`, and `/mcp/*` are deliberately excluded.
pub async fn track_http_metrics(request: Request, next: Next) -> Response {
    use std::time::Instant;

    let start = Instant::now();
    let method = request.method().clone();
    // This middleware is applied to the `/api` subtree via `.layer()` on
    // the inner router passed to `.nest("/api", ...)` (see `web/mod.rs`).
    // In Axum 0.8 the `MatchedPath` extension is set to the *full* matched
    // path (including the nest prefix), so we get `/api/jobs/{id}` — not
    // the stripped `/jobs/{id}`. No prefix manipulation is needed.
    //
    // NOTE: if this middleware is ever applied to additional subtrees,
    // verify that MatchedPath still returns the full external path.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build an uninstalled `PrometheusRecorder` and return its
    /// handle. Calling `handle.render()` reads only from this local
    /// recorder's registry. NOTE: the `metrics::counter!` / `histogram!`
    /// macros always route to the globally installed recorder — they will
    /// NOT appear in this handle's output. Use only for smoke-testing
    /// `render()`-on-empty-registry, not for verifying macro recording.
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
    fn local_handle_renders_without_panic() {
        // Smoke test: a freshly built (uninstalled) recorder produces a
        // handle whose render() returns successfully. The actual global-label
        // behaviour is exercised end-to-end by the integration tests in
        // `tests/metrics_test.rs` (added in Task 7), which scrape /metrics
        // from a fully booted server.
        let handle = local_handle();
        let rendered = handle.render();
        // Empty registry → empty string is expected and valid.
        assert!(
            rendered.is_empty(),
            "expected empty render for empty registry, got: {rendered}"
        );
    }
}
