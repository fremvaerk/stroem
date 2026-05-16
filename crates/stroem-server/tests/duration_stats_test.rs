use anyhow::Result;
use axum::body::Body;
use axum::Router;
use chrono::{Duration, Utc};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{
    ActionDef, FlowStep, InputFieldDef, TaskDef, WorkspaceConfig,
};
use stroem_db::{create_pool, run_migrations, JobRepo};
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

// ─── Minimal workspace with one task ────────────────────────────────────────

fn stats_test_workspace() -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();

    workspace.actions.insert(
        "noop".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("true".to_string()),
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

    let mut flow = HashMap::new();
    flow.insert(
        "run".to_string(),
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

    let mut task_input = HashMap::new();
    task_input.insert(
        "x".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: false,
            secret: false,
            default: Some(json!("")),
            options: None,
            allow_custom: false,
            multiple: false,
            order: None,
        },
    );

    workspace.tasks.insert(
        "my-task".to_string(),
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

// ─── Test infrastructure ────────────────────────────────────────────────────

async fn setup() -> Result<(
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
        worker_token: "test-token".to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        agents: None,
        state_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
    };

    let workspace = stats_test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir, container))
}

fn api_get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

/// Helper: insert a completed job directly via SQL.
async fn insert_completed_job(
    pool: &PgPool,
    workspace: &str,
    task: &str,
    duration_secs: i64,
) -> Result<Uuid> {
    let job_id = JobRepo::create(
        pool,
        workspace,
        task,
        "distributed",
        None,
        "api",
        None,
        None,
        None,
    )
    .await?;
    let completed_at = Utc::now();
    let started_at = completed_at - Duration::seconds(duration_secs);
    sqlx::query(
        "UPDATE job SET status='completed', started_at=$1, completed_at=$2 WHERE job_id=$3",
    )
    .bind(started_at)
    .bind(completed_at)
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(job_id)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// GET /api/workspaces/nonexistent/tasks/foo/stats → 404
#[tokio::test]
async fn test_stats_nonexistent_workspace_returns_404() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_get("/api/workspaces/nonexistent/tasks/foo/stats"))
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// Missing task within valid workspace → 404
#[tokio::test]
async fn test_stats_missing_task_returns_404() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks/no-such-task/stats"))
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// Zero completed runs → 200 with `sample_size: 0` and null aggregates
#[tokio::test]
async fn test_stats_zero_completed_runs() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks/my-task/stats"))
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    let task = &body["task"];
    assert_eq!(task["sample_size"], json!(0));
    assert!(task["avg_ms"].is_null(), "avg_ms should be null");
    assert!(task["p50_ms"].is_null(), "p50_ms should be null");
    assert!(task["p95_ms"].is_null(), "p95_ms should be null");
    assert!(task["min_ms"].is_null(), "min_ms should be null");
    assert!(task["max_ms"].is_null(), "max_ms should be null");

    Ok(())
}

/// `?limit=0` — clamps to minimum (1)
#[tokio::test]
async fn test_stats_limit_zero_clamped() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Insert two completed jobs so the limit matters.
    insert_completed_job(&pool, "default", "my-task", 1).await?;
    insert_completed_job(&pool, "default", "my-task", 2).await?;

    let response = router
        .oneshot(api_get(
            "/api/workspaces/default/tasks/my-task/stats?limit=0",
        ))
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    // limit clamped to 1 → sample_size == 1 (only the most-recent run)
    assert_eq!(body["task"]["sample_size"], json!(1));
    assert_eq!(body["window"], json!(1));

    Ok(())
}

/// `?limit=9999` — clamps to maximum (500)
#[tokio::test]
async fn test_stats_limit_large_clamped() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    insert_completed_job(&pool, "default", "my-task", 5).await?;

    let response = router
        .oneshot(api_get(
            "/api/workspaces/default/tasks/my-task/stats?limit=9999",
        ))
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    // window should be clamped to 500, not 9999
    assert_eq!(body["window"], json!(500));

    Ok(())
}

/// `?limit=abc` — non-numeric limit → 400 (axum query extractor rejects it)
#[tokio::test]
async fn test_stats_limit_non_numeric_returns_400() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_get(
            "/api/workspaces/default/tasks/my-task/stats?limit=abc",
        ))
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// `?limit=-5` — negative value → axum query deserialization fails (i64 parses
/// the value but clamp(1, 500) corrects it). Actually a valid i64 parse of "-5"
/// → after clamp becomes 1, so 200 with window=1.
#[tokio::test]
async fn test_stats_limit_negative_clamped() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    insert_completed_job(&pool, "default", "my-task", 3).await?;
    insert_completed_job(&pool, "default", "my-task", 3).await?;

    let response = router
        .oneshot(api_get(
            "/api/workspaces/default/tasks/my-task/stats?limit=-5",
        ))
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    // clamp(-5, 1, 500) == 1
    assert_eq!(body["window"], json!(1));
    assert_eq!(body["task"]["sample_size"], json!(1));

    Ok(())
}
