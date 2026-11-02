use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::{
    ActionDef, FlowStep, HookDef, InputFieldDef, TaskDef, WorkspaceConfig,
};
use stroem_db::{JobRepo, JobStepRepo, UserRepo, WorkerRepo};
use stroem_server::auth::hash_password;
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    AclAction, AclConfig, AclRule, AuthConfig, DbConfig, InitialUserConfig, JobDefaults,
    LogStorageConfig, McpConfig, RetentionConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::{WorkspaceManager, WorkspaceMetadata};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

// ─── Auth test constants ─────────────────────────────────────────────────────

const AUTH_JWT_SECRET: &str = "mcp-test-jwt-secret-key-long-enough";
const AUTH_REFRESH_SECRET: &str = "mcp-test-refresh-secret-key-long-enough";
const AUTH_USER_EMAIL: &str = "mcp-admin@test.com";
const AUTH_USER_PASSWORD: &str = "mcp-test-password-123";

// ─── Test helpers ───────────────────────────────────────────────────────

/// Pool-only stand-in for the orchestrator call: a minimal workspace holding
/// just this task, since the cascade requires a config for rendering.
fn workspace_with(task: &TaskDef) -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::default();
    ws.tasks.insert("test-task".to_string(), task.clone());
    ws
}

async fn after_step(pool: &PgPool, job_id: Uuid, task: &TaskDef) -> anyhow::Result<()> {
    stroem_server::settlement::cascade_and_settle(
        pool,
        job_id,
        task,
        &workspace_with(task),
        &stroem_server::render_context::Snapshots::default(),
    )
    .await
    .map(|_| ())
}

// ─── Minimal workspace for MCP tests ────────────────────────────────────────

// Thin wrapper preserving the pre-Task-4 call shape (Task 4 added a leading
// `&WorkspaceManager` for cross-workspace action resolution). Builds a one-entry
// manager from the passed config so single-workspace tests behave identically.
#[allow(clippy::too_many_arguments)]
async fn create_job_for_task(
    pool: &PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    task_name: &str,
    input: Value,
    source_type: &str,
    source_id: Option<&str>,
    revision: Option<&str>,
    source_job_id: Option<Uuid>,
    agents_config: Option<&stroem_server::config::AgentsConfig>,
    defaults: JobDefaults,
) -> Result<Uuid> {
    let mgr = WorkspaceManager::from_config(workspace_name, workspace_config.clone());
    stroem_server::job_creator::create_job_for_task_detailed(
        &mgr,
        pool,
        workspace_config,
        workspace_name,
        task_name,
        input,
        source_type,
        source_id,
        revision,
        source_job_id,
        agents_config,
        defaults,
        stroem_server::input_files::FileGrant::Indirect,
    )
    .await
    .map(|c| c.job_id)
}

fn mcp_test_workspace() -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();

    // Action: greet (script)
    let mut greet_input = HashMap::new();
    greet_input.insert(
        "name".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: true,
            secret: false,
            default: None,
            options: None,
            allow_custom: false,
            multiple: false,
            order: None,
        },
    );
    workspace.actions.insert(
        "greet".to_string(),
        ActionDef {
            git_ref: None,
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo Hello $NAME".to_string()),
            source: None,
            runner: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
            args: vec![],
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: greet_input,
            output: None,
            manifest: None,
            provider: None,
            model: None,
            system_prompt: None,
            prompt: None,
            temperature: None,
            max_tokens: None,
            tools: vec![],
            max_turns: None,
            interactive: false,
            message: None,
            retry: None,
        },
    );

    // Action: notify (hook action)
    let mut notify_input = HashMap::new();
    notify_input.insert(
        "message".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: false,
            secret: false,
            default: None,
            options: None,
            allow_custom: false,
            multiple: false,
            order: None,
        },
    );
    workspace.actions.insert(
        "notify".to_string(),
        ActionDef {
            git_ref: None,
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo Notifying: $MESSAGE".to_string()),
            source: None,
            runner: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
            args: vec![],
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: notify_input,
            output: None,
            manifest: None,
            provider: None,
            model: None,
            system_prompt: None,
            prompt: None,
            temperature: None,
            max_tokens: None,
            tools: vec![],
            max_turns: None,
            interactive: false,
            message: None,
            retry: None,
        },
    );

    // Task: hello-world
    let mut hello_flow = HashMap::new();
    let mut hello_input_vals = HashMap::new();
    hello_input_vals.insert("name".to_string(), json!("{{ input.name }}"));
    hello_flow.insert(
        "greet".to_string(),
        FlowStep {
            git_ref: None,
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: hello_input_vals,
            continue_on_failure: false,
            legacy_continue_when_skipped: None,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        },
    );
    let mut task_input = HashMap::new();
    task_input.insert(
        "name".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: true,
            secret: false,
            default: None,
            options: None,
            allow_custom: false,
            multiple: false,
            order: None,
        },
    );
    workspace.tasks.insert(
        "hello-world".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: task_input,
            flow: hello_flow,
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );

    // Task: with-hook (has on_success hook)
    let mut hook_flow = HashMap::new();
    hook_flow.insert(
        "step1".to_string(),
        FlowStep {
            git_ref: None,
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            legacy_continue_when_skipped: None,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        },
    );
    let mut hook_input = HashMap::new();
    hook_input.insert("message".to_string(), json!("Done: {{ hook.task_name }}"));
    workspace.tasks.insert(
        "with-hook".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: hook_flow,
            timeout: None,
            retry: None,
            on_success: vec![HookDef {
                git_ref: None,
                action: "notify".to_string(),
                input: hook_input,
            }],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );

    workspace
}

// ─── Setup helpers ───────────────────────────────────────────────────────────

async fn setup_with_mcp() -> Result<(Router, PgPool, TempDir)> {
    setup_with_mcp_using(WorkspaceManager::from_config(
        "default",
        mcp_test_workspace(),
    ))
    .await
}

async fn setup_with_mcp_using(mgr: WorkspaceManager) -> Result<(Router, PgPool, TempDir)> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool.clone();
    let url = test_db.url;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                display_name: None,
                description: None,
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-test-token-secret-long-enough-32c".to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: Some(McpConfig {
            enabled: true,
            ..Default::default()
        }),
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        input_files: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        leader: Default::default(),
        pin_store: None,
    };

    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let archive: Arc<dyn BlobArchive> =
        Arc::new(LocalBlobArchive::new(temp_dir.path().join("input-archive")));
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_input_blob(Some(archive));
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir))
}

async fn setup_mcp_disabled() -> Result<(Router, PgPool, TempDir)> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool.clone();
    let url = test_db.url;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                display_name: None,
                description: None,
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-test-token-secret-long-enough-32c".to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None, // MCP disabled
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        input_files: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        leader: Default::default(),
        pin_store: None,
    };

    let workspace = mcp_test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir))
}

async fn setup_with_auth_and_mcp() -> Result<(Router, PgPool, TempDir)> {
    setup_with_auth_and_mcp_cfg(None, None).await
}

