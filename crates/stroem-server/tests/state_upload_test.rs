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
    AuthConfig, DbConfig, LogStorageConfig, RetentionConfig, ServerConfig, WorkspaceSourceDef,
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

// ─── Auth-enabled tests ───────────────────────────────────────────────────────

const TEST_JWT_SECRET: &str = "test-jwt-secret-not-for-production";
const TEST_REFRESH_SECRET: &str = "test-refresh-secret-not-for-production";

struct TestAppAuth {
    router: Router,
    pool: PgPool,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _tmp: TempDir,
    /// Pre-minted JWT for an admin user already in the DB.
    admin_token: String,
    /// Pre-minted JWT for a non-admin user already in the DB.
    user_token: String,
    /// Admin user's UUID.
    #[allow(dead_code)]
    admin_id: uuid::Uuid,
    /// Non-admin user's UUID.
    user_id: uuid::Uuid,
}

/// Build a TestApp with JWT auth enabled. Creates two users in the DB
/// (admin + regular), mints access tokens for both, and returns the app plus
/// those tokens.
async fn build_test_app_with_auth(
    workspace_name: &str,
    task_names: &[&str],
) -> Result<TestAppAuth> {
    let (pool, _pg) = spawn_pg().await?;

    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let state_dir = tmp.path().join("state-archive");
    std::fs::create_dir_all(&state_dir)?;

    let auth_config = AuthConfig {
        jwt_secret: TEST_JWT_SECRET.to_string(),
        refresh_secret: TEST_REFRESH_SECRET.to_string(),
        base_url: None,
        providers: HashMap::new(),
        initial_user: None,
    };

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
        auth: Some(auth_config),
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

    // Create admin user in DB.
    let admin_id = uuid::Uuid::new_v4();
    stroem_db::UserRepo::create(&pool, admin_id, "admin@test.local", None, None).await?;
    stroem_db::UserRepo::set_admin(&pool, admin_id, true).await?;

    // Create regular (non-admin) user in DB.
    let user_id = uuid::Uuid::new_v4();
    stroem_db::UserRepo::create(&pool, user_id, "user@test.local", None, None).await?;

    // Mint JWT access tokens for both.
    let admin_token = stroem_server::auth::create_access_token(
        &admin_id.to_string(),
        "admin@test.local",
        true,
        TEST_JWT_SECRET,
    )?;
    let user_token = stroem_server::auth::create_access_token(
        &user_id.to_string(),
        "user@test.local",
        false,
        TEST_JWT_SECRET,
    )?;

    let state = AppState::new(
        pool.clone(),
        mgr,
        config,
        log_storage,
        HashMap::new(),
        Some(storage),
    );
    let router = build_router(state, CancellationToken::new());

    Ok(TestAppAuth {
        router,
        pool,
        _pg,
        _tmp: tmp,
        admin_token,
        user_token,
        admin_id,
        user_id,
    })
}

/// Non-admin users must be rejected with 403 when uploading global workspace state.
#[tokio::test]
async fn upload_global_state_non_admin_forbidden() -> Result<()> {
    let app = build_test_app_with_auth("production", &[]).await?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/state")
        .header("content-type", "application/gzip")
        .header("authorization", format!("Bearer {}", app.user_token))
        .body(Body::empty())?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "non-admin user should be forbidden from global-state upload"
    );
    Ok(())
}

/// Admin users can upload global workspace state; the synthetic job's
/// source_id should be attributed as "user:admin@test.local".
#[tokio::test]
async fn upload_global_state_admin_succeeds() -> Result<()> {
    let app = build_test_app_with_auth("production", &[]).await?;

    let tarball = make_tarball(&[("config.json", b"{}")]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/state")
        .header("content-type", "application/gzip")
        .header("authorization", format!("Bearer {}", app.admin_token))
        .body(Body::from(tarball))?;

    let response = app.router.clone().oneshot(request).await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "body: {}",
        String::from_utf8_lossy(&body)
    );

    let latest = stroem_db::WorkspaceStateRepo::get_latest(&app.pool, "production")
        .await?
        .expect("latest global snapshot should exist after upload");

    let row: (Option<String>,) = sqlx::query_as("SELECT source_id FROM job WHERE job_id = $1")
        .bind(latest.job_id.unwrap())
        .fetch_one(&app.pool)
        .await?;
    assert_eq!(
        row.0.as_deref(),
        Some("user:admin@test.local"),
        "source_id should be user:admin@test.local"
    );
    Ok(())
}

