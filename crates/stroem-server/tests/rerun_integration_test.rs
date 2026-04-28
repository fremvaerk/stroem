//! Integration tests for the Re-run prefill flow.
//!
//! Verifies that:
//! - source_job_id pulls connection names + secret values from source.raw_input
//!   when the UI submits the redaction sentinel `••••••`.
//! - GET /api/jobs/{id} returns raw_input with workspace secret values redacted.
//! - Cross-workspace + unknown source_job_id rejected with 400.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value as JsonValue};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{
    ActionDef, ConnectionDef, FlowStep, InputFieldDef, TaskDef, WorkspaceConfig,
};
use stroem_db::{create_pool, run_migrations};
use stroem_server::config::{
    DbConfig, LogStorageConfig, RetentionConfig, ServerConfig, WorkspaceSourceDef,
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

async fn spawn_pg() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

/// Build a workspace with one task whose inputs cover all three field categories
/// the Re-run feature handles: a plain string, a connection-typed field, and a
/// secret string (with `secret: true`).
fn build_rerun_workspace() -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();

    // Workspace secret — will be redacted in GET /api/jobs responses.
    workspace
        .secrets
        .insert("api_key".to_string(), json!("schema-default-api-key"));

    // Named connection of type "postgres".
    workspace.connections.insert(
        "production-db".to_string(),
        ConnectionDef {
            connection_type: Some("postgres".to_string()),
            values: HashMap::from([
                ("host".to_string(), json!("prod.example.com")),
                ("port".to_string(), json!(5432)),
            ]),
        },
    );

    // Minimal script action (no real execution needed — job creation is what we test).
    workspace.actions.insert(
        "noop".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo noop".to_string()),
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
            input: HashMap::new(),
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

    // Task with three input fields covering all sentinel categories.
    let mut task_input = HashMap::new();
    // 1. Plain string — sentinel has no special meaning; passed through as-is.
    task_input.insert(
        "data".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: false,
            secret: false,
            default: Some(json!("default-data")),
            options: None,
            allow_custom: false,
            order: None,
        },
    );
    // 2. Connection-typed field — sentinel resolved to connection name from source.
    task_input.insert(
        "db".to_string(),
        InputFieldDef {
            field_type: "postgres".to_string(),
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
    // 3. Secret string — sentinel resolved to actual value from source's raw_input.
    task_input.insert(
        "api_key".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: false,
            secret: true,
            default: Some(json!("{{ secret.api_key }}")),
            options: None,
            allow_custom: false,
            order: None,
        },
    );

    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        },
    );

    workspace.tasks.insert(
        "rerun-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: task_input,
            flow,
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
        },
    );

    workspace
}

struct TestApp {
    router: Router,
    pool: PgPool,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _tmp: TempDir,
}

async fn build_test_app(workspace_name: &str, workspace: WorkspaceConfig) -> Result<TestApp> {
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

    let mgr = WorkspaceManager::from_config(workspace_name, workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    let router = build_router(state, CancellationToken::new());

    Ok(TestApp {
        router,
        pool,
        _pg,
        _tmp: tmp,
    })
}

async fn execute_task(
    app: &TestApp,
    workspace: &str,
    task: &str,
    body: JsonValue,
) -> Result<(StatusCode, JsonValue)> {
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/workspaces/{}/tasks/{}/execute",
            workspace, task
        ))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))?;

    let resp = app.router.clone().oneshot(req).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let parsed: JsonValue = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes).to_string()}))
    };
    Ok((status, parsed))
}

async fn get_job(app: &TestApp, job_id: &str) -> Result<JsonValue> {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/jobs/{}", job_id))
        .body(Body::empty())?;
    let resp = app.router.clone().oneshot(req).await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /api/jobs/{} failed",
        job_id
    );
    let bytes = resp.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

