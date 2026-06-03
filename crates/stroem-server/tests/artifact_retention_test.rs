//! Retention sweep deletes artifact blobs + rows before the job row.
//!
//! Mirrors the inline-harness pattern used by `artifact_api_test.rs` and
//! `artifact_upload_test.rs` — no shared `common::TestHarness` exists in this
//! crate yet, so the relevant setup is duplicated here for clarity.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::repos::job_artifact::JobArtifactRepo;
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
    state: AppState,
    blob: Arc<dyn BlobArchive>,
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

async fn build_test_app_with_retention(job_days: u64) -> Result<TestApp> {
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
        retention: RetentionConfig {
            worker_hours: None,
            job_days: Some(job_days),
            interval_secs: 3600,
        },
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: Some(ArtifactStorageConfig::default()),
        default_step_timeout: None,
        default_job_timeout: None,
    };

    let workspace = WorkspaceConfig::new();
    let mgr = WorkspaceManager::from_config("ws1", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);

    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(artifact_dir));

    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_artifact_blob(Some(archive.clone()));
    let router = build_router(state.clone(), CancellationToken::new());

    Ok(TestApp {
        router,
        pool,
        state,
        blob: archive,
        _pg,
        _tmp: tmp,
    })
}

/// Insert a terminal job with a back-dated `created_at` so the retention
/// sweep will pick it up.
async fn create_old_terminal_job(
    pool: &PgPool,
    workspace: &str,
    task_name: &str,
    age_days: i64,
) -> Result<Uuid> {
    let job_id = Uuid::new_v4();
    let created_at = chrono::Utc::now() - chrono::Duration::days(age_days);
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at) \
         VALUES ($1, $2, $3, 'completed', 'api', $4)",
    )
    .bind(job_id)
    .bind(workspace)
    .bind(task_name)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(job_id)
}

async fn upload_artifact(
    app: &TestApp,
    job_id: Uuid,
    step_name: &str,
    artifact_name: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/worker/jobs/{job_id}/steps/{step_name}/artifacts/{artifact_name}"
        ))
        .header("authorization", format!("Bearer {WORKER_TOKEN}"))
        .header("content-type", content_type)
        .body(Body::from(body.to_vec()))
        .unwrap();
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "upload {} failed",
        artifact_name
    );
    Ok(())
}

async fn job_exists(pool: &PgPool, job_id: Uuid) -> Result<bool> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM job WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await?;
    Ok(row.0 > 0)
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_sweep_deletes_artifacts_then_job() -> Result<()> {
    // 1 day retention; job created 2 days ago is eligible for sweep.
    let app = build_test_app_with_retention(1).await?;

    let job_id = create_old_terminal_job(&app.pool, "ws1", "task1", 2).await?;

    // Upload an artifact (job is "running" in the upload path's check —
    // but the upload endpoint only requires the job exists, which it does
    // after the INSERT above. Verify by listing rows + presence on disk.)
    upload_artifact(&app, job_id, "s", "a.txt", "text/plain", b"x").await?;

    // Sanity: artifact row + blob exist pre-sweep.
    let repo = JobArtifactRepo::new(app.pool.clone());
    let rows = repo.list_for_job(job_id).await?;
    assert_eq!(rows.len(), 1, "expected one artifact pre-sweep");
    let key = format!("artifacts/ws1/{job_id}/s/a.txt");
    assert!(
        app.blob.get(&key).await?.is_some(),
        "expected blob at {key} pre-sweep"
    );

    // Run the retention sweep directly.
    stroem_server::recovery::retention_cleanup(&app.state).await;

    // Job row is gone.
    assert!(
        !job_exists(&app.pool, job_id).await?,
        "expected job row deleted after sweep"
    );
    // Artifact row is gone.
    let rows_after = repo.list_for_job(job_id).await?;
    assert!(
        rows_after.is_empty(),
        "expected zero artifact rows post-sweep, got {}",
        rows_after.len()
    );
    // Blob is gone.
    assert!(
        app.blob.get(&key).await?.is_none(),
        "expected blob deleted post-sweep at {key}"
    );

    Ok(())
}
