//! Integration tests for Restart From Step (spec 2026-09-07).
//!
//! Drives a real three-step flow (`a → b → c`) through the worker API, then
//! restarts the failed job from a chosen step via
//! `job_creator::create_restart_job` and asserts on the seeded carried rows,
//! the promoted restart set, and the replayed input.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value as JsonValue};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{
    ActionDef, FlowStep, InputFieldDef, TaskDef, WorkspaceConfig,
};
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo};
use stroem_server::config::{
    DbConfig, JobDefaults, LogStorageConfig, RetentionConfig, ServerConfig, WorkspaceSourceDef,
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

const WORKER_TOKEN: &str = "test-token";

async fn spawn_pg() -> Result<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;
    Ok((pool, container))
}

fn script_action(cmd: &str) -> ActionDef {
    ActionDef {
        action_type: "script".to_string(),
        name: None,
        description: None,
        task: None,
        cmd: Some(cmd.to_string()),
        script: None,
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
    }
}

fn flow_step(action: &str, depends_on: &[&str], input: HashMap<String, JsonValue>) -> FlowStep {
    FlowStep {
        action: action.to_string(),
        name: None,
        description: None,
        depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        input,
        continue_on_failure: false,
        timeout: None,
        when: None,
        for_each: None,
        sequential: false,
        retry: None,
        inline_action: None,
    }
}

