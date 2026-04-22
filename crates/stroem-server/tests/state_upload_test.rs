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

/// Build a raw gzip tarball where the path is written directly into the header
/// bytes, bypassing the `tar` crate's path-safety checks. Used to craft
/// hostile entries (`../foo`, `/etc/shadow`) for security tests.
fn make_hostile_tarball(raw_path: &str, content: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let block_size = 512usize;
    let data_blocks = content.len().div_ceil(block_size);
    let total_blocks = 1 + data_blocks + 2;
    let mut tar_bytes = vec![0u8; total_blocks * block_size];

    let name_bytes = raw_path.as_bytes();
    let copy_len = name_bytes.len().min(99);
    tar_bytes[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

    tar_bytes[100..107].copy_from_slice(b"0000644");
    tar_bytes[108..115].copy_from_slice(b"0000000");
    tar_bytes[116..123].copy_from_slice(b"0000000");
    let size_str = format!("{:011o}\0", content.len());
    tar_bytes[124..136].copy_from_slice(size_str.as_bytes());
    tar_bytes[136..147].copy_from_slice(b"00000000000");
    tar_bytes[156] = b'0';
    tar_bytes[257..265].copy_from_slice(b"ustar  \0");

    for b in tar_bytes[148..156].iter_mut() {
        *b = b' ';
    }
    let chksum: u32 = tar_bytes[..block_size].iter().map(|&b| b as u32).sum();
    let chksum_str = format!("{:06o}\0 ", chksum);
    tar_bytes[148..156].copy_from_slice(chksum_str.as_bytes());

    tar_bytes[block_size..block_size + content.len()].copy_from_slice(content);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar_bytes).unwrap();
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

/// Inner builder shared by `build_test_app` and `build_test_app_no_storage`.
async fn build_test_app_inner(
    workspace_name: &str,
    task_names: &[&str],
    with_state_storage: bool,
) -> Result<TestApp> {
    let (pool, _pg) = spawn_pg().await?;

    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

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

    let storage = if with_state_storage {
        let state_dir = tmp.path().join("state-archive");
        std::fs::create_dir_all(&state_dir)?;
        let archive: Arc<dyn StateArchive> = Arc::new(LocalStateArchive::new(&state_dir));
        Some(StateStorage::new(archive, "state/".to_string(), 5, None))
    } else {
        None
    };

    let state = AppState::new(
        pool.clone(),
        mgr,
        config,
        log_storage,
        HashMap::new(),
        storage,
    );
    let router = build_router(state, CancellationToken::new());

    Ok(TestApp {
        router,
        pool,
        _pg,
        _tmp: tmp,
    })
}

/// Build a Router wired to a real testcontainers Postgres, with a workspace
/// containing the named tasks and a local-filesystem state backend.
async fn build_test_app(workspace_name: &str, task_names: &[&str]) -> Result<TestApp> {
    build_test_app_inner(workspace_name, task_names, true).await
}

/// Build a test app where `state_storage` is `None` (feature disabled).
/// Uploads should return 404 "State storage not configured".
async fn build_test_app_no_storage(workspace_name: &str, task_names: &[&str]) -> Result<TestApp> {
    build_test_app_inner(workspace_name, task_names, false).await
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
async fn upload_rejects_malformed_gzip() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state")
        .header("content-type", "application/gzip")
        .body(Body::from(b"this is not gzip".to_vec()))?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn upload_rejects_oversize_body() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    // 51 MB — should trigger the DefaultBodyLimit layer (413) before the handler runs.
    let oversize = vec![0u8; 50 * 1024 * 1024 + 1];

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state")
        .header("content-type", "application/gzip")
        .body(Body::from(oversize))?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

#[tokio::test]
async fn upload_rejects_path_traversal_in_tarball() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    let tarball = make_hostile_tarball("../escape.pem", b"bad");

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

#[tokio::test]
async fn upload_task_state_missing_workspace_returns_404() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    let tarball = make_tarball(&[]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/nonexistent/tasks/t/state")
        .header("content-type", "application/gzip")
        .body(Body::from(tarball))?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn upload_global_state_missing_workspace_returns_404() -> Result<()> {
    let app = build_test_app("production", &[]).await?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/nonexistent/state")
        .header("content-type", "application/gzip")
        .body(Body::empty())?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn upload_task_state_missing_task_returns_404() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    let tarball = make_tarball(&[]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/does-not-exist/state")
        .header("content-type", "application/gzip")
        .body(Body::from(tarball))?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn upload_task_state_no_storage_returns_404() -> Result<()> {
    let app = build_test_app_no_storage("production", &["t"]).await?;

    let tarball = make_tarball(&[]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state")
        .header("content-type", "application/gzip")
        .body(Body::from(tarball))?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn upload_global_state_no_storage_returns_404() -> Result<()> {
    let app = build_test_app_no_storage("production", &[]).await?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/state")
        .header("content-type", "application/gzip")
        .body(Body::empty())?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn upload_max_snapshots_prunes_archive_blobs() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    // Upload 6 snapshots — max_snapshots is 5, so the oldest should be pruned.
    for i in 0..6 {
        let tarball = make_tarball(&[("f.txt", format!("v{}", i).as_bytes())]);
        let request = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/workspaces/production/tasks/t/state?seq={}",
                i
            ))
            .header("content-type", "application/gzip")
            .body(Body::from(tarball))?;
        let response = app.router.clone().oneshot(request).await?;
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "upload {} failed",
            i
        );
    }

    // Poll until the fire-and-forget background prune task completes (max 2 s).
    for _ in 0..20 {
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM task_state WHERE workspace = $1 AND task_name = $2",
        )
        .bind("production")
        .bind("t")
        .fetch_one(&app.pool)
        .await?;
        if count.0 == 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM task_state WHERE workspace = $1 AND task_name = $2")
            .bind("production")
            .bind("t")
            .fetch_one(&app.pool)
            .await?;
    assert_eq!(count.0, 5, "expected 5 rows after pruning, got {}", count.0);

    // Also verify on-disk archive blobs — there should be exactly 5 .tar.gz files
    // under the state archive directory. Poll briefly for background storage.delete calls.
    let archive_dir = app
        ._tmp
        .path()
        .join("state-archive")
        .join("state")
        .join("production")
        .join("t");

    for _ in 0..20 {
        if archive_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&archive_dir)?.collect();
            if entries.len() == 5 {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let entries: Vec<_> = std::fs::read_dir(&archive_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(
        entries.len(),
        5,
        "expected 5 archive blobs on disk, got {}",
        entries.len()
    );

    Ok(())
}
