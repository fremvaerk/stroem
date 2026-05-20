//! Integration tests for the `/metrics` endpoint.

use anyhow::Result;
use axum::body::Body;
use axum::Extension;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serial_test::serial;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::OnceLock;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{create_pool, run_migrations};
use stroem_server::config::{
    DbConfig, LogStorageConfig, MetricsConfig, RecoveryConfig, RetentionConfig, ServerConfig,
};
use stroem_server::events::EventBus;
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

const WORKER_TOKEN: &str = "test-token-must-be-long-enough-32";

struct Harness {
    _container: testcontainers::ContainerAsync<Postgres>,
    pool: PgPool,
    url: String,
    _temp: TempDir,
}

async fn boot() -> Result<Harness> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{port}/postgres");
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    let temp = TempDir::new()?;
    Ok(Harness {
        _container: container,
        pool,
        url,
        _temp: temp,
    })
}

fn empty_config(url: &str, log_dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: url.to_string(),
        },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: WORKER_TOKEN.to_string(),
        auth: None,
        recovery: RecoveryConfig {
            heartbeat_timeout_secs: 120,
            sweep_interval_secs: 60,
            unmatched_step_timeout_secs: 30,
        },
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
    }
}

/// The `metrics` facade has a single global recorder per process. Install
/// exactly once for the whole test binary; every test reuses the same handle.
///
/// Side effect: counters accumulate across tests in the same process. Tests
/// must assert *presence* (or per-test diffs) rather than absolute values.
/// Gauges set a fresh value each scrape via `gather_gauges`, so they are
/// stable across tests as long as each test seeds known DB state.
fn global_test_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            stroem_server::metrics::install_recorder(Uuid::new_v4()).expect("install recorder once")
        })
        .clone()
}