/// Like [`setup_with_auth_and_mcp`] but lets a test configure ACL and pin the
/// canonical issuer via `base_url` (so audience-bound tokens can be forged with
/// a deterministic `aud`).
async fn setup_with_auth_and_mcp_cfg(
    acl: Option<AclConfig>,
    base_url: Option<String>,
) -> Result<(Router, PgPool, TempDir)> {
    setup_with_auth_and_mcp_ws(mcp_test_workspace(), acl, base_url).await
}

async fn setup_with_auth_and_mcp_ws(
    workspace: WorkspaceConfig,
    acl: Option<AclConfig>,
    base_url: Option<String>,
) -> Result<(Router, PgPool, TempDir)> {
    setup_with_auth_and_mcp_ws_blob(workspace, acl, base_url, true).await
}

/// `setup_with_auth_and_mcp_ws`; `with_blob: false` leaves the server with
/// no input-file archive (`file storage is not configured`).
async fn setup_with_auth_and_mcp_ws_blob(
    workspace: WorkspaceConfig,
    acl: Option<AclConfig>,
    base_url: Option<String>,
    with_blob: bool,
) -> Result<(Router, PgPool, TempDir)> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool.clone();
    let url = test_db.url;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url: url.clone() },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                display_name: None,
                description: None,
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-test-token-secret-long-enough-32c".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: AUTH_JWT_SECRET.to_string(),
            refresh_secret: AUTH_REFRESH_SECRET.to_string(),
            base_url,
            providers: HashMap::new(),
            initial_user: Some(InitialUserConfig {
                email: AUTH_USER_EMAIL.to_string(),
                password: AUTH_USER_PASSWORD.to_string(),
            }),
            rate_limit: Default::default(),
        }),
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl,
        mcp: Some(McpConfig {
            enabled: true,
            ..Default::default()
        }),
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        input_files: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        leader: Default::default(),
        pin_store: None,
    };

    // Seed initial user
    let password_hash = hash_password(AUTH_USER_PASSWORD)?;
    UserRepo::create(
        &pool,
        Uuid::new_v4(),
        AUTH_USER_EMAIL,
        Some(&password_hash),
        None,
    )
    .await?;

    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let archive: Option<Arc<dyn BlobArchive>> = with_blob.then(|| {
        Arc::new(LocalBlobArchive::new(temp_dir.path().join("input-archive")))
            as Arc<dyn BlobArchive>
    });
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_input_blob(archive);
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir))
}

// ─── Request helpers ─────────────────────────────────────────────────────────

/// rmcp 1.5+ enforces a Host header check (DNS-rebinding protection per the
/// MCP spec's "Origin / Host validation" requirement). The bare hyper `Request::builder()`
/// doesn't synthesize a Host like a real HTTP client would, so tests must set it
/// explicitly. The value is arbitrary as long as it's present.
const TEST_HOST: &str = "localhost";

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

async fn body_json(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

/// Register a test worker in the DB and return its UUID.
async fn register_test_worker(pool: &PgPool) -> Uuid {
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        &[],
        false,
        None,
    )
    .await
    .expect("Failed to register test worker");
    worker_id
}

/// Send an MCP initialize request and return the session ID header value.
/// In stateless mode, the server may not return a session ID — that is fine.
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

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Test 1: tools/list returns all 11 expected tools.
#[tokio::test]
async fn test_mcp_tools_list_returns_all_11_tools() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_mcp().await?;

    let (router, session_id) = mcp_initialize(router).await;

    // Send notifications/initialized (no response expected, but still valid)
    let notif_body = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let _ = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), notif_body))
        .await?;

    // Send tools/list
    let list_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 1
    });
    let response = router
        .oneshot(mcp_request(session_id.as_deref(), list_body))
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    let tools = body["result"]["tools"]
        .as_array()
        .expect("tools should be an array");

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    let expected = [
        "list_workspaces",
        "list_tasks",
        "get_task",
        "execute_task",
        "get_job_status",
        "get_job_logs",
        "list_jobs",
        "cancel_job",
        "list_artifacts",
        "get_artifact",
        "create_upload_url",
    ];

    assert_eq!(
        tool_names.len(),
        expected.len(),
        "expected {} tools, got {}: {:?}",
        expected.len(),
        tool_names.len(),
        tool_names
    );

    for name in &expected {
        assert!(
            tool_names.contains(name),
            "missing tool '{name}', got: {tool_names:?}"
        );
    }

    Ok(())
}

/// Test 2: MCP endpoint disabled — no JSON-RPC result returned.
///
/// When MCP is disabled the `/mcp` route is not registered.
/// Axum's static file fallback handler serves `index.html` (200) or 404 depending on
/// whether the embedded `static/` directory is present. Either way the response body
/// must NOT be a valid JSON-RPC 2.0 success envelope.
#[tokio::test]
async fn test_mcp_endpoint_disabled_returns_404() -> Result<()> {
    let (router, _pool, _tmp) = setup_mcp_disabled().await?;

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
    let response = router.oneshot(mcp_request(None, init_body)).await?;

    let status = response.status();
    let raw = response.into_body().collect().await?.to_bytes();

    // If the status is 200 the body must not be a JSON-RPC envelope (it is the SPA).
    // If the status is 404 we're done. Either way, no JSON-RPC `result` key expected.
    if status == StatusCode::OK {
        // Try to parse as JSON; if it succeeds it must not have a `result` field.
        if let Ok(body) = serde_json::from_slice::<Value>(&raw) {
            assert!(
                body.get("result").is_none(),
                "should not have JSON-RPC result when MCP disabled, got: {body}"
            );
        }
        // Non-JSON (HTML) response is fine — means the SPA fallback was served.
    } else {
        // Any non-200 status is acceptable (404 or similar) — MCP not available.
        assert!(
            status.as_u16() >= 400,
            "unexpected success status {status} when MCP disabled"
        );
    }

    Ok(())
}

