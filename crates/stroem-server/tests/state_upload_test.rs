//! Integration tests for the state upload API.
//!
//! These use testcontainers Postgres + a local filesystem state backend
//! and exercise the full request -> synthetic job -> snapshot row ->
//! archive blob flow.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{create_pool, run_migrations};
use stroem_server::config::{
    DbConfig, LogStorageConfig, RetentionConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::state_storage::{LocalStateArchive, StateArchive, StateStorage};
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

async fn spawn_pg() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

/// Build a gzip tarball from (path, bytes) pairs for test fixtures.
fn make_tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        for (path, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            builder.append_data(&mut header, path, &bytes[..]).unwrap();
        }
        builder.finish().unwrap();
    }
    encoder.finish().unwrap()
}

/// Minimal TaskDef suitable for a workspace with just one task.
fn minimal_task() -> TaskDef {
    TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow: HashMap::new(),
        timeout: None,
        retry: None,
        on_success: vec![],
        on_error: vec![],
        on_suspended: vec![],
        on_cancel: vec![],
    }
}

struct TestApp {
    router: Router,
    pool: PgPool,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _tmp: TempDir,
}

/// Build a Router wired to a real testcontainers Postgres, with a workspace
/// containing the named tasks and a local-filesystem state backend.
async fn build_test_app(workspace_name: &str, task_names: &[&str]) -> Result<TestApp> {
    let (pool, _pg) = spawn_pg().await?;

    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let state_dir = tmp.path().join("state-archive");
    std::fs::create_dir_all(&state_dir)?;

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
            workspace_name.to_string(),
            WorkspaceSourceDef::Folder {
                path: tmp.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token".to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        agents: None,
        state_storage: None,
    };

    let mut workspace = WorkspaceConfig::new();
    for name in task_names {
        workspace.tasks.insert((*name).to_string(), minimal_task());
    }

    let mgr = WorkspaceManager::from_config(workspace_name, workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);

    let archive: Arc<dyn StateArchive> = Arc::new(LocalStateArchive::new(&state_dir));
    let storage = StateStorage::new(archive, "state/".to_string(), 5, None);

    let state = AppState::new(
        pool.clone(),
        mgr,
        config,
        log_storage,
        HashMap::new(),
        Some(storage),
    );
    let router = build_router(state, CancellationToken::new());

    Ok(TestApp {
        router,
        pool,
        _pg,
        _tmp: tmp,
    })
}

#[tokio::test]
async fn task_state_upload_round_trip() -> Result<()> {
    let app = build_test_app("production", &["renew-ssl"]).await?;
    let tarball = make_tarball(&[("cert.pem", b"PEMBYTES"), ("privkey.pem", b"KEYBYTES")]);

    let request = Request::builder()
        .method("POST")
        .uri(
            "/api/workspaces/production/tasks/renew-ssl/state?domain=example.com&expiry=2026-07-21",
        )
        .header("content-type", "application/gzip")
        .body(Body::from(tarball.clone()))?;

    let response = app.router.clone().oneshot(request).await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "response body: {}",
        String::from_utf8_lossy(&body)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(parsed["snapshot_id"].is_string());
    assert!(parsed["job_id"].is_string());

    let latest = stroem_db::TaskStateRepo::get_latest(&app.pool, "production", "renew-ssl")
        .await?
        .expect("latest snapshot should exist after upload");
    assert_eq!(latest.workspace, "production");
    assert_eq!(latest.task_name, "renew-ssl");
    // state.json was injected via query params, so has_json should be true
    assert!(latest.has_json);

    let row: (String, String) =
        sqlx::query_as("SELECT source_type, status FROM job WHERE job_id = $1")
            .bind(latest.job_id.unwrap())
            .fetch_one(&app.pool)
            .await?;
    assert_eq!(row.0, "upload");
    assert_eq!(row.1, "completed");

    Ok(())
}

#[tokio::test]
async fn global_state_upload_round_trip() -> Result<()> {
    let app = build_test_app("production", &[]).await?;
    let tarball = make_tarball(&[("config.json", b"{\"k\":\"v\"}")]);

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/state")
        .header("content-type", "application/gzip")
        .body(Body::from(tarball))?;

    let response = app.router.clone().oneshot(request).await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "response body: {}",
        String::from_utf8_lossy(&body)
    );

    let parsed: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(parsed["snapshot_id"].is_string());
    assert!(parsed["job_id"].is_string());

    let latest = stroem_db::WorkspaceStateRepo::get_latest(&app.pool, "production")
        .await?
        .expect("latest global snapshot should exist");

    let row: (String, String) =
        sqlx::query_as("SELECT source_type, task_name FROM job WHERE job_id = $1")
            .bind(latest.job_id.unwrap())
            .fetch_one(&app.pool)
            .await?;
    assert_eq!(row.0, "upload");
    assert_eq!(row.1, "_global_state");

    Ok(())
}