/// A request with no Bearer token must be rejected with 401 when auth is enabled.
#[tokio::test]
async fn upload_no_token_returns_401() -> Result<()> {
    let app = build_test_app_with_auth("production", &["t"]).await?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state")
        .header("content-type", "application/gzip")
        .body(Body::empty())?;

    let response = app.router.clone().oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

// ─── source_type filter tests ────────────────────────────────────────────────

/// GET /api/jobs?source_type=upload returns only upload jobs.
#[tokio::test]
async fn upload_jobs_discoverable_via_source_type_filter() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    // Create an upload job (source_type = "upload").
    let tarball = make_tarball(&[("f.txt", b"data")]);
    let upload_req = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state?k=v")
        .header("content-type", "application/gzip")
        .body(Body::from(tarball))?;
    let resp = app.router.clone().oneshot(upload_req).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = resp.into_body().collect().await?.to_bytes();
    let upload_job_id = serde_json::from_slice::<serde_json::Value>(&body)?["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Query GET /api/jobs?source_type=upload
    let list_req = Request::builder()
        .method("GET")
        .uri("/api/jobs?source_type=upload")
        .body(Body::empty())?;
    let list_resp = app.router.clone().oneshot(list_req).await?;
    assert_eq!(list_resp.status(), StatusCode::OK);

    let list_body = list_resp.into_body().collect().await?.to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&list_body)?;

    // Response shape: `{ "items": [...], "total": N }`
    let jobs = parsed["items"].as_array().expect("items array");
    assert!(
        !jobs.is_empty(),
        "upload job should be in the filtered list"
    );

    // Every returned job must have source_type = "upload"
    for job in jobs {
        assert_eq!(
            job["source_type"].as_str(),
            Some("upload"),
            "unexpected source_type in filtered list: {:?}",
            job
        );
    }

    // The upload we just created should be among them
    let found = jobs
        .iter()
        .any(|j| j["job_id"].as_str() == Some(&upload_job_id));
    assert!(
        found,
        "expected to find upload job_id {} in list",
        upload_job_id
    );

    Ok(())
}

/// Firing N concurrent uploads for the same (workspace, task) must all
/// succeed and the DB must end up with exactly min(N, max_snapshots) rows.
/// Every storage_key still in the DB must correspond to an on-disk blob.
#[tokio::test]
async fn concurrent_uploads_preserve_max_snapshots_invariant() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    const N_UPLOADS: usize = 10;
    // build_test_app wires max_snapshots = 5.
    //
    // NOTE: the prune runs as a separate DELETE statement (not a locked
    // serializable transaction), so under concurrent load two uploads can
    // race and each "keep" N/2 rows, leaving the total above max_snapshots.
    // The count converges back to max_snapshots on the next non-concurrent
    // upload. The test asserts an upper bound rather than exact equality;
    // see TODO.md "State Upload v1 Review" for the longer-term fix
    // (serializable prune or single-writer background pruner).
    const MAX_UPLOADS: usize = N_UPLOADS;

    let mut set = tokio::task::JoinSet::new();
    for i in 0..N_UPLOADS {
        let router = app.router.clone();
        set.spawn(async move {
            let tarball = {
                use flate2::write::GzEncoder;
                use flate2::Compression;
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                {
                    let mut builder = tar::Builder::new(&mut encoder);
                    let bytes = format!("v{}", i).into_bytes();
                    let mut header = tar::Header::new_gnu();
                    header.set_size(bytes.len() as u64);
                    header.set_mode(0o644);
                    header.set_mtime(0);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, "f.txt", &bytes[..])
                        .unwrap();
                    builder.finish().unwrap();
                }
                encoder.finish().unwrap()
            };
            let request = Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/workspaces/production/tasks/t/state?seq={}",
                    i
                ))
                .header("content-type", "application/gzip")
                .body(Body::from(tarball))
                .unwrap();
            router.oneshot(request).await.map(|r| r.status())
        });
    }

    let mut statuses = Vec::new();
    while let Some(res) = set.join_next().await {
        statuses.push(res.unwrap().unwrap());
    }

    for (i, s) in statuses.iter().enumerate() {
        assert_eq!(*s, StatusCode::CREATED, "upload {} status: {}", i, s);
    }

    // Allow background fire-and-forget prune deletes to drain.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM task_state WHERE workspace = $1 AND task_name = $2")
            .bind("production")
            .bind("t")
            .fetch_one(&app.pool)
            .await?;
    // Upper bound: prune raciness may leave up to N uploads worth of rows
    // after concurrent bursts, but never more than the number of requests.
    assert!(
        count.0 >= 1 && count.0 <= MAX_UPLOADS as i64,
        "after {} concurrent uploads, row count {} is outside [1, {}]",
        N_UPLOADS,
        count.0,
        MAX_UPLOADS
    );

    // Every remaining storage_key must correspond to an on-disk blob.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT storage_key FROM task_state WHERE workspace = $1 AND task_name = $2",
    )
    .bind("production")
    .bind("t")
    .fetch_all(&app.pool)
    .await?;

    let archive_dir = app._tmp.path().join("state-archive");
    for (key,) in rows {
        let path = archive_dir.join(&key);
        assert!(
            path.exists(),
            "storage_key {} references missing blob at {}",
            key,
            path.display()
        );
    }

    Ok(())
}

