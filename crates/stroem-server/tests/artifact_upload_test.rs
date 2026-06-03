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
    create_test_job_with_status(pool, workspace, task_name, "running").await
}

async fn create_test_job_with_status(
    pool: &PgPool,
    workspace: &str,
    task_name: &str,
    status: &str,
) -> Result<Uuid> {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at) \
         VALUES ($1, $2, $3, $4, 'api', NOW())",
    )
    .bind(job_id)
    .bind(workspace)
    .bind(task_name)
    .bind(status)
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

/// Regression for F2: `DefaultBodyLimit` was hardcoded at 100 MiB regardless
/// of `artifact_storage.max_file_bytes`, so operators raising the cap couldn't
/// upload large files (axum 413'd before the handler) and operators lowering
/// the cap still let axum buffer 100 MiB per concurrent request before the
/// handler rejected. With the fix, the per-route `DefaultBodyLimit` is wired
/// from config so bodies over the configured limit are rejected at the
/// axum-layer level — before the handler runs at all.
///
/// We prove "before the handler runs" by asserting the response body is NOT
/// the handler's JSON shape (`{"error": "...exceeds per-file limit of..."}`).
/// A pre-fix run would have buffered the 100 KiB body, reached the handler's
/// own per-file check (1024), and returned that JSON.
#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_body_limit_wired_from_config_returns_413_before_handler() -> Result<()> {
    let cfg = ArtifactStorageConfig {
        max_file_bytes: 1024,
        ..ArtifactStorageConfig::default()
    };
    let app = build_test_app(cfg).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    // 100 KiB — comfortably over the 1024-byte cap.
    let body = vec![0u8; 100 * 1024];
    let request = upload_request(
        job_id,
        "build",
        "huge.bin",
        "application/octet-stream",
        body,
    );
    let resp = app.router.clone().oneshot(request).await?;
    let status = resp.status();
    let body_bytes = resp.into_body().collect().await?.to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes);

    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "expected 413; got {status} with body: {body_str}"
    );

    // Layer-level rejection should NOT carry the handler's JSON envelope.
    // The handler builds its 413 via `AppError::PayloadTooLarge` which serialises
    // to `{"error":"...exceeds per-file limit of..."}`. axum's `RequestBodyLimit`
    // tower layer produces a plain "length limit exceeded" text body instead.
    assert!(
        !body_str.contains("exceeds per-file limit of"),
        "expected layer-level rejection (no handler message) but got handler JSON: {body_str}"
    );
    assert!(
        serde_json::from_slice::<serde_json::Value>(&body_bytes).is_err()
            || !body_str.contains("\"error\""),
        "expected non-JSON layer body, got: {body_str}"
    );

    // No row should land — the handler never executed.
    let rows = list_artifacts(&app.pool, job_id).await?;
    assert!(
        rows.is_empty(),
        "handler must not have executed; found rows: {rows:?}"
    );

    Ok(())
}

/// Sister check: with the cap raised above the body size, the same upload
/// succeeds — proving the limit really is being read from config, not just
/// rejecting everything at the new lower bound.
#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_body_limit_wired_from_config_accepts_when_under_cap() -> Result<()> {
    let cfg = ArtifactStorageConfig {
        max_file_bytes: 200 * 1024,
        ..ArtifactStorageConfig::default()
    };
    let app = build_test_app(cfg).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    // 100 KiB — under the 200 KiB cap.
    let body = vec![0u8; 100 * 1024];
    let request = upload_request(
        job_id,
        "build",
        "okay.bin",
        "application/octet-stream",
        body,
    );
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

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

#[tokio::test(flavor = "multi_thread")]
async fn delete_step_artifacts_clears_rows_and_blobs() -> Result<()> {
    let app = build_test_app(ArtifactStorageConfig::default()).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    // Upload two artifacts under step `build` and one under step `test` so we
    // can prove the delete is scoped to a single step.
    let request = upload_request(job_id, "build", "a.txt", "text/plain", b"x".to_vec());
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let request = upload_request(job_id, "build", "b.txt", "text/plain", b"y".to_vec());
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let request = upload_request(job_id, "test", "c.txt", "text/plain", b"z".to_vec());
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Snapshot the on-disk blob paths so we can verify they get cleaned up.
    let rows = list_artifacts(&app.pool, job_id).await?;
    assert_eq!(rows.len(), 3);
    let archive_root = app._tmp.path().join("artifact-archive");
    let build_blobs: Vec<_> = rows
        .iter()
        .filter(|r| r.step_name == "build")
        .map(|r| archive_root.join(&r.storage_key))
        .collect();
    for p in &build_blobs {
        assert!(p.exists(), "expected blob on disk at {}", p.display());
    }

    let delete_request = Request::builder()
        .method("DELETE")
        .uri(format!("/worker/jobs/{job_id}/steps/build/artifacts"))
        .header("authorization", format!("Bearer {WORKER_TOKEN}"))
        .body(Body::empty())?;
    let resp = app.router.clone().oneshot(delete_request).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Rows for `build` are gone; the `test` row survives.
    let remaining = list_artifacts(&app.pool, job_id).await?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].step_name, "test");

    // The blob subtree for `build` is gone.
    for p in &build_blobs {
        assert!(
            !p.exists(),
            "expected blob removed from disk at {}",
            p.display()
        );
    }

    // Idempotent: a second delete returns 204 even though there's nothing left.
    let delete_request = Request::builder()
        .method("DELETE")
        .uri(format!("/worker/jobs/{job_id}/steps/build/artifacts"))
        .header("authorization", format!("Bearer {WORKER_TOKEN}"))
        .body(Body::empty())?;
    let resp = app.router.clone().oneshot(delete_request).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    Ok(())
}

