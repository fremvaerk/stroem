//! Integration tests for the MCP `list_artifacts` and `get_artifact` tools.
//!
//! Drives the tools through the same JSON-RPC over `/mcp` flow that real
//! agents use. Artifacts are seeded directly into the blob archive + repo
//! to keep the test focused on the MCP path itself.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::repos::job_artifact::{JobArtifactRepo, NewArtifactRow};
use stroem_db::{create_pool, run_migrations};
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    ArtifactStorageConfig, DbConfig, LogStorageConfig, McpConfig, RetentionConfig, ServerConfig,
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

const TEST_HOST: &str = "localhost";

struct TestApp {
    router: Router,
    pool: PgPool,
    archive: Arc<dyn BlobArchive>,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _tmp: TempDir,
}

async fn build_test_app() -> Result<TestApp> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let artifact_dir = tmp.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url: url.clone() },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: tmp.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-artifact-test-token-long-enough-32c".to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: Some(McpConfig { enabled: true }),
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: Some(ArtifactStorageConfig::default()),
        default_step_timeout: None,
        default_job_timeout: None,
    };

    let workspace = WorkspaceConfig::new();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(artifact_dir));

    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_artifact_blob(Some(archive.clone()));
    let router = build_router(state, CancellationToken::new());

    Ok(TestApp {
        router,
        pool,
        archive,
        _pg: container,
        _tmp: tmp,
    })
}

async fn seed_job(pool: &PgPool, workspace: &str, task_name: &str) -> Result<Uuid> {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at) \
         VALUES ($1, $2, $3, 'completed', 'api', NOW())",
    )
    .bind(job_id)
    .bind(workspace)
    .bind(task_name)
    .execute(pool)
    .await?;
    Ok(job_id)
}

/// Insert an artifact row + matching blob.
async fn seed_artifact(
    app: &TestApp,
    job_id: Uuid,
    step_name: &str,
    name: &str,
    content_type: &str,
    data: &[u8],
) -> Result<()> {
    let storage_key = format!("artifacts/default/{job_id}/{step_name}/{name}");
    app.archive
        .put(&storage_key, content_type, Bytes::copy_from_slice(data))
        .await?;
    JobArtifactRepo::new(app.pool.clone())
        .upsert(NewArtifactRow {
            job_id,
            step_name: step_name.to_string(),
            name: name.to_string(),
            content_type: content_type.to_string(),
            size_bytes: data.len() as i64,
            storage_key,
        })
        .await?;
    Ok(())
}

fn mcp_request(session_id: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Host", TEST_HOST)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if let Some(sid) = session_id {
        builder = builder.header("Mcp-Session-Id", sid);
    }
    builder
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

async fn mcp_initialize(router: Router) -> (Router, Option<String>) {
    let init_body = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "id": 0,
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(None, init_body))
        .await
        .unwrap();
    let session_id = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    (router, session_id)
}

async fn call_tool(
    router: &Router,
    session_id: Option<&str>,
    name: &str,
    args: Value,
) -> Result<Value> {
    let body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 42,
        "params": { "name": name, "arguments": args }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id, body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(body_json(response).await)
}

fn content_text(resp: &Value) -> &str {
    resp["result"]["content"][0]["text"]
        .as_str()
        .expect("expected text content")
}