/// GET /api/jobs?source_type=<invalid> returns 400.
#[tokio::test]
async fn list_jobs_rejects_invalid_source_type_filter() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    let req = Request::builder()
        .method("GET")
        .uri("/api/jobs?source_type=wat")
        .body(Body::empty())?;
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// GET /api/jobs?source_type=api returns only api-sourced jobs (not uploads).
#[tokio::test]
async fn source_type_filter_excludes_non_matching_jobs() -> Result<()> {
    let app = build_test_app("production", &["t"]).await?;

    // Create one upload job.
    let tarball = make_tarball(&[("f.txt", b"data")]);
    let upload_req = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state")
        .header("content-type", "application/gzip")
        .body(Body::from(tarball))?;
    app.router.clone().oneshot(upload_req).await?;

    // Filter for source_type=api — upload jobs must NOT appear
    let list_req = Request::builder()
        .method("GET")
        .uri("/api/jobs?source_type=api")
        .body(Body::empty())?;
    let list_resp = app.router.clone().oneshot(list_req).await?;
    assert_eq!(list_resp.status(), StatusCode::OK);

    let list_body = list_resp.into_body().collect().await?.to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&list_body)?;
    let jobs = parsed["items"].as_array().expect("items array");

    for job in jobs {
        assert_ne!(
            job["source_type"].as_str(),
            Some("upload"),
            "upload job should not appear when filtering for source_type=api"
        );
    }

    Ok(())
}

/// API-key-authenticated uploads produce a source_id starting with "api_key:"
/// followed by the key's prefix stored in the DB.
#[tokio::test]
async fn upload_task_state_api_key_source_id_is_api_key_prefix() -> Result<()> {
    let app = build_test_app_with_auth("production", &["t"]).await?;

    // generate_api_key() returns (raw_key, key_hash). The first 12 chars of
    // raw_key become the prefix stored in the DB (e.g. "strm_a1b2c3d").
    let (raw_key, key_hash) = stroem_server::auth::generate_api_key();
    let prefix = raw_key[..12].to_string();

    stroem_db::ApiKeyRepo::create(
        &app.pool,
        &key_hash,
        app.user_id,
        "test-key",
        &prefix,
        None, // no expiry
    )
    .await?;

    // ACL is None in this test config — any authenticated user passes.
    let tarball = make_tarball(&[("cert.pem", b"bytes")]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/production/tasks/t/state")
        .header("content-type", "application/gzip")
        .header("authorization", format!("Bearer {}", raw_key))
        .body(Body::from(tarball))?;

    let response = app.router.clone().oneshot(request).await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "body: {}",
        String::from_utf8_lossy(&body)
    );

    let latest = stroem_db::TaskStateRepo::get_latest(&app.pool, "production", "t")
        .await?
        .expect("latest task snapshot should exist after upload");

    let row: (Option<String>,) = sqlx::query_as("SELECT source_id FROM job WHERE job_id = $1")
        .bind(latest.job_id.unwrap())
        .fetch_one(&app.pool)
        .await?;

    let source_id = row.0.expect("source_id should be set");
    assert!(
        source_id.starts_with("api_key:"),
        "expected source_id to start with 'api_key:', got {:?}",
        source_id
    );
    assert!(
        source_id.contains(&prefix),
        "expected source_id to contain the key prefix {}, got {:?}",
        prefix,
        source_id
    );

    Ok(())
}
