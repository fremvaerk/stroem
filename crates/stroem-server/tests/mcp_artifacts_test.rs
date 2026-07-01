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

// ─── Size boundary tests ────────────────────────────────────────────────────

/// Artifact of exactly 1 MiB is accepted by `get_artifact` — the MCP cap is
/// inclusive at the documented limit.
#[tokio::test(flavor = "multi_thread")]
async fn get_artifact_at_1mib_exactly_passes() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = seed_job(&app.pool, "default", "task1").await?;

    let body = vec![b'a'; 1024 * 1024];
    seed_artifact(&app, job_id, "step", "exact.txt", "text/plain", &body).await?;

    let (router, session_id) = mcp_initialize(app.router.clone()).await;
    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "get_artifact",
        json!({ "job_id": job_id.to_string(), "name": "exact.txt" }),
    )
    .await?;

    assert!(
        !is_error(&resp),
        "expected success at exactly 1 MiB, got: {resp}"
    );
    let text = content_text(&resp);
    assert_eq!(text.len(), 1024 * 1024);
    assert!(text.chars().all(|c| c == 'a'));
    Ok(())
}

/// Artifact of 1 MiB + 1 byte is rejected by `get_artifact`. We can't actually
/// stuff a 1 MiB blob through the test harness here just to assert the
/// rejection, so we seed metadata claiming size_bytes = 1 MiB + 1 without
/// writing the blob — the size guard must short-circuit before the blob fetch.
#[tokio::test(flavor = "multi_thread")]
async fn get_artifact_one_byte_over_1mib_returns_invalid_params() -> Result<()> {
    let app = build_test_app().await?;
    let job_id = seed_job(&app.pool, "default", "task1").await?;

    let storage_key = format!("artifacts/default/{job_id}/step/over.txt");
    JobArtifactRepo::new(app.pool.clone())
        .upsert(NewArtifactRow {
            job_id,
            step_name: "step".to_string(),
            name: "over.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: (1024 * 1024) + 1,
            storage_key,
        })
        .await?;

    let (router, session_id) = mcp_initialize(app.router.clone()).await;
    let resp = call_tool(
        &router,
        session_id.as_deref(),
        "get_artifact",
        json!({ "job_id": job_id.to_string(), "name": "over.txt" }),
    )
    .await?;

    assert!(
        is_error(&resp),
        "expected error one byte over 1 MiB, got: {resp}"
    );
    Ok(())
}

// ─── ACL Deny test ──────────────────────────────────────────────────────────

/// `list_artifacts` must refuse with a tool error when the ACL denies access
/// to the job's task — *not* return an empty list, which would look
/// indistinguishable from "no artifacts uploaded yet" to the agent and could
/// mask the ACL boundary.
mod acl_deny {
    use super::*;
    use stroem_db::UserRepo;
    use stroem_server::auth::hash_password;
    use stroem_server::config::{AclAction, AclConfig, AuthConfig, InitialUserConfig};

    const JWT_SECRET: &str = "mcp-artifact-acl-test-jwt-secret-long-enough";
    const REFRESH_SECRET: &str = "mcp-artifact-acl-test-refresh-secret-long-enough";
    const USER_EMAIL: &str = "user@example.com";
    const USER_PASSWORD: &str = "test-password-1234";

    struct AuthApp {
        router: Router,
        pool: PgPool,
        archive: Arc<dyn BlobArchive>,
        _pg: testcontainers::ContainerAsync<Postgres>,
        _tmp: TempDir,
    }

    async fn build_auth_app_with_default_deny() -> Result<AuthApp> {
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
            worker_token: "mcp-artifact-acl-worker-token-long-enough".to_string(),
            auth: Some(AuthConfig {
                jwt_secret: JWT_SECRET.to_string(),
                refresh_secret: REFRESH_SECRET.to_string(),
                base_url: None,
                providers: HashMap::new(),
                initial_user: Some(InitialUserConfig {
                    email: USER_EMAIL.to_string(),
                    password: USER_PASSWORD.to_string(),
                }),
            }),
            recovery: Default::default(),
            retention: RetentionConfig::default(),
            // Default-deny ACL with no rules → every task evaluates Deny for
            // the non-admin test user.
            acl: Some(AclConfig {
                default: AclAction::Deny,
                rules: vec![],
            }),
            mcp: Some(McpConfig { enabled: true }),
            metrics: None,
            agents: None,
            state_storage: None,
            artifact_storage: Some(ArtifactStorageConfig::default()),
            default_step_timeout: None,
            default_job_timeout: None,
        };