fn is_error(resp: &Value) -> bool {
    resp.get("error").is_some() || resp["result"]["isError"].as_bool().unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn list_artifacts_returns_seeded_rows() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = seed_job(&app.pool, "default", "task1").await?;
    seed_artifact(&app, job_id, "build", "report.txt", "text/plain", b"hello").await?;
    seed_artifact(
        &app,
        job_id,
        "build",
        "icon.png",
        "image/png",
        &[0x89, 0x50, 0x4E, 0x47],
    )
    .await?;

    let (router, session_id) = mcp_initialize(app.router.clone()).await;
    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "list_artifacts",
        json!({ "job_id": job_id.to_string() }),
    )
    .await?;

    let items: Value = serde_json::from_str(content_text(&resp))?;
    let arr = items.as_array().expect("items must be array");
    assert_eq!(arr.len(), 2);

    let names: Vec<&str> = arr.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"report.txt"));
    assert!(names.contains(&"icon.png"));

    for item in arr {
        let url = item["url"].as_str().unwrap();
        assert!(
            url.starts_with(&format!("/api/jobs/{job_id}/artifacts/")),
            "url should be a relative download path, got: {url}"
        );
        assert!(item["content_type"].is_string());
        assert!(item["size_bytes"].is_i64());
        assert!(item["step_name"].is_string());
        assert!(item["created_at"].is_string());
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn list_artifacts_invalid_uuid_errors() -> Result<()> {
    let app = build_test_app().await?;
    let (router, session_id) = mcp_initialize(app.router.clone()).await;

    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "list_artifacts",
        json!({ "job_id": "not-a-uuid" }),
    )
    .await?;

    assert!(is_error(&resp), "expected error response, got: {resp}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_artifact_returns_textual_content() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = seed_job(&app.pool, "default", "task1").await?;
    let body = b"the answer is 42";
    seed_artifact(
        &app,
        job_id,
        "step",
        "note.txt",
        "text/plain; charset=utf-8",
        body,
    )
    .await?;

    let (router, session_id) = mcp_initialize(app.router.clone()).await;
    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "get_artifact",
        json!({ "job_id": job_id.to_string(), "name": "note.txt" }),
    )
    .await?;

    assert!(!is_error(&resp), "expected success, got: {resp}");
    let text = content_text(&resp);
    assert_eq!(text, "the answer is 42");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_artifact_returns_json_content() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = seed_job(&app.pool, "default", "task1").await?;
    let body = b"{\"ok\":true,\"count\":3}";
    seed_artifact(&app, job_id, "step", "data.json", "application/json", body).await?;

    let (router, session_id) = mcp_initialize(app.router.clone()).await;
    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "get_artifact",
        json!({ "job_id": job_id.to_string(), "name": "data.json" }),
    )
    .await?;

    assert!(!is_error(&resp), "expected success, got: {resp}");
    let text = content_text(&resp);
    let parsed: Value = serde_json::from_str(text)?;
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["count"], 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_artifact_rejects_binary_content_type() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = seed_job(&app.pool, "default", "task1").await?;
    // PNG magic header. Tiny payload — we want the content-type check to win.
    let payload = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    seed_artifact(&app, job_id, "step", "logo.png", "image/png", &payload).await?;

    let (router, session_id) = mcp_initialize(app.router.clone()).await;
    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "get_artifact",
        json!({ "job_id": job_id.to_string(), "name": "logo.png" }),
    )
    .await?;

    assert!(
        is_error(&resp),
        "expected error for binary content_type, got: {resp}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_artifact_rejects_oversize() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = seed_job(&app.pool, "default", "task1").await?;

    // Insert metadata claiming the artifact is > 1 MiB without actually writing
    // the blob — the size guard must short-circuit before the blob fetch.
    let storage_key = format!("artifacts/default/{job_id}/step/huge.txt");
    JobArtifactRepo::new(app.pool.clone())
        .upsert(NewArtifactRow {
            job_id,
            step_name: "step".to_string(),
            name: "huge.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: 2 * 1024 * 1024,
            storage_key,
        })
        .await?;

    let (router, session_id) = mcp_initialize(app.router.clone()).await;
    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "get_artifact",
        json!({ "job_id": job_id.to_string(), "name": "huge.txt" }),
    )
    .await?;

    assert!(
        is_error(&resp),
        "expected error for oversize artifact, got: {resp}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_artifact_missing_name_returns_not_found() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = seed_job(&app.pool, "default", "task1").await?;

    let (router, session_id) = mcp_initialize(app.router.clone()).await;
    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "get_artifact",
        json!({ "job_id": job_id.to_string(), "name": "missing.txt" }),
    )
    .await?;

    assert!(
        is_error(&resp),
        "expected error for missing artifact, got: {resp}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_artifact_invalid_uuid_errors() -> Result<()> {
    let app = build_test_app().await?;
    let (router, session_id) = mcp_initialize(app.router.clone()).await;

    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "get_artifact",
        json!({ "job_id": "bogus", "name": "x.txt" }),
    )
    .await?;

    assert!(is_error(&resp), "expected error response, got: {resp}");
    Ok(())
}