/// Test 3: execute_task creates a job; get_job_status returns the job.
#[tokio::test]
async fn test_mcp_execute_and_check_status() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_mcp().await?;

    let (router, session_id) = mcp_initialize(router).await;

    // execute_task
    let exec_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "execute_task",
            "arguments": {
                "workspace": "default",
                "task_name": "hello-world",
                "input": {"name": "test"}
            }
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), exec_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let exec_resp = body_json(response).await;

    // The content is a text block containing JSON
    let content_text = exec_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let exec_result: Value = serde_json::from_str(content_text)?;
    let job_id = exec_result["job_id"]
        .as_str()
        .expect("job_id should be present");

    // get_job_status
    let status_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 3,
        "params": {
            "name": "get_job_status",
            "arguments": {"job_id": job_id}
        }
    });
    let response = router
        .oneshot(mcp_request(session_id.as_deref(), status_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let status_resp = body_json(response).await;

    let status_text = status_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let job_status: Value = serde_json::from_str(status_text)?;

    assert_eq!(job_status["job_id"].as_str().unwrap(), job_id);
    // No worker running — job status is "pending"
    assert_eq!(job_status["status"].as_str().unwrap(), "pending");
    // Steps should be present
    let steps = job_status["steps"]
        .as_array()
        .expect("steps should be array");
    assert!(!steps.is_empty(), "job should have at least one step");
    assert!(
        steps[0].get("skip_reason").is_some(),
        "skip_reason must be present on every step (null while pending): {steps:?}"
    );

    Ok(())
}

/// Test 3b (regression): execute_task with `input` sent as a JSON-encoded
/// STRING (as some MCP clients do) must be parsed back into an object, not
/// silently dropped in favour of schema defaults.
///
/// Reproduces the production bug where a job was created with defaults-only
/// input because the client double-encoded `input` as a string and
/// `merge_defaults` treated the non-object value as empty.
#[tokio::test]
async fn test_mcp_execute_with_stringified_input() -> Result<()> {
    let (router, pool, _tmp) = setup_with_mcp().await?;

    let (router, session_id) = mcp_initialize(router).await;

    // `input` is a JSON-encoded STRING, not an object.
    let exec_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "execute_task",
            "arguments": {
                "workspace": "default",
                "task_name": "hello-world",
                "input": "{\"name\": \"stringy\"}"
            }
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), exec_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let exec_resp = body_json(response).await;
    let content_text = exec_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let exec_result: Value = serde_json::from_str(content_text)?;
    let job_id = exec_result["job_id"]
        .as_str()
        .expect("job_id should be present");
    let job_uuid: Uuid = job_id.parse()?;

    // The stored, resolved input must contain the user-provided `name`, proving
    // the stringified input was parsed rather than dropped.
    let stored_input: Value = sqlx::query_scalar("SELECT input FROM job WHERE job_id = $1")
        .bind(job_uuid)
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        stored_input["name"].as_str(),
        Some("stringy"),
        "stringified input should be parsed into the job input, got: {stored_input}"
    );

    Ok(())
}

/// Test 3c (regression): execute_task with a non-object `input` (e.g. a JSON
/// array) is rejected with an error rather than silently running with defaults.
#[tokio::test]
async fn test_mcp_execute_with_non_object_input_errors() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_mcp().await?;

    let (router, session_id) = mcp_initialize(router).await;

    let exec_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "execute_task",
            "arguments": {
                "workspace": "default",
                "task_name": "hello-world",
                "input": [1, 2, 3]
            }
        }
    });
    let response = router
        .oneshot(mcp_request(session_id.as_deref(), exec_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let resp = body_json(response).await;
    // JSON-RPC error surfaces in the `error` field (not a tool result).
    assert!(
        resp.get("error").is_some(),
        "non-object input must produce a JSON-RPC error, got: {resp}"
    );

    Ok(())
}

/// A `file` input value that is not the caller's own upload is a value-free
/// invalid-params error (spec 2026-10-29-file-inputs § 8), not an internal one.
#[tokio::test]
async fn mcp_execute_with_unknown_file_id_is_invalid_params() -> Result<()> {
    let ws: WorkspaceConfig = serde_yaml::from_str(
        r#"
actions:
  noop: { type: script, script: "true" }
tasks:
  files:
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
"#,
    )?;
    let (router, pool, _tmp) =
        setup_with_mcp_using(WorkspaceManager::from_config("default", ws)).await?;
    let (router, session_id) = mcp_initialize(router).await;

    let unknown = stroem_common::file_field::new_file_id();
    let exec_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "execute_task",
            "arguments": {
                "workspace": "default",
                "task_name": "files",
                "input": {"data_file": unknown}
            }
        }
    });
    let response = router
        .oneshot(mcp_request(session_id.as_deref(), exec_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let resp = body_json(response).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{resp}");
    assert_eq!(
        resp["error"]["message"],
        json!("Input field 'data_file': the file cannot be used"),
        "{resp}"
    );
    let created: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE task_name = 'files'")
        .fetch_one(&pool)
        .await?;
    assert_eq!(created, 0);
    Ok(())
}

/// list_workspaces carries the server-config display_name / description,
/// and omits both keys for a workspace that sets neither.
#[tokio::test]
async fn test_mcp_list_workspaces_reports_display_name_and_description() -> Result<()> {
    let mgr = WorkspaceManager::from_configs(vec![
        ("default".to_string(), mcp_test_workspace(), None),
        ("plain".to_string(), mcp_test_workspace(), None),
    ])
    .with_metadata(
        "default",
        WorkspaceMetadata {
            display_name: Some("Default jobs".to_string()),
            description: Some("Everything that runs by default".to_string()),
        },
    );
    let (router, _pool, _tmp) = setup_with_mcp_using(mgr).await?;
    let (router, session_id) = mcp_initialize(router).await;

    let body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "list_workspaces",
            "arguments": {}
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let resp = body_json(response).await;
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let workspaces: Value = serde_json::from_str(text)?;
    let arr = workspaces.as_array().expect("should be array");
    assert_eq!(arr.len(), 2);

    assert_eq!(arr[0]["name"], "default");
    assert_eq!(arr[0]["display_name"], "Default jobs");
    assert_eq!(arr[0]["description"], "Everything that runs by default");
    assert_eq!(arr[0]["task_count"], 2);

    assert_eq!(arr[1]["name"], "plain");
    assert!(arr[1].get("display_name").is_none(), "got: {}", arr[1]);
    assert!(arr[1].get("description").is_none(), "got: {}", arr[1]);

    Ok(())
}

/// Test 4: list_tasks and list_workspaces return expected results.
#[tokio::test]
async fn test_mcp_list_tasks_and_workspaces() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_mcp().await?;

    let (router, session_id) = mcp_initialize(router).await;

    // list_workspaces
    let ws_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "list_workspaces",
            "arguments": {}
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), ws_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let ws_resp = body_json(response).await;
    let ws_text = ws_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let workspaces: Value = serde_json::from_str(ws_text)?;
    let ws_arr = workspaces.as_array().expect("should be array");
    assert_eq!(ws_arr.len(), 1, "expected exactly 1 workspace");
    assert_eq!(ws_arr[0]["name"].as_str().unwrap(), "default");
    // mcp_test_workspace() has 2 tasks: hello-world + with-hook
    assert_eq!(ws_arr[0]["task_count"].as_u64().unwrap(), 2);

    // list_tasks (all workspaces)
    let tasks_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 3,
        "params": {
            "name": "list_tasks",
            "arguments": {}
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), tasks_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let tasks_resp = body_json(response).await;
    let tasks_text = tasks_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let tasks: Value = serde_json::from_str(tasks_text)?;
    let tasks_arr = tasks.as_array().expect("should be array");
    assert_eq!(tasks_arr.len(), 2, "expected 2 tasks from test workspace");

    // list_tasks filtered by workspace
    let tasks_ws_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 4,
        "params": {
            "name": "list_tasks",
            "arguments": {"workspace": "default"}
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), tasks_ws_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let filtered_resp = body_json(response).await;
    let filtered_text = filtered_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let filtered_tasks: Value = serde_json::from_str(filtered_text)?;
    assert_eq!(
        filtered_tasks.as_array().unwrap().len(),
        2,
        "filtered by workspace should still return 2 tasks"
    );

    // list_tasks with nonexistent workspace → error
    let bad_ws_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 5,
        "params": {
            "name": "list_tasks",
            "arguments": {"workspace": "nonexistent"}
        }
    });
    let response = router
        .oneshot(mcp_request(session_id.as_deref(), bad_ws_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let err_resp = body_json(response).await;
    // The tool may signal an error either as a JSON-RPC error envelope (`error` key)
    // or as a tool-level error (`result.isError = true`).
    let has_error =
        err_resp.get("error").is_some() || err_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        has_error,
        "expected error response for nonexistent workspace, got: {err_resp}"
    );

    Ok(())
}