#[tokio::test]
async fn synthetic_upload_job_is_inserted() -> Result<()> {
    let (pool, _pg) = spawn_pg().await?;

    let mut tx = pool.begin().await?;
    let job_id = stroem_server::web::api::state_upload::insert_synthetic_upload_job(
        &mut tx,
        "production",
        "renew-ssl",
        Some("user:ala@allunite.com"),
        serde_json::json!({"upload": {"size_bytes": 123, "mode": "replace"}}),
        serde_json::json!({"snapshot_id": Uuid::new_v4()}),
        Some("rev-abc"),
    )
    .await?;
    tx.commit().await?;

    let row: (String, String, String) =
        sqlx::query_as("SELECT status, source_type, task_name FROM job WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await?;

    assert_eq!(row.0, "completed");
    assert_eq!(row.1, "upload");
    assert_eq!(row.2, "renew-ssl");

    Ok(())
}

#[tokio::test]
async fn upload_rejects_tarball_with_root_state_json() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;
    let tarball = make_tarball(&[("state.json", b"{}")]);

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state")
        .header("content-type", "application/gzip")
        .body(Body::from(tarball))?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn upload_rejects_invalid_mode() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state?mode=wat")
        .header("content-type", "application/gzip")
        .body(Body::empty())?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn upload_empty_body_is_accepted() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state?k=v")
        .body(Body::empty())?;

    let response = app.router.clone().oneshot(request).await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "response body: {}",
        String::from_utf8_lossy(&body)
    );

    let latest = stroem_db::TaskStateRepo::get_latest(&app.pool, "production", "t")
        .await?
        .unwrap();
    assert!(
        latest.has_json,
        "state.json synthesised from query params should be present"
    );
    Ok(())
}

#[tokio::test]
async fn upload_merge_mode_preserves_prior_files() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    // First upload (replace-mode by default): creates snapshot with a.txt and b.txt
    let first = make_tarball(&[("a.txt", b"A"), ("b.txt", b"B")]);
    let req1 = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state?x=1")
        .header("content-type", "application/gzip")
        .body(Body::from(first))?;
    let resp1 = app.router.clone().oneshot(req1).await?;
    assert_eq!(resp1.status(), StatusCode::CREATED);

    // Second upload (merge mode): replaces only b.txt and adds c.txt. a.txt should be preserved.
    let second = make_tarball(&[("b.txt", b"NEWB"), ("c.txt", b"C")]);
    let req2 = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state?mode=merge&y=2")
        .header("content-type", "application/gzip")
        .body(Body::from(second))?;
    let resp2 = app.router.clone().oneshot(req2).await?;
    let status2 = resp2.status();
    let body2 = resp2.into_body().collect().await?.to_bytes();
    assert_eq!(
        status2,
        StatusCode::CREATED,
        "response body: {}",
        String::from_utf8_lossy(&body2)
    );

    // Two snapshots total: first and second.
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM task_state WHERE workspace = $1 AND task_name = $2")
            .bind("production")
            .bind("t")
            .fetch_one(&app.pool)
            .await?;
    assert_eq!(count.0, 2, "both snapshots should exist");

    // Fetch the merged snapshot from the archive by reading from the TempDir.
    let latest = stroem_db::TaskStateRepo::get_latest(&app.pool, "production", "t")
        .await?
        .unwrap();

    let storage_key = latest.storage_key.clone();
    let archive_path = app._tmp.path().join("state-archive").join(&storage_key);
    let bytes = std::fs::read(&archive_path).map_err(|e| {
        anyhow::anyhow!("read merged snapshot at {}: {}", archive_path.display(), e)
    })?;

    let files = stroem_server::web::api::state_upload::unpack_tarball(&bytes)?;
    assert!(
        files.contains_key("a.txt"),
        "a.txt should be preserved from prior snapshot"
    );
    assert_eq!(
        files.get("b.txt").map(|v| v.as_slice()),
        Some(b"NEWB".as_slice())
    );
    assert!(
        files.contains_key("c.txt"),
        "c.txt should be added from upload"
    );
    assert!(
        files.contains_key("state.json"),
        "merged state.json should exist"
    );

    let state: serde_json::Value = serde_json::from_slice(files.get("state.json").unwrap())?;
    assert_eq!(state["x"], "1", "x should be preserved from first upload");
    assert_eq!(state["y"], "2", "y should be added in merge");

    Ok(())
}
