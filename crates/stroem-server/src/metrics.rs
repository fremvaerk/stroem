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