/// Test 5: parameter validation returns errors for invalid inputs.
#[tokio::test]
async fn test_mcp_parameter_validation() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_mcp().await?;

    let (router, session_id) = mcp_initialize(router).await;

    // get_job_status with invalid UUID
    let bad_uuid_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "get_job_status",
            "arguments": {"job_id": "not-a-uuid"}
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), bad_uuid_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let err_resp = body_json(response).await;
    // Should be an error response (either JSON-RPC error or tool isError)
    let has_error =
        err_resp.get("error").is_some() || err_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        has_error,
        "expected error for invalid UUID, got: {err_resp}"
    );

    // list_jobs with invalid status
    let bad_status_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 3,
        "params": {
            "name": "list_jobs",
            "arguments": {"status": "invalid-status"}
        }
    });
    let response = router
        .clone()
        .oneshot(mcp_request(session_id.as_deref(), bad_status_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let err_resp = body_json(response).await;
    let has_error =
        err_resp.get("error").is_some() || err_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        has_error,
        "expected error for invalid status, got: {err_resp}"
    );

    // cancel_job with invalid UUID
    let bad_cancel_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 4,
        "params": {
            "name": "cancel_job",
            "arguments": {"job_id": "bad-uuid-here"}
        }
    });
    let response = router
        .oneshot(mcp_request(session_id.as_deref(), bad_cancel_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let err_resp = body_json(response).await;
    let has_error =
        err_resp.get("error").is_some() || err_resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        has_error,
        "expected error for invalid cancel UUID, got: {err_resp}"
    );

    Ok(())
}

/// Test 6: auth required — POST /mcp without token returns 401 when auth is enabled.
#[tokio::test]
async fn test_mcp_auth_required_when_enabled() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_auth_and_mcp().await?;

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
    // No Authorization header
    let response = router.oneshot(mcp_request(None, init_body)).await?;

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "expected 401 when auth enabled and no token provided"
    );

    Ok(())
}

/// Test 7a: a login-flow JWT (no `aud`) MUST be rejected at /mcp. The
/// only paths to MCP are the OAuth consent flow (which mints an
/// audience-bound JWT) or an `strm_*` API key. Without this gate, any
/// browser session would silently grant MCP access without the user
/// ever seeing the consent screen.
#[tokio::test]
async fn test_mcp_login_jwt_rejected() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_auth_and_mcp().await?;

    let login_req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({"email": AUTH_USER_EMAIL, "password": AUTH_USER_PASSWORD}).to_string(),
        ))
        .unwrap();
    let login_resp = router.clone().oneshot(login_req).await?;
    assert_eq!(login_resp.status(), StatusCode::OK);
    let login_data = body_json(login_resp).await;
    let access_token = login_data["access_token"].as_str().unwrap();

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
        .oneshot(mcp_request_with_auth(None, access_token, init_body))
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "login JWT (no aud) must be rejected — only OAuth tokens or API keys reach /mcp"
    );
    Ok(())
}

/// Test 7b: API keys continue to work at /mcp as a non-interactive
/// out-of-band credential (CI/CD pipelines, scripts, etc.). The API-key
/// path doesn't go through JWT validation, so the audience policy
/// doesn't apply to it.
#[tokio::test]
async fn test_mcp_api_key_accepted() -> Result<()> {
    let (router, pool, _tmp) = setup_with_auth_and_mcp().await?;

    let user = stroem_db::UserRepo::get_by_email(&pool, AUTH_USER_EMAIL)
        .await?
        .expect("seeded user exists");
    let (raw_key, key_hash) = stroem_server::auth::generate_api_key();
    let prefix = raw_key[..12].to_string();
    stroem_db::ApiKeyRepo::create(&pool, &key_hash, user.user_id, "mcp-test", &prefix, None)
        .await?;

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
        .oneshot(mcp_request_with_auth(None, &raw_key, init_body))
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "API keys must continue to work at /mcp"
    );
    let body = body_json(response).await;
    assert!(body.get("result").is_some(), "expected JSON-RPC result");
    Ok(())
}

/// Pinned issuer for the ACL/OAuth-derivation test, so a forged access token
/// can carry the exact `aud` the MCP auth layer expects.
const ACL_BASE_URL: &str = "http://localhost";

/// Forge an OAuth-style MCP access token: audience-bound to `/mcp` and
/// deliberately unprivileged (`is_admin: false`), exactly like the tokens
/// `/oauth/token` mints. The MCP layer must derive real admin from the DB.
fn forge_mcp_access_token(user_id: Uuid) -> String {
    use jsonwebtoken::{encode, EncodingKey, Header};
    use stroem_common::models::auth::Claims;
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        email: AUTH_USER_EMAIL.to_string(),
        is_admin: false,
        iat: now,
        exp: now + 3600,
        aud: Some(format!("{ACL_BASE_URL}/mcp")),
        iss: Some(ACL_BASE_URL.to_string()),
        scope: Some("mcp".to_string()),
        client_id: None,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(AUTH_JWT_SECRET.as_bytes()),
    )
    .expect("failed to forge token")
}