/// Regression for the per-job cap TOCTOU race. Two concurrent uploads to the
/// same `job_id` with distinct names whose combined size just exceeds the
/// per-job cap. Without the row-level lock around the cap math, both reads
/// see the same `existing=0` and both insert, leaving the DB over-cap. With
/// the lock, one upload sees the other's insert and is rejected with 413.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_uploads_respect_per_job_cap() -> Result<()> {
    let cfg = ArtifactStorageConfig {
        max_job_bytes: 18, // tight: each upload is 10 bytes, two = 20 > 18
        ..ArtifactStorageConfig::default()
    };
    let app = build_test_app(cfg).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    let router1 = app.router.clone();
    let router2 = app.router.clone();

    let req_a = upload_request(
        job_id,
        "s",
        "a.bin",
        "application/octet-stream",
        vec![0u8; 10],
    );
    let req_b = upload_request(
        job_id,
        "s",
        "b.bin",
        "application/octet-stream",
        vec![0u8; 10],
    );

    // Fire both uploads in parallel. The row-level lock should serialise
    // them, so exactly one sees the other's 10 bytes already counted and is
    // rejected.
    let (res_a, res_b) = tokio::join!(router1.oneshot(req_a), router2.oneshot(req_b));
    let status_a = res_a?.status();
    let status_b = res_b?.status();

    let mut statuses = [status_a, status_b];
    statuses.sort_by_key(|s| s.as_u16());
    assert_eq!(
        statuses,
        [StatusCode::CREATED, StatusCode::PAYLOAD_TOO_LARGE],
        "expected exactly one 201 + one 413, got {status_a:?} and {status_b:?}"
    );

    // Critically: the DB must be left in a consistent state — only the
    // successful upload's row exists, and the total is under cap.
    let rows = list_artifacts(&app.pool, job_id).await?;
    assert_eq!(rows.len(), 1, "expected one row, got {}", rows.len());
    let total: i64 = rows.iter().map(|r| r.size_bytes).sum();
    assert_eq!(total, 10, "total size must reflect only the winning upload");
    assert!(
        (total as u64) <= 18,
        "DB left over per-job cap: {total} > 18"
    );

    Ok(())
}

/// Upload to a terminal job must be refused with 409 Conflict so workers
/// can't keep writing artifacts after the orchestrator has already finalised
/// the job (and started log archival / retention math).
#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_rejects_completed_job_with_409() -> Result<()> {
    let app = build_test_app(ArtifactStorageConfig::default()).await?;
    let job_id = create_test_job_with_status(&app.pool, "ws1", "task1", "completed").await?;

    let request = upload_request(
        job_id,
        "build",
        "late.txt",
        "text/plain",
        b"too late".to_vec(),
    );
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // No row should have landed.
    let rows = list_artifacts(&app.pool, job_id).await?;
    assert!(
        rows.is_empty(),
        "no row should be inserted for terminal job"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_upload_rejects_failed_and_cancelled_jobs_with_409() -> Result<()> {
    let app = build_test_app(ArtifactStorageConfig::default()).await?;

    for status in ["failed", "cancelled"] {
        let job_id = create_test_job_with_status(&app.pool, "ws1", "task1", status).await?;
        let request = upload_request(job_id, "s", "x.txt", "text/plain", b"x".to_vec());
        let resp = app.router.clone().oneshot(request).await?;
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "expected 409 for status={status}, got {:?}",
            resp.status()
        );
    }

    Ok(())
}

/// Delete on a terminal job is also refused: once the job has finalised, its
/// artifact set is frozen for audit and retention is the only thing allowed
/// to remove rows.
#[tokio::test(flavor = "multi_thread")]
async fn worker_delete_rejects_completed_job_with_409() -> Result<()> {
    let app = build_test_app(ArtifactStorageConfig::default()).await?;
    let job_id = create_test_job(&app.pool, "ws1", "task1").await?;

    // Upload while the job is still running so there's something to delete.
    let request = upload_request(job_id, "build", "a.txt", "text/plain", b"hello".to_vec());
    let resp = app.router.clone().oneshot(request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Flip to a terminal state and try to delete.
    sqlx::query("UPDATE job SET status = 'completed' WHERE job_id = $1")
        .bind(job_id)
        .execute(&app.pool)
        .await?;

    let delete_request = Request::builder()
        .method("DELETE")
        .uri(format!("/worker/jobs/{job_id}/steps/build/artifacts"))
        .header("authorization", format!("Bearer {WORKER_TOKEN}"))
        .body(Body::empty())?;
    let resp = app.router.clone().oneshot(delete_request).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Row must still be there — delete was refused, not silently dropped.
    let rows = list_artifacts(&app.pool, job_id).await?;
    assert_eq!(rows.len(), 1, "row must survive a refused delete");

    Ok(())
}
