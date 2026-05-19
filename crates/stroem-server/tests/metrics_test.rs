//! Integration tests for the `/metrics` endpoint.

use anyhow::Result;
use axum::body::Body;
use axum::Extension;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
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
            ..Default::default()
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
            stroem_server::metrics::install_recorder(Uuid::new_v4())
                .expect("install recorder once")
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