/// Call `list_tasks` with the given bearer token and return the number of
/// tasks visible to that identity.
async fn list_tasks_count(router: &Router, token: &str) -> usize {
    let body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 1,
        "params": { "name": "list_tasks", "arguments": {} }
    });
    let resp = router
        .clone()
        .oneshot(mcp_request_with_auth(None, token, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let arr: Value = serde_json::from_str(text).unwrap();
    arr.as_array().expect("tasks should be an array").len()
}

/// Option B: MCP derives `is_admin` from the DB per request, not from the
/// (unprivileged) OAuth token. An admin bypasses ACL over MCP exactly as in the
/// UI, and revoking admin takes effect immediately on a still-live token —
/// closing the token-TTL revocation gap that baking `is_admin` into the JWT
/// would open. The workspace has 2 tasks; the ACL defaults to deny with no
/// rules, so a non-admin sees none.
#[tokio::test]
async fn test_mcp_admin_derived_from_db_not_token() -> Result<()> {
    let acl = AclConfig {
        default: AclAction::Deny,
        rules: vec![],
    };
    let (router, pool, _tmp) =
        setup_with_auth_and_mcp_cfg(Some(acl), Some(ACL_BASE_URL.to_string())).await?;

    let user = UserRepo::get_by_email(&pool, AUTH_USER_EMAIL)
        .await?
        .expect("seeded user exists");
    // A real OAuth token is unprivileged; admin must come from the DB, not here.
    let token = forge_mcp_access_token(user.user_id);

    // Baseline: non-admin under default-deny ACL sees no tasks.
    assert_eq!(
        list_tasks_count(&router, &token).await,
        0,
        "non-admin under default-deny ACL must see no tasks"
    );

    // Promote to admin in the DB → the SAME unprivileged token now bypasses ACL.
    UserRepo::set_admin(&pool, user.user_id, true).await?;
    assert_eq!(
        list_tasks_count(&router, &token).await,
        2,
        "admin (per DB) must bypass ACL over MCP even though the token is unprivileged"
    );

    // Revoke admin → the same live token is denied immediately (no TTL lag).
    UserRepo::set_admin(&pool, user.user_id, false).await?;
    assert_eq!(
        list_tasks_count(&router, &token).await,
        0,
        "revoking admin must take effect immediately on a live MCP token"
    );
    Ok(())
}

// ─── OAuth 2.1 metadata + WWW-Authenticate tests ────────────────────────────

/// Helper: GET a JSON document with an explicit Host header.
fn get_json_request(uri: &str, host: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("Host", host)
        .header("Accept", "application/json")
        .body(Body::empty())
        .unwrap()
}

/// 401 from /mcp must carry a `WWW-Authenticate: Bearer ...` header whose
/// `resource_metadata` parameter points at the discovery doc. Spec-conformant
/// MCP clients use this to bootstrap the OAuth flow without any manual config.
#[tokio::test]
async fn test_mcp_401_includes_www_authenticate_with_resource_metadata() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_auth_and_mcp().await?;

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
    let response = router.oneshot(mcp_request(None, init_body)).await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let www_auth = response
        .headers()
        .get(http::header::WWW_AUTHENTICATE)
        .expect("WWW-Authenticate header MUST be present on 401 per MCP spec")
        .to_str()?;

    assert!(
        www_auth.starts_with("Bearer "),
        "WWW-Authenticate must use Bearer scheme, got: {www_auth}"
    );
    assert!(
        www_auth.contains(r#"resource_metadata=""#),
        "WWW-Authenticate must advertise resource_metadata URL, got: {www_auth}"
    );
    assert!(
        www_auth.contains("/.well-known/oauth-protected-resource"),
        "resource_metadata must point at the well-known doc, got: {www_auth}"
    );

    Ok(())
}

/// Discovery doc per RFC 9728: lists the resource URL, authorization servers,
/// and supported scopes/bearer methods. Claude Desktop, Cursor, and MCP
/// Inspector all read this before initiating any auth dance.
#[tokio::test]
async fn test_protected_resource_metadata_endpoint() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_auth_and_mcp().await?;

    let response = router
        .oneshot(get_json_request(
            "/.well-known/oauth-protected-resource",
            "stroem.example.com",
        ))
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    let resource = body["resource"]
        .as_str()
        .expect("resource field is required");
    assert!(
        resource.ends_with("/mcp"),
        "resource URL must point at the /mcp endpoint, got: {resource}"
    );

    let servers = body["authorization_servers"]
        .as_array()
        .expect("authorization_servers must be present");
    assert_eq!(
        servers.len(),
        1,
        "exactly one AS advertised (Strøm self-hosts), got: {servers:?}"
    );

    assert_eq!(
        body["scopes_supported"],
        json!(["mcp"]),
        "single coarse `mcp` scope as designed"
    );
    assert_eq!(
        body["bearer_methods_supported"],
        json!(["header"]),
        "only Authorization header method supported"
    );

    Ok(())
}

/// RFC 8414 AS metadata. Crucially, `code_challenge_methods_supported` must
/// include `S256` — OAuth 2.1 doesn't allow `plain`, and clients use this
/// list to decide which PKCE method to send.
#[tokio::test]
async fn test_authorization_server_metadata_endpoint() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_auth_and_mcp().await?;

    let response = router
        .oneshot(get_json_request(
            "/.well-known/oauth-authorization-server",
            "stroem.example.com",
        ))
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    assert!(body["issuer"].is_string(), "issuer must be present");
    assert!(
        body["authorization_endpoint"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/oauth/authorize"),
        "authorization_endpoint must end with /oauth/authorize, got: {}",
        body["authorization_endpoint"]
    );
    assert!(
        body["token_endpoint"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/oauth/token"),
        "token_endpoint must end with /oauth/token"
    );
    assert!(
        body["registration_endpoint"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/oauth/register"),
        "registration_endpoint must end with /oauth/register"
    );

    assert_eq!(body["response_types_supported"], json!(["code"]));
    assert_eq!(
        body["grant_types_supported"],
        json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(
        body["code_challenge_methods_supported"],
        json!(["S256"]),
        "OAuth 2.1 mandates S256 (plain is forbidden)"
    );

    let auth_methods = body["token_endpoint_auth_methods_supported"]
        .as_array()
        .expect("token_endpoint_auth_methods_supported is required");
    let methods: Vec<&str> = auth_methods.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        methods.contains(&"none"),
        "public clients (Claude Desktop, Inspector) need `none`"
    );

    Ok(())
}

/// Without auth configured, MCP is open and OAuth metadata isn't meaningful —
/// the routes shouldn't be mounted at all. Verifying 404 prevents accidental
/// metadata leakage from open dev servers.
#[tokio::test]
async fn test_oauth_metadata_not_mounted_without_auth() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_mcp().await?;

    let response = router
        .clone()
        .oneshot(get_json_request(
            "/.well-known/oauth-protected-resource",
            "localhost",
        ))
        .await?;
    // SPA fallback (StaticFiles) returns 200 with index.html when present, or
    // 404 when no static files are embedded — either way the OAuth route is
    // not handling this. We assert it's not the JSON discovery doc.
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        status == StatusCode::NOT_FOUND || !body_str.starts_with("{\"resource\""),
        "OAuth metadata must not be served when auth is disabled (status={status}, body={body_str})"
    );

    let response = router
        .oneshot(get_json_request(
            "/.well-known/oauth-authorization-server",
            "localhost",
        ))
        .await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        status == StatusCode::NOT_FOUND || !body_str.starts_with("{\"issuer\""),
        "AS metadata must not be served when auth is disabled (status={status}, body={body_str})"
    );

    Ok(())
}

/// Sanity-check the Phase-2 endpoints exist and reject malformed inputs.
/// Detailed behaviour is covered by `oauth_flow_test.rs`.
#[tokio::test]
async fn test_oauth_endpoints_are_mounted() -> Result<()> {
    let (router, _pool, _tmp) = setup_with_auth_and_mcp().await?;

    // /oauth/authorize without required query params must 400 (axum Query
    // extractor rejects), not 404.
    let authorize_req = Request::builder()
        .method("GET")
        .uri("/oauth/authorize")
        .header("Host", "localhost")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(authorize_req).await?;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "/oauth/authorize must be mounted and reject empty params"
    );

    // /oauth/token with an unknown grant_type must return the
    // OAuth-standard `unsupported_grant_type` error.
    let token_req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header("Host", "localhost")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from("grant_type=password"))
        .unwrap();
    let response = router.clone().oneshot(token_req).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"], "unsupported_grant_type");

    // /oauth/register: Phase 3 made this real. An empty body fails JSON
    // deserialization → 422; an empty client_name fails validation → 400
    // `invalid_client_metadata`. Either way, NOT 501. Detailed behaviour
    // is covered by oauth_flow_test::test_dcr_*.
    let register_req = Request::builder()
        .method("POST")
        .uri("/oauth/register")
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = router.oneshot(register_req).await?;
    assert!(
        response.status() == StatusCode::BAD_REQUEST
            || response.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "/oauth/register must be mounted (Phase 3) — got {}",
        response.status()
    );

    Ok(())
}

