//! End-to-end: worker uploads file → row in DB → blob in storage.
//!
//! Exercises `POST /worker/jobs/{job_id}/steps/{step_name}/artifacts/{name}`
//! through the real router, against a testcontainers Postgres and a
//! local-filesystem `BlobArchive` backend, with the artifact upload size
//! caps configurable per test.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::repos::job_artifact::{JobArtifactRecord, JobArtifactRepo};
use stroem_db::{create_pool, run_migrations};
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    ArtifactStorageConfig, DbConfig, LogStorageConfig, RetentionConfig, ServerConfig,
    WorkspaceSourceDef,
};
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

const WORKER_TOKEN: &str = "test-worker-token";

struct TestApp {
    router: Router,
    pool: PgPool,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _tmp: TempDir,
}

async fn spawn_pg() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

async fn build_test_app(artifact_cfg: ArtifactStorageConfig) -> Result<TestApp> {
    let (pool, _pg) = spawn_pg().await?;

    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let artifact_dir = tmp.path().join("artifact-archive");
    std::fs::create_dir_all(&artifact_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: "postgres://unused".to_string(),
        },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
        },
        workspaces: HashMap::from([(
            "ws1".to_string(),
            WorkspaceSourceDef::Folder {
                path: tmp.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: WORKER_TOKEN.to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: Some(artifact_cfg),
        default_step_timeout: None,
        default_job_timeout: None,
    };

    let workspace = WorkspaceConfig::new();
    let mgr = WorkspaceManager::from_config("ws1", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);

    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(artifact_dir));

    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_artifact_blob(Some(archive));
    let router = build_router(state, CancellationToken::new());

    Ok(TestApp {
        router,
        pool,
        _pg,
        _tmp: tmp,
    })
}

async fn create_test_job(pool: &PgPool, workspace: &str, task_name: &str) -> Result<Uuid> {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at) \
         VALUES ($1, $2, $3, 'running', 'api', NOW())",
    )
    .bind(job_id)
    .bind(workspace)
    .bind(task_name)
    .execute(pool)
    .await?;
    Ok(job_id)
}

async fn list_artifacts(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobArtifactRecord>> {
    let repo = JobArtifactRepo::new(pool.clone());
    repo.list_for_job(job_id).await
}

fn upload_request(
    job_id: Uuid,
    step_name: &str,
    artifact_name: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!(
            "/worker/jobs/{job_id}/steps/{step_name}/artifacts/{artifact_name}"
        ))
        .header("authorization", format!("Bearer {WORKER_TOKEN}"))
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_persists_row_and_blob() -> Result<()> {
    let app = build_test_app(ArtifactStorageConfig::default()).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    let body = b"<h1>hi</h1>".to_vec();
    let request = upload_request(job_id, "build", "report.html", "text/html", body.clone());
    let resp = app.router.clone().oneshot(request).await?;
    let status = resp.status();
    let resp_body = resp.into_body().collect().await?.to_bytes();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "response body: {}",
        String::from_utf8_lossy(&resp_body)
    );

    let rows = list_artifacts(&app.pool, job_id).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "report.html");
    assert_eq!(rows[0].content_type, "text/html");
    assert_eq!(rows[0].size_bytes, body.len() as i64);

    // Verify blob landed on disk under the configured prefix.
    let blob_path = app
        ._tmp
        .path()
        .join("artifact-archive")
        .join(&rows[0].storage_key);
    assert!(
        blob_path.exists(),
        "expected blob on disk at {}",
        blob_path.display()
    );
    let bytes = std::fs::read(&blob_path)?;
    assert_eq!(bytes, body);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_rejects_oversized_file() -> Result<()> {
    let cfg = ArtifactStorageConfig {
        max_file_bytes: 8,
        ..ArtifactStorageConfig::default()
    };
    let app = build_test_app(cfg).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    let body = vec![0u8; 100];
    let request = upload_request(job_id, "build", "big.bin", "application/octet-stream", body);
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_rejects_when_per_job_cap_would_exceed() -> Result<()> {
    let cfg = ArtifactStorageConfig {
        max_job_bytes: 20,
        ..ArtifactStorageConfig::default()
    };
    let app = build_test_app(cfg).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    let request = upload_request(
        job_id,
        "s",
        "a.bin",
        "application/octet-stream",
        vec![0u8; 15],
    );
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let request = upload_request(
        job_id,
        "s",
        "b.bin",
        "application/octet-stream",
        vec![0u8; 10],
    );
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_replaces_existing_artifact_by_name() -> Result<()> {
    let app = build_test_app(ArtifactStorageConfig::default()).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    let request = upload_request(job_id, "s1", "x.txt", "text/plain", b"first".to_vec());
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let request = upload_request(job_id, "s2", "x.txt", "text/plain", b"second".to_vec());
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let rows = list_artifacts(&app.pool, job_id).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].step_name, "s2");
    assert_eq!(rows[0].size_bytes, 6);

    Ok(())
}