/// Happy path: re-run with sentinels for connection + secret fields resolves
/// the values from the source job's raw_input.
#[tokio::test(flavor = "multi_thread")]
async fn rerun_replays_connection_and_secret_from_source() -> Result<()> {
    let app = build_test_app("default", build_rerun_workspace()).await?;

    // First job: submit the workspace secret value for api_key (so it can be
    // redacted in the response), and use the connection name for db.
    let (status, body) = execute_task(
        &app,
        "default",
        "rerun-task",
        json!({
            "input": {
                "data": "hello",
                "db": "production-db",
                // Use the workspace secret value so that redaction kicks in.
                "api_key": "schema-default-api-key",
            }
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "first execute failed: {body:?}");
    let first_job_id = body["job_id"].as_str().expect("job_id").to_string();

    let first_resp = get_job(&app, &first_job_id).await?;
    // Plain field — raw_input stores the value as submitted.
    assert_eq!(
        first_resp["raw_input"]["data"],
        json!("hello"),
        "plain field preserved in raw_input"
    );
    // Connection field — raw_input stores the connection NAME (not the values object).
    assert_eq!(
        first_resp["raw_input"]["db"],
        json!("production-db"),
        "connection name preserved in raw_input"
    );
    // Secret field — raw_input has the secret value, but GET response redacts it.
    assert_eq!(
        first_resp["raw_input"]["api_key"],
        json!("••••••"),
        "secret value redacted in raw_input on GET response"
    );
    // First job has no source lineage.
    assert!(first_resp["source_job_id"].is_null());
    assert_eq!(first_resp["source_type"], json!("api"));

    // Re-run: UI sends sentinels for connection + secret fields the user didn't touch.
    // The plain field "data" is sent unchanged.
    let (status, body) = execute_task(
        &app,
        "default",
        "rerun-task",
        json!({
            "input": {
                "data": "hello",
                "db": "••••••",
                "api_key": "••••••",
            },
            "source_job_id": first_job_id,
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "re-run execute failed: {body:?}");
    let second_job_id = body["job_id"].as_str().expect("job_id").to_string();

    let second_resp = get_job(&app, &second_job_id).await?;

    // Lineage fields set correctly.
    assert_eq!(
        second_resp["source_job_id"],
        json!(first_job_id),
        "source_job_id linked to first job"
    );
    assert_eq!(
        second_resp["source_type"],
        json!("rerun"),
        "source_type is 'rerun'"
    );

    // Connection sentinel was resolved to the connection NAME from source's raw_input,
    // then resolve_connection_inputs expanded it to the full values object in `input`.
    assert!(
        second_resp["input"]["db"].is_object(),
        "connection should resolve to values object in input, got: {:?}",
        second_resp["input"]["db"]
    );
    assert_eq!(
        second_resp["input"]["db"]["host"],
        json!("prod.example.com"),
        "connection host resolved correctly"
    );
    assert_eq!(
        second_resp["input"]["db"]["port"],
        json!(5432),
        "connection port resolved correctly"
    );

    // raw_input preserves the connection NAME (not the expanded values object).
    assert_eq!(
        second_resp["raw_input"]["db"],
        json!("production-db"),
        "connection name (not values) preserved in raw_input on re-run"
    );

    // Secret was replayed from source's raw_input (which stored the actual value)
    // and is redacted in the GET response because it matches the workspace secret.
    assert_eq!(
        second_resp["raw_input"]["api_key"],
        json!("••••••"),
        "secret field redacted in re-run raw_input response"
    );

    // Plain field passes through unchanged.
    assert_eq!(
        second_resp["raw_input"]["data"],
        json!("hello"),
        "plain field preserved on re-run"
    );

    Ok(())
}

/// Negative: unknown source_job_id → 400 Bad Request.
#[tokio::test(flavor = "multi_thread")]
async fn rerun_with_unknown_source_job_returns_400() -> Result<()> {
    let app = build_test_app("default", build_rerun_workspace()).await?;

    let (status, body) = execute_task(
        &app,
        "default",
        "rerun-task",
        json!({
            "input": {},
            "source_job_id": Uuid::new_v4().to_string(),
        }),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 for unknown source_job_id, got: {body:?}"
    );
    Ok(())
}

/// Negative: source job from a different workspace → 400 Bad Request.
#[tokio::test(flavor = "multi_thread")]
async fn rerun_cross_workspace_returns_400() -> Result<()> {
    let app = build_test_app("default", build_rerun_workspace()).await?;

    // Insert a synthetic job row whose workspace is 'other-ws' — different from 'default'.
    let foreign_job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job \
         (job_id, workspace, task_name, mode, input, raw_input, source_type, status, created_at) \
         VALUES ($1, 'other-ws', 'rerun-task', 'distributed', \
                 '{}'::jsonb, '{\"data\": \"x\"}'::jsonb, 'api', 'completed', NOW())",
    )
    .bind(foreign_job_id)
    .execute(&app.pool)
    .await?;

    let (status, body) = execute_task(
        &app,
        "default",
        "rerun-task",
        json!({
            "input": {},
            "source_job_id": foreign_job_id.to_string(),
        }),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 for cross-workspace source_job_id, got: {body:?}"
    );
    Ok(())
}