/// Test 8: get_job_status includes the revision field when the job was created with one.
#[tokio::test]
async fn test_mcp_get_job_status_includes_revision() -> Result<()> {
    let (router, pool, _tmp) = setup_with_mcp().await?;

    // Create a job directly with a known revision via JobRepo so we can control the
    // exact value without depending on WorkspaceManager::get_revision().
    let workspace = mcp_test_workspace();
    let revision = "mcp-test-rev-abc123";
    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "hello-world",
        json!({"name": "revision-test"}),
        "api",
        None,
        Some(revision),
        None,
        None, // source_job_id
        JobDefaults::default(),
    )
    .await?;

    let (router, session_id) = mcp_initialize(router).await;

    let status_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "get_job_status",
            "arguments": {"job_id": job_id.to_string()}
        }
    });
    let response = router
        .oneshot(mcp_request(session_id.as_deref(), status_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let resp = body_json(response).await;

    let content_text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let job_status: Value = serde_json::from_str(content_text)?;

    assert_eq!(job_status["job_id"].as_str().unwrap(), job_id.to_string());
    assert_eq!(
        job_status["revision"].as_str(),
        Some(revision),
        "get_job_status must include the revision stored on the job"
    );

    Ok(())
}

/// Test 9: list_jobs includes the revision field for each job.
#[tokio::test]
async fn test_mcp_list_jobs_includes_revision() -> Result<()> {
    let (router, pool, _tmp) = setup_with_mcp().await?;

    let workspace = mcp_test_workspace();
    let revision = "list-jobs-rev-xyz789";
    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "hello-world",
        json!({"name": "list-rev-test"}),
        "api",
        None,
        Some(revision),
        None,
        None, // source_job_id
        JobDefaults::default(),
    )
    .await?;

    let (router, session_id) = mcp_initialize(router).await;

    let list_body = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "list_jobs",
            "arguments": {"workspace": "default"}
        }
    });
    let response = router
        .oneshot(mcp_request(session_id.as_deref(), list_body))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let resp = body_json(response).await;

    let content_text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a string");
    let list_result: Value = serde_json::from_str(content_text)?;

    let jobs = list_result["jobs"]
        .as_array()
        .expect("list_jobs should return a 'jobs' array");

    let matching = jobs
        .iter()
        .find(|j| j["job_id"].as_str() == Some(&job_id.to_string()))
        .expect("created job must appear in list_jobs response");

    assert_eq!(
        matching["revision"].as_str(),
        Some(revision),
        "list_jobs must include the revision stored on the job"
    );

    Ok(())
}

/// Test 10: jobs created via MCP have source_type "mcp"; on_success hooks fire correctly.
#[tokio::test]
async fn test_mcp_created_jobs_fire_hooks() -> Result<()> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool.clone();
    let url = test_db.url;

    let workspace = mcp_test_workspace();

    // Create a job via MCP source (simulate what the MCP execute_task tool does)
    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "with-hook",
        json!({}),
        "mcp",
        None,
        None,
        None,
        None, // source_job_id
        JobDefaults::default(),
    )
    .await?;

    // Verify the job was created with source_type = "mcp"
    let job = JobRepo::get(&pool, job_id)
        .await?
        .expect("job should exist");
    assert_eq!(
        job.source_type, "mcp",
        "job created via MCP should have source_type 'mcp'"
    );
    assert_eq!(job.task_name, "with-hook");

    // Verify steps were created
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps.len(), 1, "with-hook has 1 step");
    assert_eq!(steps[0].status, "ready");

    // Simulate step execution: register worker, mark running, mark completed
    let worker_id = register_test_worker(&pool).await;
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step1", Some(json!({"result": "ok"}))).await?;

    // Run the cascade to complete the job
    let task = workspace.tasks.get("with-hook").unwrap();
    after_step(&pool, job_id, task).await?;

    // Verify job is completed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    // Build state for firing hooks
    let temp_dir = tempfile::TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-test-token-secret-long-enough-32c".to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: Some(McpConfig {
            enabled: true,
            ..Default::default()
        }),
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        input_files: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        leader: Default::default(),
        pin_store: None,
    };
    let mgr = WorkspaceManager::from_config("default", workspace.clone());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);

    // Fire hooks
    stroem_server::settlement::hooks::fire_hooks(&state.settlement(), &workspace, &job, task).await;

    // Verify hook job was created
    let all_jobs = JobRepo::list(&pool, Some("default"), None, None, None, 100, 0).await?;
    assert_eq!(all_jobs.len(), 2, "expected original job + hook job");

    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("hook job should have been created");
    assert_eq!(hook_job.task_name, "_hook:notify");

    // Verify hook step was created and is ready
    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    assert_eq!(hook_steps.len(), 1);
    assert_eq!(hook_steps[0].step_name, "hook");
    assert_eq!(hook_steps[0].action_name, "notify");
    assert_eq!(hook_steps[0].status, "ready");

    Ok(())
}