/// `a → b → c`, all plain script steps. `b` templates `a`'s output so a
/// restart from `b` proves the carried-over output still renders downstream.
fn line_workspace() -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();
    for name in ["a", "b", "c"] {
        workspace
            .actions
            .insert(name.to_string(), script_action("true"));
    }

    let mut flow = HashMap::new();
    flow.insert("a".to_string(), flow_step("a", &[], HashMap::new()));
    flow.insert(
        "b".to_string(),
        flow_step(
            "b",
            &["a"],
            HashMap::from([(
                "upstream".to_string(),
                json!("{{ a.output.val }}".to_string()),
            )]),
        ),
    );
    flow.insert("c".to_string(), flow_step("c", &["b"], HashMap::new()));

    let task_input = HashMap::from([(
        "note".to_string(),
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
    )]);

    workspace.tasks.insert(
        "line".to_string(),
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
    /// The same `WorkspaceConfig` the router serves — restart planning needs
    /// the current flow, and `create_restart_job` needs the config directly.
    workspace: WorkspaceConfig,
    /// A second manager over the same in-memory config; `AppState::new` takes
    /// the router's copy by value and `WorkspaceManager` is not `Clone`.
    mgr: WorkspaceManager,
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
        worker_token: WORKER_TOKEN.to_string(),
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
    };

    let mgr = WorkspaceManager::from_config(workspace_name, workspace.clone());
    let router_mgr = WorkspaceManager::from_config(workspace_name, workspace.clone());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(
        pool.clone(),
        router_mgr,
        config,
        log_storage,
        HashMap::new(),
        None,
    );
    let router = build_router(state, CancellationToken::new());

    Ok(TestApp {
        router,
        pool,
        workspace,
        mgr,
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

/// Authenticated worker-API call. Returns `(status, parsed body)`.
async fn worker_req(
    app: &TestApp,
    method: &str,
    uri: &str,
    body: JsonValue,
) -> Result<(StatusCode, JsonValue)> {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", WORKER_TOKEN))
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

async fn register_worker(app: &TestApp) -> Result<String> {
    let (status, body) = worker_req(
        app,
        "POST",
        "/worker/register",
        json!({"name": "restart-worker", "capabilities": ["script"]}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "register worker: {body}");
    Ok(body["worker_id"].as_str().unwrap().to_string())
}

async fn claim(app: &TestApp, worker_id: &str) -> Result<JsonValue> {
    let (status, body) = worker_req(
        app,
        "POST",
        "/worker/jobs/claim",
        json!({"worker_id": worker_id, "capabilities": ["script"]}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "claim: {body}");
    Ok(body)
}

async fn complete(app: &TestApp, job_id: Uuid, step: &str, body: JsonValue) -> Result<()> {
    let (status, resp) = worker_req(
        app,
        "POST",
        &format!("/worker/jobs/{}/steps/{}/complete", job_id, step),
        body,
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "complete {step}: {resp}");
    Ok(())
}

/// Claim the next ready step, assert it is the expected one, then complete it.
async fn complete_next(
    app: &TestApp,
    worker_id: &str,
    job_id: Uuid,
    expected_step: &str,
    result: JsonValue,
) -> Result<()> {
    let claimed = claim(app, worker_id).await?;
    assert_eq!(
        claimed["job_id"],
        json!(job_id.to_string()),
        "claimed a step from a different job: {claimed}"
    );
    assert_eq!(
        claimed["step_name"],
        json!(expected_step),
        "unexpected claimed step: {claimed}"
    );
    complete(app, job_id, expected_step, result).await
}

/// Run `line` to a failure at `b`: `a` completes with `val = A-OUT`, `b` fails.
async fn run_source_job_failing_at_b(app: &TestApp) -> Result<Uuid> {
    let (st, body) = execute_task(app, "default", "line", json!({"input": {"note": "n1"}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let worker = register_worker(app).await?;
    complete_next(
        app,
        &worker,
        job_id,
        "a",
        json!({"output": {"val": "A-OUT"}}),
    )
    .await?;
    complete_next(
        app,
        &worker,
        job_id,
        "b",
        json!({"exit_code": 1, "error": "b broke"}),
    )
    .await?;
    assert_eq!(get_job(app, &job_id.to_string()).await?["status"], "failed");
    Ok(job_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_from_middle_carries_upstream_and_reruns_downstream() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    let source_steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let plan = stroem_server::restart::compute_restart_set(
        &app.workspace.tasks["line"].flow,
        &source_steps,
        "b",
    )
    .unwrap();

    let created = stroem_server::job_creator::create_restart_job(
        &app.mgr,
        &app.pool,
        &app.workspace,
        "default",
        &source,
        &plan,
        "b",
        Some("tester"),
        None,
        JobDefaults::default(),
    )
    .await?;
    assert!(!created.terminal_at_creation);

    let new = JobRepo::get(&app.pool, created.job_id).await?.unwrap();
    assert_eq!(new.source_type, "restart");
    assert_eq!(new.source_id.as_deref(), Some("tester"));
    assert_eq!(new.source_job_id, Some(source_id));
    assert_eq!(new.restart_from_step.as_deref(), Some("b"));
    assert_eq!(new.raw_input, source.raw_input);
    assert_eq!(
        new.input, source.input,
        "replayed raw_input resolves to the same input"
    );

    let by: HashMap<_, _> = JobStepRepo::get_steps_for_job(&app.pool, created.job_id)
        .await?
        .into_iter()
        .map(|s| (s.step_name.clone(), s))
        .collect();
    assert_eq!(by["a"].status, "completed");
    assert!(by["a"].carried_over);
    assert_eq!(by["a"].output.as_ref().unwrap()["val"], "A-OUT");
    assert!(by["a"].started_at.is_none());
    assert_eq!(
        by["b"].status, "ready",
        "promoted immediately: its dep is carried completed"
    );
    assert!(!by["b"].carried_over);
    assert_eq!(by["c"].status, "pending");

    // Worker claims b: the carried output must render into b's input.
    let worker = register_worker(&app).await?;
    let claimed = claim(&app, &worker).await?;
    assert_eq!(claimed["job_id"], json!(created.job_id.to_string()));
    assert_eq!(claimed["step_name"], json!("b"));
    assert_eq!(claimed["input"]["upstream"], json!("A-OUT"));
    complete(&app, created.job_id, "b", json!({"output": {"ok": true}})).await?;
    complete_next(
        &app,
        &worker,
        created.job_id,
        "c",
        json!({"output": {"done": 1}}),
    )
    .await?;

    let j = get_job(&app, &created.job_id.to_string()).await?;
    assert_eq!(j["status"], "completed");
    assert_eq!(j["output"]["c"]["done"], 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_set_entirely_skipped_settles_failed_at_creation() -> Result<()> {
    // a → b → c, source: a failed, b/c skipped. Restart from c: b carried skipped,
    // a carried failed → c has all deps skipped → cascade-skipped → job failed at creation.
    let app = build_test_app("default", line_workspace()).await?;
    let (st, body) =
        execute_task(&app, "default", "line", json!({"input": {"note": "n1"}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let worker = register_worker(&app).await?;
    complete_next(
        &app,
        &worker,
        source_id,
        "a",
        json!({"exit_code": 1, "error": "a broke"}),
    )
    .await?;
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    assert_eq!(source.status, "failed");

    let steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let plan =
        stroem_server::restart::compute_restart_set(&app.workspace.tasks["line"].flow, &steps, "c")
            .unwrap();
    assert_eq!(plan.carried_failed, vec!["a".to_string()]);

    let created = stroem_server::job_creator::create_restart_job(
        &app.mgr,
        &app.pool,
        &app.workspace,
        "default",
        &source,
        &plan,
        "c",
        None,
        None,
        JobDefaults::default(),
    )
    .await?;
    assert!(created.terminal_at_creation);

    let new = JobRepo::get(&app.pool, created.job_id).await?.unwrap();
    assert_eq!(new.status, "failed");
    let by: HashMap<_, _> = JobStepRepo::get_steps_for_job(&app.pool, created.job_id)
        .await?
        .into_iter()
        .map(|s| (s.step_name.clone(), s))
        .collect();
    assert_eq!(by["c"].status, "skipped");
    assert!(!by["c"].carried_over);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_rejects_legacy_source_without_raw_input() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;
    sqlx::query("UPDATE job SET raw_input = NULL WHERE job_id = $1")
        .bind(source_id)
        .execute(&app.pool)
        .await?;
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    let steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let plan =
        stroem_server::restart::compute_restart_set(&app.workspace.tasks["line"].flow, &steps, "b")
            .unwrap();
    let err = stroem_server::job_creator::create_restart_job(
        &app.mgr,
        &app.pool,
        &app.workspace,
        "default",
        &source,
        &plan,
        "b",
        None,
        None,
        JobDefaults::default(),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("predates Re-run prefill"),
        "{err:#}"
    );
    Ok(())
}