async fn build_router_with(h: &Harness, config: ServerConfig) -> axum::Router {
    let mgr = WorkspaceManager::from_config("default", WorkspaceConfig::new());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(
        h.pool.clone(),
        mgr,
        config,
        log_storage,
        HashMap::new(),
        None,
    )
    .with_event_bus(EventBus::noop());

    let cancel = CancellationToken::new();
    let handle = global_test_handle();
    build_router(state, cancel).layer(Extension(handle))
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_public_endpoint_no_auth() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    let response = router
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;

    let (parts, body) = response.into_parts();
    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(
        parts
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; version=0.0.4")
    );
    let bytes = body.collect().await?.to_bytes();
    let _body = String::from_utf8_lossy(&bytes).to_string();
    // Empty registry renders empty body — that's fine. We just verified
    // status + content-type for the public auth-less path.
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_default_requires_worker_token() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let config = empty_config(&h.url, &log_dir);
    let router = build_router_with(&h, config).await;

    let response = router
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_with_valid_token_returns_text_format() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let config = empty_config(&h.url, &log_dir);
    let router = build_router_with(&h, config).await;

    let response = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(
                    http::header::AUTHORIZATION,
                    format!("Bearer {WORKER_TOKEN}"),
                )
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn leader_gauge_is_one_for_always_leader() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    let body = scrape(&router).await?;
    // Prometheus output includes global labels: `stroem_leader_status{replica_id="..."} 1`
    // so we check that the metric name appears followed by `} 1` on the same line.
    assert!(
        body.lines().any(|line| {
            line.starts_with(stroem_server::metrics::STROEM_LEADER_STATUS) && line.ends_with("} 1")
        }),
        "expected leader gauge = 1 in:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn workers_active_gauge_reflects_db() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    sqlx::query(
        "INSERT INTO worker (worker_id, name, tags, status, last_heartbeat) \
         VALUES ($1, 'w1', '[]'::jsonb, 'active', NOW()), \
                ($2, 'w2', '[]'::jsonb, 'active', NOW()), \
                ($3, 'w3', '[]'::jsonb, 'inactive', NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&h.pool)
    .await?;

    let body = scrape(&router).await?;
    // Prometheus output includes global labels: `stroem_workers_active{replica_id="..."} 2`
    // so we check that the metric name appears followed by `} 2` on the same line.
    assert!(
        body.lines().any(|line| {
            line.starts_with(stroem_server::metrics::STROEM_WORKERS_ACTIVE) && line.ends_with("} 2")
        }),
        "expected 2 active workers in:\n{body}"
    );
    Ok(())
}

// Helper shared by T8 onward.
async fn scrape(router: &axum::Router) -> Result<String> {
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn http_request_counter_records_api_hits() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    // Hit any /api/* endpoint a few times.
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
async fn jobs_created_counter_renders_with_source_type_label() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    // Exercise the counter macro at the same call shape used in
    // `job_creator::create_job_for_task_inner`. The production wiring is
    // also verified by the grep check below — this test ensures the metric
    // name + label render correctly.
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
        body.contains(r#"source_type="api""#),
        "expected source_type=\"api\" label in:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn http_route_label_uses_matched_pattern_not_raw_uri() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    // Hit the same parameterised route with two different UUIDs.
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
    // Match `route="<value>"` and check that the value itself does not
    // contain a raw UUID. We deliberately skip `replica_id` (which is a
    // UUID by design) by extracting only the route label value.
    let route_uuid_re = regex_lite::Regex::new(
        r#"route="[^"]*[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}[^"]*""#,
    )
    .unwrap();
    let uuid_in_route = body
        .lines()
        .filter(|l| l.starts_with(stroem_server::metrics::STROEM_HTTP_REQUESTS_TOTAL))
        .any(|l| route_uuid_re.is_match(l));
    assert!(!uuid_in_route, "raw UUIDs leaked into route label:\n{body}");

    // Verify the /api prefix is preserved in the route label so Grafana
    // dashboards filtering on `/api/jobs/*` get matches.
    assert!(
        body.lines()
            .filter(|l| l.starts_with(stroem_server::metrics::STROEM_HTTP_REQUESTS_TOTAL))
            .any(|l| l.contains(r#"route="/api/jobs/"#)),
        "expected /api/jobs/* prefix in route label, got:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_completed_counter_includes_status_label() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    // Exercise the counter macro shape used in
    // `job_recovery::handle_job_terminal`. The production site is verified
    // by the grep check below.
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
    assert!(
        body.contains(r#"status="completed""#),
        "missing status=\"completed\" label in:\n{body}"
    );
    assert!(
        body.contains(r#"status="failed""#),
        "missing status=\"failed\" label in:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn steps_ready_gauge_reflects_db_state() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    // Insert one job + 3 ready steps + 1 ready-but-retry-future step (which
    // must NOT be counted because its retry_at is in the future).
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
            "INSERT INTO job_step (job_id, step_name, action_name, status, action_type, required_tags) \
             VALUES ($1, $2, 'noop', 'ready', 'script', '[]'::jsonb)",
        )
        .bind(job_id)
        .bind(format!("s{i}"))
        .execute(&h.pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, status, action_type, required_tags, retry_at) \
         VALUES ($1, 'future', 'noop', 'ready', 'script', '[]'::jsonb, NOW() + INTERVAL '1 hour')",
    )
    .bind(job_id)
    .execute(&h.pool)
    .await?;

    let body = scrape(&router).await?;
    let expected = format!("{} 3", stroem_server::metrics::STROEM_STEPS_READY);
    let has_three = body
        .lines()
        .any(|l| l.starts_with(stroem_server::metrics::STROEM_STEPS_READY) && l.ends_with("} 3"));
    assert!(
        has_three,
        "expected 3 ready steps (line ending '}} 3' for {expected}) in:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn jobs_in_flight_emits_both_status_labels_even_when_empty() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    let body = scrape(&router).await?;
    // Empty DB still emits 0 for both buckets so dashboards don't go blank.
    let pending_line = body.lines().find(|l| {
        l.starts_with(stroem_server::metrics::STROEM_JOBS_IN_FLIGHT)
            && l.contains(r#"status="pending""#)
    });
    let running_line = body.lines().find(|l| {
        l.starts_with(stroem_server::metrics::STROEM_JOBS_IN_FLIGHT)
            && l.contains(r#"status="running""#)
    });
    assert!(
        pending_line.is_some(),
        "missing stroem_jobs_in_flight pending bucket in:\n{body}"
    );
    assert!(
        running_line.is_some(),
        "missing stroem_jobs_in_flight running bucket in:\n{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn background_task_alive_reflects_atomic_flag() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    let body = scrape(&router).await?;
    // All flags default to false at boot since no background loops are
    // spawned by the test harness → all bg-task gauges should be 0.
    let has_scheduler_zero = body.lines().any(|l| {
        l.starts_with(stroem_server::metrics::STROEM_BACKGROUND_TASK_ALIVE)
            && l.contains(r#"task="scheduler""#)
            && l.ends_with("} 0")
    });
    assert!(has_scheduler_zero, "expected scheduler=0 in:\n{body}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_unauthorized_response_is_text_plain() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let config = empty_config(&h.url, &log_dir);
    let router = build_router_with(&h, config).await;

    let response = router
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let ct = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.starts_with("text/plain"),
        "expected text/plain on unauth response, got: {ct}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_success_response_has_cache_control_no_store() -> Result<()> {
    let h = boot().await?;
    let log_dir = h._temp.path().to_path_buf();
    let mut config = empty_config(&h.url, &log_dir);
    config.metrics = Some(MetricsConfig { public: true });
    let router = build_router_with(&h, config).await;

    let response = router
        .oneshot(Request::builder().uri("/metrics").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let cache = response
        .headers()
        .get(http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());
    assert_eq!(cache.as_deref(), Some("no-store"));
    Ok(())
}