#[tokio::test]
async fn test_mcp_get_job_logs_tail_bytes_and_truncation_trailer() -> Result<()> {
    let (router, pool, tmp) = setup_with_mcp().await?;
    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        None,
        "api",
        None,
        None,
        None,
    )
    .await?;
    let content: String = (0..2_000)
        .map(|i| format!(r#"{{"ts":"2026-09-22T00:00:00Z","stream":"stdout","step":"build","line":"line {i:05}"}}"#) + "\n")
        .collect();
    std::fs::write(
        tmp.path().join("logs").join(format!("{job_id}.jsonl")),
        &content,
    )?;
    let (router, session_id) = mcp_initialize(router).await;
    let call = |id: u64, args: Value| {
        json!({"jsonrpc": "2.0", "method": "tools/call", "id": id,
               "params": {"name": "get_job_logs", "arguments": args}})
    };

    let resp = body_json(
        router
            .clone()
            .oneshot(mcp_request(
                session_id.as_deref(),
                call(2, json!({"job_id": job_id.to_string()})),
            ))
            .await?,
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("line 01999") && text.contains("line 00000"));
    assert!(
        !text.contains("[truncated:"),
        "under the default tail there is no trailer"
    );

    let resp = body_json(
        router
            .clone()
            .oneshot(mcp_request(
                session_id.as_deref(),
                call(3, json!({"job_id": job_id.to_string(), "tail_bytes": 1000})),
            ))
            .await?,
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("line 01999") && !text.contains("line 00000"));
    assert!(
        text.trim_end().ends_with("earlier lines omitted]"),
        "{text}"
    );

    let resp = body_json(
        router
            .oneshot(mcp_request(
                session_id.as_deref(),
                call(4, json!({"job_id": job_id.to_string(), "tail_bytes": 0})),
            ))
            .await?,
    )
    .await;
    assert!(
        resp.get("error").is_some(),
        "tail_bytes 0 is invalid params: {resp}"
    );
    Ok(())
}

// ─── File inputs (spec 2026-10-29-file-inputs § 7.5, § 8) ───────────────────

const FILES_YAML: &str = r#"
actions:
  noop: { type: script, script: "true" }
tasks:
  files:
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
  plain:
    flow: { run: { action: noop } }
  view-files:
    folder: view
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
"#;

const FILES_BASE_URL: &str = "http://stroem.test";

async fn files_app() -> Result<(Router, PgPool, TempDir, String)> {
    files_app_with(true).await
}

/// `files_app`; `with_blob: false` = no input-file archive configured.
async fn files_app_with(with_blob: bool) -> Result<(Router, PgPool, TempDir, String)> {
    files_app_acl(
        with_blob,
        AclConfig {
            default: AclAction::Run,
            rules: vec![AclRule {
                workspace: "default".to_string(),
                tasks: vec!["view/*".to_string()],
                action: AclAction::View,
                groups: vec![],
                users: vec![AUTH_USER_EMAIL.to_string()],
            }],
        },
    )
    .await
}

async fn files_app_acl(
    with_blob: bool,
    acl: AclConfig,
) -> Result<(Router, PgPool, TempDir, String)> {
    let ws: WorkspaceConfig = serde_yaml::from_str(FILES_YAML)?;
    let (router, pool, tmp) =
        setup_with_auth_and_mcp_ws_blob(ws, Some(acl), Some(FILES_BASE_URL.to_string()), with_blob)
            .await?;
    let user = UserRepo::get_by_email(&pool, AUTH_USER_EMAIL)
        .await?
        .expect("seeded user exists");
    let (raw_key, key_hash) = stroem_server::auth::generate_api_key();
    stroem_db::ApiKeyRepo::create(
        &pool,
        &key_hash,
        user.user_id,
        "files",
        &raw_key[..12],
        None,
    )
    .await?;
    Ok((router, pool, tmp, raw_key))
}

/// Initialize a session and call one tool with an API key; returns the JSON-RPC body.
async fn call_tool_authed(router: &Router, key: &str, name: &str, args: Value) -> Value {
    let init = json!({
        "jsonrpc": "2.0", "method": "initialize", "id": 0,
        "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "1.0"}}
    });
    let resp = router
        .clone()
        .oneshot(mcp_request_with_auth(None, key, init))
        .await
        .unwrap();
    let sid = resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = json!({
        "jsonrpc": "2.0", "method": "tools/call", "id": 2,
        "params": {"name": name, "arguments": args}
    });
    let resp = router
        .clone()
        .oneshot(mcp_request_with_auth(sid.as_deref(), key, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// The tool result's JSON text payload.
fn tool_payload(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no tool text: {resp}"));
    serde_json::from_str(text).unwrap()
}

async fn file_origin_and_attachment(pool: &PgPool, job_id: &str) -> Result<(String, i64)> {
    let origin: String = sqlx::query_scalar(
        "SELECT f.origin FROM input_file f \
         JOIN job_input_file j ON j.file_id = f.file_id WHERE j.job_id = $1::uuid",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await?;
    let size: i64 = sqlx::query_scalar(
        "SELECT f.size_bytes FROM input_file f \
         JOIN job_input_file j ON j.file_id = f.file_id WHERE j.job_id = $1::uuid",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await?;
    Ok((origin, size))
}

#[tokio::test]
async fn mcp_execute_inline_text_file() -> Result<()> {
    let (router, pool, _tmp, key) = files_app().await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "execute_task",
        json!({"workspace": "default", "task_name": "files",
               "input": {"data_file": {"name": "a.txt", "content": "hello"}}}),
    )
    .await;
    let job_id = tool_payload(&resp)["job_id"].as_str().unwrap().to_string();
    let (origin, size) = file_origin_and_attachment(&pool, &job_id).await?;
    assert_eq!(origin, "mcp_inline");
    assert_eq!(size, 5);
    Ok(())
}

#[tokio::test]
async fn mcp_execute_inline_base64_file() -> Result<()> {
    let (router, pool, _tmp, key) = files_app().await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "execute_task",
        json!({"workspace": "default", "task_name": "files",
               "input": {"data_file": {"name": "b.bin", "content_base64": "AAEC/w=="}}}),
    )
    .await;
    let job_id = tool_payload(&resp)["job_id"].as_str().unwrap().to_string();
    let (origin, size) = file_origin_and_attachment(&pool, &job_id).await?;
    assert_eq!(origin, "mcp_inline");
    assert_eq!(size, 4);
    Ok(())
}

#[tokio::test]
async fn mcp_execute_inline_over_cap_names_field_and_cap() -> Result<()> {
    let (router, pool, _tmp, key) = files_app().await?;
    let big = "x".repeat(1024 * 1024 + 1);
    let resp = call_tool_authed(
        &router,
        &key,
        "execute_task",
        json!({"workspace": "default", "task_name": "files",
               "input": {"data_file": {"name": "a.txt", "content": big}}}),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{resp}");
    assert_eq!(
        resp["error"]["message"],
        json!("Input field 'data_file': inline content is larger than 1048576 bytes")
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM input_file")
        .fetch_one(&pool)
        .await?;
    assert_eq!(n, 0);
    Ok(())
}

#[tokio::test]
async fn mcp_execute_inline_bad_base64_is_invalid_params() -> Result<()> {
    let (router, _pool, _tmp, key) = files_app().await?;
    for value in [
        json!({"name": "a.bin", "content_base64": "!!not base64!!"}),
        json!({"name": "a.bin"}),
        json!({"name": "a.bin", "content": "x", "content_base64": "eA=="}),
        json!({"name": "a.bin", "content": "x", "extra": 1}),
    ] {
        let resp = call_tool_authed(
            &router,
            &key,
            "execute_task",
            json!({"workspace": "default", "task_name": "files",
                   "input": {"data_file": value}}),
        )
        .await;
        assert_eq!(resp["error"]["code"], json!(-32602), "{resp}");
        assert_eq!(
            resp["error"]["message"],
            json!("Input field 'data_file': the file cannot be used")
        );
    }
    Ok(())
}

fn upload_url_request(url: &str, body: &'static str) -> Request<Body> {
    let path = url.strip_prefix(FILES_BASE_URL).expect("base url prefix");
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "text/csv")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn mcp_create_upload_url_and_upload_once() -> Result<()> {
    let (router, pool, _tmp, key) = files_app().await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "create_upload_url",
        json!({"workspace": "default", "task_name": "files", "file_name": "data.csv"}),
    )
    .await;
    let out = tool_payload(&resp);
    assert_eq!(out["method"], json!("POST"));
    assert_eq!(out["max_bytes"], json!(104_857_600u64));
    assert!(out["expires_at"].as_str().is_some());
    let url = out["upload_url"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://stroem.test/api/uploads/"), "{url}");

    let r = router
        .clone()
        .oneshot(upload_url_request(&url, "a,b\n1,2\n"))
        .await?;
    assert_eq!(r.status(), StatusCode::CREATED);
    let uploaded = body_json(r).await;
    assert_eq!(uploaded["name"], json!("data.csv"));
    assert_eq!(uploaded["size_bytes"], json!(8));
    let file_id = uploaded["file_id"].as_str().unwrap().to_string();

    let r = router
        .clone()
        .oneshot(upload_url_request(&url, "again"))
        .await?;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(r).await["error"], json!("Upload URL not found"));

    let resp = call_tool_authed(
        &router,
        &key,
        "execute_task",
        json!({"workspace": "default", "task_name": "files",
               "input": {"data_file": file_id}}),
    )
    .await;
    let job_id = tool_payload(&resp)["job_id"].as_str().unwrap().to_string();
    let (origin, _) = file_origin_and_attachment(&pool, &job_id).await?;
    assert_eq!(origin, "mcp_url");
    Ok(())
}

async fn input_file_count(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM input_file")
        .fetch_one(pool)
        .await?)
}

#[tokio::test]
async fn mcp_upload_url_refused_when_run_permission_was_removed() -> Result<()> {
    // Run comes from the "ops" group only; default is deny.
    let (router, pool, _tmp, key) = files_app_acl(
        true,
        AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "default".to_string(),
                tasks: vec!["*".to_string()],
                action: AclAction::Run,
                groups: vec!["ops".to_string()],
                users: vec![],
            }],
        },
    )
    .await?;
    let user = UserRepo::get_by_email(&pool, AUTH_USER_EMAIL)
        .await?
        .expect("seeded user exists");
    stroem_db::UserGroupRepo::add(&pool, user.user_id, "ops").await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "create_upload_url",
        json!({"workspace": "default", "task_name": "files", "file_name": "data.csv"}),
    )
    .await;
    let url = tool_payload(&resp)["upload_url"]
        .as_str()
        .unwrap()
        .to_string();
    stroem_db::UserGroupRepo::remove(&pool, user.user_id, "ops").await?;

    for _ in 0..2 {
        let r = router
            .clone()
            .oneshot(upload_url_request(&url, "a,b\n"))
            .await?;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_json(r).await["error"], json!("Upload URL not found"));
    }
    assert_eq!(input_file_count(&pool).await?, 0);
    Ok(())
}