        // Seed the initial non-admin user so login can succeed.
        let password_hash = hash_password(USER_PASSWORD)?;
        UserRepo::create(
            &pool,
            uuid::Uuid::new_v4(),
            USER_EMAIL,
            Some(&password_hash),
            None,
        )
        .await?;

        let workspace = WorkspaceConfig::new();
        let mgr = WorkspaceManager::from_config("default", workspace);
        let log_storage = LogStorage::new(&config.log_storage.local_dir);
        let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(artifact_dir));

        let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
            .with_artifact_blob(Some(archive.clone()));
        let router = build_router(state, CancellationToken::new());

        Ok(AuthApp {
            router,
            pool,
            archive,
            _pg: container,
            _tmp: tmp,
        })
    }

    /// Pool-aware login: mints an API key for the seeded user and returns it.
    ///
    /// Post-OAuth-2.1 refactor, login JWTs no longer authenticate `/mcp`
    /// (only audience-bound OAuth tokens or `strm_*` API keys do). We use
    /// the API-key path here to keep the artifact tests focused on ACL
    /// behaviour without dragging in the full OAuth flow.
    async fn login_with_pool(pool: &sqlx::PgPool) -> Result<String> {
        let user = stroem_db::UserRepo::get_by_email(pool, USER_EMAIL)
            .await?
            .expect("seeded user exists");
        let (raw_key, key_hash) = stroem_server::auth::generate_api_key();
        let prefix = raw_key[..12].to_string();
        stroem_db::ApiKeyRepo::create(pool, &key_hash, user.user_id, "mcp-test", &prefix, None)
            .await?;
        Ok(raw_key)
    }

    fn mcp_request_with_auth(session_id: Option<&str>, token: &str, body: Value) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", TEST_HOST)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", format!("Bearer {token}"));
        if let Some(sid) = session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        builder
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap()
    }

    async fn initialize_authed(router: Router, token: &str) -> (Router, Option<String>) {
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
            .oneshot(mcp_request_with_auth(None, token, init_body))
            .await
            .unwrap();
        let session_id = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        (router, session_id)
    }

    async fn seed_artifact_for_auth_app(
        app: &AuthApp,
        job_id: uuid::Uuid,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn list_artifacts_acl_deny_returns_tool_error_not_empty_list() -> Result<()> {
        let app = build_auth_app_with_default_deny().await?;

        // Seed a job under a task that ACL denies. We still create the row so
        // that "ACL denies" is the *only* reason the tool fails (we want to
        // distinguish from "no such job" / "no rows yet").
        let job_id = super::seed_job(&app.pool, "default", "task-denied").await?;
        seed_artifact_for_auth_app(&app, job_id, "build", "x.txt", "text/plain", b"hello").await?;

        // Sanity: with default-deny ACL, listing under our non-admin user
        // must error (not return `[]`).
        let token = login_with_pool(&app.pool).await?;
        let (router, session_id) = initialize_authed(app.router.clone(), &token).await;

        let body = json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 99,
            "params": {
                "name": "list_artifacts",
                "arguments": { "job_id": job_id.to_string() }
            }
        });
        let response = router
            .oneshot(mcp_request_with_auth(session_id.as_deref(), &token, body))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let resp = body_json(response).await;

        // The critical assertion: the tool surfaces an *error*, never an
        // empty list.
        assert!(
            is_error(&resp),
            "expected tool error on ACL deny, got: {resp}"
        );

        // Belt-and-suspenders: even if some change accidentally moved this to
        // a success-with-content path, the content must not be an empty array
        // — that's the failure mode this test is guarding against.
        if let Some(text) = resp["result"]["content"][0]["text"].as_str() {
            let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
            assert_ne!(
                parsed,
                json!([]),
                "ACL deny must not collapse to an empty artifact list (would mask the boundary)"
            );
        }
        Ok(())
    }
}
