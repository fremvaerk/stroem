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
#[tracing::instrument]
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
    fn local_handle_renders_without_panic() {
        // Smoke test: a freshly built (uninstalled) recorder produces a
        // handle whose render() returns successfully. The actual global-label
        // behaviour is exercised end-to-end by the integration tests in
        // `tests/metrics_test.rs` (added in Task 7), which scrape /metrics
        // from a fully booted server.
        let handle = local_handle();
        let rendered = handle.render();
        // Empty registry → empty string is expected and valid.
        assert!(rendered.is_empty(), "expected empty render for empty registry, got: {rendered}");
        // Verify the `metrics` macro is importable (compile-time check).
        let _ = counter!("dummy");
    }
}