#[tokio::test]
async fn mcp_upload_url_refused_when_task_has_no_file_field_or_is_gone() -> Result<()> {
    let (router, pool, _tmp, _key) = files_app().await?;
    let user = UserRepo::get_by_email(&pool, AUTH_USER_EMAIL)
        .await?
        .expect("seeded user exists");
    for (n, task) in ["plain", "no-such-task"].into_iter().enumerate() {
        let token = format!("refused-token-{n}");
        stroem_db::UploadTokenRepo::insert(
            &pool,
            &stroem_server::auth::hash_token(&token),
            user.user_id,
            "default",
            task,
            "d.csv",
            chrono::Utc::now() + chrono::Duration::seconds(900),
        )
        .await?;
        let r = router
            .clone()
            .oneshot(upload_url_request(
                &format!("{FILES_BASE_URL}/api/uploads/{token}"),
                "x",
            ))
            .await?;
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{task}");
        assert_eq!(body_json(r).await["error"], json!("Upload URL not found"));
    }
    assert_eq!(input_file_count(&pool).await?, 0);
    Ok(())
}

#[tokio::test]
async fn mcp_upload_url_expired_is_404() -> Result<()> {
    let (router, pool, _tmp, key) = files_app().await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "create_upload_url",
        json!({"workspace": "default", "task_name": "files", "file_name": "data.csv"}),
    )
    .await;
    let url = tool_payload(&resp)["upload_url"]
        .as_str()
        .unwrap()
        .to_string();
    sqlx::query("UPDATE input_file_upload_token SET expires_at = NOW() - INTERVAL '1 second'")
        .execute(&pool)
        .await?;
    let r = router.oneshot(upload_url_request(&url, "x")).await?;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn mcp_create_upload_url_without_base_url_is_tool_error() -> Result<()> {
    let ws: WorkspaceConfig = serde_yaml::from_str(FILES_YAML)?;
    let (router, _pool, _tmp) =
        setup_with_mcp_using(WorkspaceManager::from_config("default", ws)).await?;
    let (router, session_id) = mcp_initialize(router).await;
    let body = json!({
        "jsonrpc": "2.0", "method": "tools/call", "id": 2,
        "params": {"name": "create_upload_url", "arguments": {
            "workspace": "default", "task_name": "files", "file_name": "d.csv"}}
    });
    let resp = body_json(
        router
            .oneshot(mcp_request(session_id.as_deref(), body))
            .await?,
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{resp}");
    assert_eq!(
        resp["error"]["message"],
        json!("upload URLs need auth.base_url; send the file inline or use the REST upload")
    );
    Ok(())
}

#[tokio::test]
async fn mcp_create_upload_url_view_only_is_denied() -> Result<()> {
    let (router, pool, _tmp, key) = files_app().await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "create_upload_url",
        json!({"workspace": "default", "task_name": "view-files", "file_name": "d.csv"}),
    )
    .await;
    assert!(resp.get("error").is_some(), "{resp}");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Insufficient permissions"),
        "{resp}"
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM input_file_upload_token")
        .fetch_one(&pool)
        .await?;
    assert_eq!(n, 0);
    // A task without a file field is invalid params.
    let resp = call_tool_authed(
        &router,
        &key,
        "create_upload_url",
        json!({"workspace": "default", "task_name": "plain", "file_name": "d.csv"}),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{resp}");
    Ok(())
}

/// Without an input-file archive, no upload URL is minted: its first POST
/// would consume the token and answer 503.
#[tokio::test]
async fn mcp_create_upload_url_without_file_storage_is_tool_error() -> Result<()> {
    let (router, pool, _tmp, key) = files_app_with(false).await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "create_upload_url",
        json!({"workspace": "default", "task_name": "files", "file_name": "d.csv"}),
    )
    .await;
    assert!(resp.get("result").is_none(), "{resp}");
    assert_eq!(
        resp["error"]["message"],
        json!("file storage is not configured"),
        "{resp}"
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM input_file_upload_token")
        .fetch_one(&pool)
        .await?;
    assert_eq!(n, 0);
    Ok(())
}

/// Inline content is stored only after the Run check: a View-only task
/// answers the ACL error and stores nothing.
#[tokio::test]
async fn mcp_execute_inline_on_view_only_task_stores_nothing() -> Result<()> {
    let (router, pool, _tmp, key) = files_app().await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "execute_task",
        json!({"workspace": "default", "task_name": "view-files",
               "input": {"data_file": {"name": "a.txt", "content": "hello"}}}),
    )
    .await;
    assert!(
        resp["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Insufficient permissions")),
        "{resp}"
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM input_file")
        .fetch_one(&pool)
        .await?;
    assert_eq!(n, 0);
    Ok(())
}

#[tokio::test]
async fn mcp_get_task_shows_file_type() -> Result<()> {
    let (router, _pool, _tmp, key) = files_app().await?;
    let resp = call_tool_authed(
        &router,
        &key,
        "get_task",
        json!({"workspace": "default", "task_name": "files"}),
    )
    .await;
    assert_eq!(
        tool_payload(&resp)["input"]["data_file"]["type"],
        json!("file")
    );
    Ok(())
}
