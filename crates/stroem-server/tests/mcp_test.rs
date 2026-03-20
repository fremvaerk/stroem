use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{
    ActionDef, FlowStep, HookDef, InputFieldDef, TaskDef, WorkspaceConfig,
};
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, UserRepo, WorkerRepo};
use stroem_server::auth::hash_password;
use stroem_server::config::{
    AuthConfig, DbConfig, InitialUserConfig, LogStorageConfig, McpConfig, ServerConfig,
    WorkspaceSourceDef,
};
use stroem_server::job_creator::create_job_for_task;
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

// ─── Auth test constants ─────────────────────────────────────────────────────

const AUTH_JWT_SECRET: &str = "mcp-test-jwt-secret-key-long-enough";
const AUTH_REFRESH_SECRET: &str = "mcp-test-refresh-secret-key-long-enough";
const AUTH_USER_EMAIL: &str = "mcp-admin@test.com";
const AUTH_USER_PASSWORD: &str = "mcp-test-password-123";

// ─── Minimal workspace for MCP tests ────────────────────────────────────────

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
            order: None,
        },
    );
    workspace.actions.insert(
        "greet".to_string(),
        ActionDef {
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
            output_schema: None,
            temperature: None,
            max_tokens: None,
            tools: vec![],
            max_turns: None,
            message: None,
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
            order: None,
        },
    );
    workspace.actions.insert(
        "notify".to_string(),
        ActionDef {
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
            output_schema: None,
            temperature: None,
            max_tokens: None,
            tools: vec![],
            max_turns: None,
            message: None,
        },
    );

    // Task: hello-world
    let mut hello_flow = HashMap::new();
    let mut hello_input_vals = HashMap::new();
    hello_input_vals.insert("name".to_string(), json!("{{ input.name }}"));
    hello_flow.insert(
        "greet".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: hello_input_vals,
            continue_on_failure: false,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
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
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
        },
    );

    // Task: with-hook (has on_success hook)
    let mut hook_flow = HashMap::new();
    hook_flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
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
            on_success: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
            on_error: vec![],
            on_suspended: vec![],
        },
    );

    workspace
}

// ─── Setup helpers ───────────────────────────────────────────────────────────

async fn setup_with_mcp() -> Result<(
    Router,
    PgPool,
    TempDir,
    testcontainers::ContainerAsync<Postgres>,
)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-test-token-secret-long-enough-32c".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: Some(McpConfig { enabled: true }),
        agents: None,
    };

    let workspace = mcp_test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir, container))
}

async fn setup_mcp_disabled() -> Result<(
    Router,
    PgPool,
    TempDir,
    testcontainers::ContainerAsync<Postgres>,
)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-test-token-secret-long-enough-32c".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None, // MCP disabled
        agents: None,
    };

    let workspace = mcp_test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir, container))
}

async fn setup_with_auth_and_mcp() -> Result<(
    Router,
    PgPool,
    TempDir,
    testcontainers::ContainerAsync<Postgres>,
)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-test-token-secret-long-enough-32c".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: AUTH_JWT_SECRET.to_string(),
            refresh_secret: AUTH_REFRESH_SECRET.to_string(),
            base_url: None,
            providers: HashMap::new(),
            initial_user: Some(InitialUserConfig {
                email: AUTH_USER_EMAIL.to_string(),
                password: AUTH_USER_PASSWORD.to_string(),
            }),
        }),
        recovery: Default::default(),
        acl: None,
        mcp: Some(McpConfig { enabled: true }),
        agents: None,
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

    let workspace = mcp_test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir, container))
}

// ─── Request helpers ─────────────────────────────────────────────────────────

fn mcp_request(session_id: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
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

/// Test 1: tools/list returns all 8 expected tools.
#[tokio::test]
async fn test_mcp_tools_list_returns_all_8_tools() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_mcp().await?;

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
    ];

    assert_eq!(
        tool_names.len(),
        expected.len(),
        "expected 8 tools, got {}: {:?}",
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
    let (router, _pool, _tmp, _container) = setup_mcp_disabled().await?;

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
    let (router, _pool, _tmp, _container) = setup_with_mcp().await?;

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

    Ok(())
}

/// Test 4: list_tasks and list_workspaces return expected results.
#[tokio::test]
async fn test_mcp_list_tasks_and_workspaces() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_mcp().await?;

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
    let (router, _pool, _tmp, _container) = setup_with_mcp().await?;

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
    let (router, _pool, _tmp, _container) = setup_with_auth_and_mcp().await?;

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

/// Test 7: valid JWT token grants access to MCP endpoint.
#[tokio::test]
async fn test_mcp_auth_with_valid_token() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth_and_mcp().await?;

    // Login to get access token
    let login_body = json!({
        "email": AUTH_USER_EMAIL,
        "password": AUTH_USER_PASSWORD
    });
    let login_req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&login_body).unwrap()))
        .unwrap();
    let login_resp = router.clone().oneshot(login_req).await?;
    assert_eq!(login_resp.status(), StatusCode::OK, "login should succeed");
    let login_data = body_json(login_resp).await;
    let access_token = login_data["access_token"]
        .as_str()
        .expect("access_token should be present");

    // Now send initialize with valid token
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
        StatusCode::OK,
        "expected 200 with valid token"
    );
    let body = body_json(response).await;
    // Should have a JSON-RPC result with server info
    assert!(
        body.get("result").is_some(),
        "expected JSON-RPC result, got: {body}"
    );
    assert!(
        body["result"].get("serverInfo").is_some()
            || body["result"].get("server_info").is_some()
            || body["result"].get("capabilities").is_some(),
        "expected server info in result, got: {body}"
    );

    Ok(())
}

/// Test 8: get_job_status includes the revision field when the job was created with one.
#[tokio::test]
async fn test_mcp_get_job_status_includes_revision() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_with_mcp().await?;

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
    let (router, pool, _tmp, _container) = setup_with_mcp().await?;

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
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

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

    // Run orchestrator to complete the job
    let task = workspace.tasks.get("with-hook").unwrap();
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;

    // Verify job is completed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    // Build state for firing hooks
    let temp_dir = tempfile::TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: format!(
                "postgres://postgres:postgres@localhost:{}/postgres",
                container.get_host_port_ipv4(5432).await?
            ),
        },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-test-token-secret-long-enough-32c".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: Some(McpConfig { enabled: true }),
        agents: None,
    };
    let mgr = WorkspaceManager::from_config("default", workspace.clone());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());

    // Fire hooks
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    // Verify hook job was created
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
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
