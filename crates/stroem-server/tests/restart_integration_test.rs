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
    ActionDef, ConnectionDef, ConnectionPropertyDef, ConnectionTypeDef, FlowStep, HookDef,
    InputFieldDef, TaskDef, WorkspaceConfig,
};
use stroem_db::{create_pool, run_migrations, JobRepo, JobStepRepo, JobStepRow, Seed};
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
        continue_when_skipped: false,
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
    ///
    /// **Warning — two managers.** A `TestApp` holds this one AND the router's,
    /// both `from_config` over identical configs. Calling
    /// `create_restart_job(&app.mgr, …)` directly resolves cross-workspace
    /// actions and `type: task` children against THIS manager, while anything
    /// driven through the router (`/execute`, worker claims, `/approve`) uses
    /// the router's. Nothing reloads either one, so the two stay equivalent —
    /// but suspect this split first if a `type: task` or cross-workspace
    /// restart test behaves oddly, and note that
    /// `build_test_app_with_pool` gives the second app a fully separate pair.
    mgr: WorkspaceManager,
    /// Kept alive for the lifetime of the app. `None` for a second app built
    /// over another app's pool (`build_test_app_with_pool`) — that app owns it.
    _pg: Option<testcontainers::ContainerAsync<Postgres>>,
    _tmp: TempDir,
}

async fn build_test_app(workspace_name: &str, workspace: WorkspaceConfig) -> Result<TestApp> {
    let (pool, pg) = spawn_pg().await?;
    let mut app = build_test_app_with_pool(pool, workspace_name, workspace).await?;
    app._pg = Some(pg);
    Ok(app)
}

/// Build a second app — its own router, workspace config and manager — over an
/// existing pool. Used by tests that must change the workspace between the
/// source run and the restart while the jobs stay in one database.
async fn build_test_app_with_pool(
    pool: PgPool,
    workspace_name: &str,
    workspace: WorkspaceConfig,
) -> Result<TestApp> {
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
                triggers: true,
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
        _pg: None,
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
    // `terminal_at_creation` is private to the settlement module; the step
    // statuses checked below ("ready"/"pending", not terminal) are the
    // externally-observable proof the job did not settle at creation.
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
    assert!(created.terminal_at_creation());

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

// ─────────────────────────────────────────────────────────────────────────────
// Task 4: for_each / type:task / approval carry-over, flow drift, input replay.
// ─────────────────────────────────────────────────────────────────────────────

/// A non-script action (`task`, `approval`, …) — `script_action` with `cmd`
/// cleared, since validation rejects `cmd` on those types.
fn base_action(action_type: &str) -> ActionDef {
    let mut a = script_action("true");
    a.action_type = action_type.to_string();
    a.cmd = None;
    a
}

fn input_field(field_type: &str) -> InputFieldDef {
    InputFieldDef {
        field_type: field_type.to_string(),
        name: None,
        description: None,
        required: false,
        secret: false,
        default: None,
        options: None,
        allow_custom: false,
        multiple: false,
        order: None,
    }
}

fn task_def(input: HashMap<String, InputFieldDef>, flow: HashMap<String, FlowStep>) -> TaskDef {
    TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input,
        flow,
        timeout: None,
        retry: None,
        on_success: vec![],
        on_error: vec![],
        on_suspended: vec![],
        on_cancel: vec![],
    }
}

/// Unauthenticated API call (`auth: None` ⇒ open server, no ACL).
async fn api_post(app: &TestApp, uri: &str, body: JsonValue) -> Result<(StatusCode, JsonValue)> {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
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

async fn steps_by_name(pool: &PgPool, job_id: Uuid) -> Result<HashMap<String, JobStepRow>> {
    Ok(JobStepRepo::get_steps_for_job(pool, job_id)
        .await?
        .into_iter()
        .map(|s| (s.step_name.clone(), s))
        .collect())
}

fn sorted_names(by: &HashMap<String, JobStepRow>) -> Vec<String> {
    let mut names: Vec<String> = by.keys().cloned().collect();
    names.sort();
    names
}

/// Every job row whose parent is `parent_job_id`, whatever its status
/// (`JobRepo::get_child_jobs` only lists live ones).
async fn children_of(pool: &PgPool, parent_job_id: Uuid) -> Result<Vec<(Uuid, Option<String>)>> {
    let rows: Vec<(Uuid, Option<String>)> =
        sqlx::query_as("SELECT job_id, parent_step_name FROM job WHERE parent_job_id = $1")
            .bind(parent_job_id)
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

/// Claim the next ready step of any job. `None` when the queue is empty.
async fn claim_next(app: &TestApp, worker_id: &str) -> Result<Option<(Uuid, String)>> {
    let claimed = claim(app, worker_id).await?;
    match claimed["step_name"].as_str() {
        None => Ok(None),
        Some(step) => Ok(Some((
            claimed["job_id"]
                .as_str()
                .expect("claim returned a step with no job_id")
                .parse()?,
            step.to_string(),
        ))),
    }
}

/// Claim/complete until nothing is left to claim. `result` picks the completion
/// body per step name, so one drain can complete some steps and fail others.
async fn drain_steps(
    app: &TestApp,
    worker_id: &str,
    result: impl Fn(&str) -> JsonValue,
) -> Result<usize> {
    for done in 0..64 {
        match claim_next(app, worker_id).await? {
            None => return Ok(done),
            Some((job_id, step)) => complete(app, job_id, &step, result(&step)).await?,
        }
    }
    anyhow::bail!("drain_steps still had claimable steps after 64 completions")
}

/// `seed → loop (for_each) → after → tail`. `after` templates the placeholder's
/// aggregated output, so a carried placeholder must keep that array intact.
fn for_each_workspace() -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::default();
    for name in ["seed", "loop", "after", "tail"] {
        ws.actions.insert(name.to_string(), script_action("true"));
    }

    let mut lp = flow_step("loop", &["seed"], HashMap::new());
    lp.for_each = Some(json!("{{ seed.output.items | json_encode() }}"));

    let flow = HashMap::from([
        ("seed".to_string(), flow_step("seed", &[], HashMap::new())),
        ("loop".to_string(), lp),
        (
            "after".to_string(),
            flow_step(
                "after",
                &["loop"],
                HashMap::from([("n".to_string(), json!("{{ loop.output | length }}"))]),
            ),
        ),
        (
            "tail".to_string(),
            flow_step("tail", &["after"], HashMap::new()),
        ),
    ]);
    ws.tasks
        .insert("looped".to_string(), task_def(HashMap::new(), flow));
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn carried_for_each_placeholder_exposes_aggregated_output_without_instances() -> Result<()> {
    let app = build_test_app("default", for_each_workspace()).await?;
    let (st, body) = execute_task(&app, "default", "looped", json!({"input": {}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    let worker = register_worker(&app).await?;
    drain_steps(&app, &worker, |step| match step {
        "seed" => json!({"output": {"items": [1, 2]}}),
        "tail" => json!({"exit_code": 1, "error": "tail broke"}),
        _ => json!({"output": {"i": 1}}),
    })
    .await?;

    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    assert_eq!(source.status, "failed");
    let source_steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    assert_eq!(
        sorted_names(&steps_by_name(&app.pool, source_id).await?),
        vec!["after", "loop", "loop[0]", "loop[1]", "seed", "tail"],
        "source job must have expanded the placeholder into two instances"
    );

    let plan = stroem_server::restart::compute_restart_set(
        &app.workspace.tasks["looped"].flow,
        &source_steps,
        "tail",
    )
    .unwrap();
    assert_eq!(plan.restart_steps, vec!["tail".to_string()]);

    let created = stroem_server::job_creator::create_restart_job(
        &app.mgr,
        &app.pool,
        &app.workspace,
        "default",
        &source,
        &plan,
        "tail",
        None,
        None,
        JobDefaults::default(),
    )
    .await?;

    // `terminal_at_creation` is private to the settlement module; the step
    // statuses checked below (not terminal) are the externally-observable
    // proof the job did not settle at creation.
    let by = steps_by_name(&app.pool, created.job_id).await?;
    assert_eq!(
        sorted_names(&by),
        vec!["after", "loop", "seed", "tail"],
        "instances are never recreated — only the flow's own steps exist"
    );
    assert!(by["loop"].carried_over);
    assert_eq!(by["loop"].status, "completed");
    assert_eq!(
        by["loop"].output,
        Some(json!([{"i": 1}, {"i": 1}])),
        "the placeholder keeps the source's aggregated array"
    );
    assert!(by["after"].carried_over && by["after"].status == "completed");
    assert_eq!(by["tail"].status, "ready");
    assert!(!by["tail"].carried_over);

    complete_next(
        &app,
        &worker,
        created.job_id,
        "tail",
        json!({"output": {"done": true}}),
    )
    .await?;
    let j = get_job(&app, &created.job_id.to_string()).await?;
    assert_eq!(j["status"], "completed");
    assert_eq!(j["output"]["tail"]["done"], json!(true));
    Ok(())
}

/// `sub` is a `type: task` action creating a child job; `tail` follows it.
fn task_action_workspace() -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::default();
    ws.actions.insert("work".to_string(), script_action("true"));
    ws.actions.insert("tail".to_string(), script_action("true"));
    let mut sub = base_action("task");
    sub.task = Some("child-task".to_string());
    ws.actions.insert("sub".to_string(), sub);

    ws.tasks.insert(
        "child-task".to_string(),
        task_def(
            HashMap::new(),
            HashMap::from([("work".to_string(), flow_step("work", &[], HashMap::new()))]),
        ),
    );
    ws.tasks.insert(
        "parent-task".to_string(),
        task_def(
            HashMap::new(),
            HashMap::from([
                ("sub".to_string(), flow_step("sub", &[], HashMap::new())),
                (
                    "tail".to_string(),
                    flow_step("tail", &["sub"], HashMap::new()),
                ),
            ]),
        ),
    );
    ws
}

/// Run `parent-task` until `tail` fails: the child job completes, propagates
/// `{"work": {"n": 7}}` onto `sub`, then `tail` fails.
async fn run_task_action_source(app: &TestApp, worker: &str) -> Result<Uuid> {
    let (st, body) = execute_task(app, "default", "parent-task", json!({"input": {}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    drain_steps(app, worker, |step| match step {
        "tail" => json!({"exit_code": 1, "error": "tail broke"}),
        _ => json!({"output": {"n": 7}}),
    })
    .await?;
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    assert_eq!(source.status, "failed");
    let by = steps_by_name(&app.pool, source_id).await?;
    assert_eq!(by["sub"].status, "completed");
    assert_eq!(by["sub"].output, Some(json!({"work": {"n": 7}})));
    Ok(source_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn carried_task_step_keeps_output_and_creates_no_child() -> Result<()> {
    let app = build_test_app("default", task_action_workspace()).await?;
    let worker = register_worker(&app).await?;
    let source_id = run_task_action_source(&app, &worker).await?;

    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    let source_steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let plan = stroem_server::restart::compute_restart_set(
        &app.workspace.tasks["parent-task"].flow,
        &source_steps,
        "tail",
    )
    .unwrap();

    let created = stroem_server::job_creator::create_restart_job(
        &app.mgr,
        &app.pool,
        &app.workspace,
        "default",
        &source,
        &plan,
        "tail",
        None,
        None,
        JobDefaults::default(),
    )
    .await?;

    let by = steps_by_name(&app.pool, created.job_id).await?;
    assert_eq!(by["sub"].status, "completed");
    assert!(by["sub"].carried_over);
    assert_eq!(
        by["sub"].output,
        Some(json!({"work": {"n": 7}})),
        "the carried row keeps the output Settlement::propagate stamped on it"
    );
    assert_eq!(by["tail"].status, "ready");
    assert!(
        JobRepo::get_child_jobs(&app.pool, created.job_id)
            .await?
            .is_empty(),
        "a carried type:task step must not re-dispatch its child"
    );
    assert!(
        children_of(&app.pool, created.job_id).await?.is_empty(),
        "no child job of any status may exist under the restart job"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_from_task_step_creates_child_under_new_job() -> Result<()> {
    let app = build_test_app("default", task_action_workspace()).await?;
    let worker = register_worker(&app).await?;
    let source_id = run_task_action_source(&app, &worker).await?;

    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    let source_steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let plan = stroem_server::restart::compute_restart_set(
        &app.workspace.tasks["parent-task"].flow,
        &source_steps,
        "sub",
    )
    .unwrap();
    assert_eq!(
        plan.restart_steps,
        vec!["sub".to_string(), "tail".to_string()]
    );
    assert!(plan.carried.is_empty());

    let created = stroem_server::job_creator::create_restart_job(
        &app.mgr,
        &app.pool,
        &app.workspace,
        "default",
        &source,
        &plan,
        "sub",
        None,
        None,
        JobDefaults::default(),
    )
    .await?;

    let children = children_of(&app.pool, created.job_id).await?;
    assert_eq!(
        children.len(),
        1,
        "restarting a type:task step re-dispatches it"
    );
    assert_eq!(children[0].1.as_deref(), Some("sub"));
    let child = JobRepo::get(&app.pool, children[0].0).await?.unwrap();
    assert_eq!(child.parent_job_id, Some(created.job_id));
    assert_eq!(child.task_name, "child-task");

    // The re-dispatched child runs for real and drives the restart job home.
    drain_steps(&app, &worker, |_| json!({"output": {"n": 9}})).await?;
    let j = get_job(&app, &created.job_id.to_string()).await?;
    assert_eq!(j["status"], "completed", "{j}");
    Ok(())
}

/// `a → gate (approval) → b`.
fn approval_workspace() -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::default();
    ws.actions.insert("a".to_string(), script_action("true"));
    ws.actions.insert("b".to_string(), script_action("true"));
    let mut gate = base_action("approval");
    gate.message = Some("ok?".to_string());
    ws.actions.insert("gate".to_string(), gate);

    ws.tasks.insert(
        "gated".to_string(),
        task_def(
            HashMap::new(),
            HashMap::from([
                ("a".to_string(), flow_step("a", &[], HashMap::new())),
                (
                    "gate".to_string(),
                    flow_step("gate", &["a"], HashMap::new()),
                ),
                ("b".to_string(), flow_step("b", &["gate"], HashMap::new())),
            ]),
        ),
    );
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_from_approval_step_suspends_new_job() -> Result<()> {
    let app = build_test_app("default", approval_workspace()).await?;
    let (st, body) = execute_task(&app, "default", "gated", json!({"input": {}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let worker = register_worker(&app).await?;

    complete_next(
        &app,
        &worker,
        source_id,
        "a",
        json!({"output": {"val": "A"}}),
    )
    .await?;
    let by = steps_by_name(&app.pool, source_id).await?;
    assert_eq!(
        by["gate"].status, "suspended",
        "gate must suspend on promotion"
    );

    let (st, resp) = api_post(
        &app,
        &format!("/api/jobs/{}/steps/gate/approve", source_id),
        json!({"approved": true}),
    )
    .await?;
    assert_eq!(st, StatusCode::OK, "{resp}");
    complete_next(
        &app,
        &worker,
        source_id,
        "b",
        json!({"exit_code": 1, "error": "b broke"}),
    )
    .await?;

    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    assert_eq!(source.status, "failed");
    let source_steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let plan = stroem_server::restart::compute_restart_set(
        &app.workspace.tasks["gated"].flow,
        &source_steps,
        "gate",
    )
    .unwrap();
    assert_eq!(
        plan.restart_steps,
        vec!["b".to_string(), "gate".to_string()]
    );

    let created = stroem_server::job_creator::create_restart_job(
        &app.mgr,
        &app.pool,
        &app.workspace,
        "default",
        &source,
        &plan,
        "gate",
        None,
        None,
        JobDefaults::default(),
    )
    .await?;

    // `terminal_at_creation` is private to the settlement module; the step
    // statuses checked below (not terminal) are the externally-observable
    // proof the job did not settle at creation.
    let by = steps_by_name(&app.pool, created.job_id).await?;
    assert_eq!(by["a"].status, "completed");
    assert!(by["a"].carried_over);
    assert_eq!(
        by["gate"].status, "suspended",
        "an approval step in the restart set is re-suspended at creation"
    );
    assert!(by["gate"].suspended_at.is_some());
    assert!(!by["gate"].carried_over);
    assert_eq!(by["b"].status, "pending");
    Ok(())
}

/// `chain` before the edit: `a → c`, plus an independent `z`.
fn chain_workspace_v1() -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::default();
    for name in ["a", "b", "c", "z"] {
        ws.actions.insert(name.to_string(), script_action("true"));
    }
    ws.tasks.insert(
        "chain".to_string(),
        task_def(
            HashMap::new(),
            HashMap::from([
                ("a".to_string(), flow_step("a", &[], HashMap::new())),
                ("c".to_string(), flow_step("c", &["a"], HashMap::new())),
                ("z".to_string(), flow_step("z", &[], HashMap::new())),
            ]),
        ),
    );
    ws
}

/// After the edit: `b` is inserted between `a` and `c`.
fn chain_workspace_v2() -> WorkspaceConfig {
    let mut ws = chain_workspace_v1();
    let flow = &mut ws.tasks.get_mut("chain").unwrap().flow;
    flow.insert("b".to_string(), flow_step("b", &["a"], HashMap::new()));
    flow.insert("c".to_string(), flow_step("c", &["b"], HashMap::new()));
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn flow_change_new_upstream_step_forces_rerun_of_dependent() -> Result<()> {
    let app = build_test_app("default", chain_workspace_v1()).await?;
    let (st, body) = execute_task(&app, "default", "chain", json!({"input": {}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let worker = register_worker(&app).await?;
    drain_steps(&app, &worker, |step| match step {
        "z" => json!({"exit_code": 1, "error": "z broke"}),
        _ => json!({"output": {"v": 1}}),
    })
    .await?;

    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    assert_eq!(source.status, "failed");
    let by = steps_by_name(&app.pool, source_id).await?;
    assert_eq!(by["a"].status, "completed");
    assert_eq!(by["c"].status, "completed");
    assert_eq!(by["z"].status, "failed");

    // The workspace gains `b` between `a` and `c` while the jobs stay put.
    let app2 = build_test_app_with_pool(app.pool.clone(), "default", chain_workspace_v2()).await?;
    let source_steps = JobStepRepo::get_steps_for_job(&app2.pool, source_id).await?;
    let plan = stroem_server::restart::compute_restart_set(
        &app2.workspace.tasks["chain"].flow,
        &source_steps,
        "z",
    )
    .unwrap();
    assert_eq!(
        plan.restart_steps,
        vec!["b".to_string(), "c".to_string(), "z".to_string()],
        "`b` is new so it is a root; `c` now depends on `b` so it reruns too"
    );
    assert_eq!(
        plan.carried
            .iter()
            .map(|s| s.step_name.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );

    let created = stroem_server::job_creator::create_restart_job(
        &app2.mgr,
        &app2.pool,
        &app2.workspace,
        "default",
        &source,
        &plan,
        "z",
        None,
        None,
        JobDefaults::default(),
    )
    .await?;

    let by = steps_by_name(&app2.pool, created.job_id).await?;
    assert_eq!(sorted_names(&by), vec!["a", "b", "c", "z"]);
    assert_eq!(by["a"].status, "completed");
    assert!(by["a"].carried_over);
    assert_eq!(
        by["b"].status, "ready",
        "the new step runs behind carried `a`"
    );
    assert!(!by["b"].carried_over);
    assert_eq!(by["c"].status, "pending", "`c` waits for the new `b`");
    assert!(!by["c"].carried_over);
    assert_eq!(by["z"].status, "ready");
    Ok(())
}

/// One connection `db` of type `postgres`, one task taking it as input.
fn conn_workspace(host: &str) -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::default();
    ws.connection_types.insert(
        "postgres".to_string(),
        ConnectionTypeDef {
            properties: HashMap::from([(
                "host".to_string(),
                ConnectionPropertyDef {
                    property_type: "string".to_string(),
                    required: true,
                    default: None,
                    secret: false,
                },
            )]),
        },
    );
    ws.connections.insert(
        "db".to_string(),
        ConnectionDef {
            connection_type: Some("postgres".to_string()),
            shared: false,
            values: HashMap::from([("host".to_string(), json!(host))]),
        },
    );
    ws.actions.insert("a".to_string(), script_action("true"));
    ws.actions.insert("b".to_string(), script_action("true"));
    ws.tasks.insert(
        "uses-conn".to_string(),
        task_def(
            HashMap::from([("conn".to_string(), input_field("postgres"))]),
            HashMap::from([
                ("a".to_string(), flow_step("a", &[], HashMap::new())),
                ("b".to_string(), flow_step("b", &["a"], HashMap::new())),
            ]),
        ),
    );
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn input_replay_rotated_connection_yields_new_value() -> Result<()> {
    let app = build_test_app("default", conn_workspace("old")).await?;
    let (st, body) = execute_task(
        &app,
        "default",
        "uses-conn",
        json!({"input": {"conn": "db"}}),
    )
    .await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let worker = register_worker(&app).await?;
    drain_steps(&app, &worker, |step| match step {
        "b" => json!({"exit_code": 1, "error": "b broke"}),
        _ => json!({"output": {"v": 1}}),
    })
    .await?;

    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    assert_eq!(source.status, "failed");
    assert_eq!(
        source.input.as_ref().unwrap()["conn"]["host"],
        json!("old"),
        "the source resolved the connection as it stood then"
    );
    assert_eq!(source.raw_input, Some(json!({"conn": "db"})));

    // The connection is rotated; the source job's frozen `input` is now stale.
    let app2 = build_test_app_with_pool(app.pool.clone(), "default", conn_workspace("new")).await?;
    let source_steps = JobStepRepo::get_steps_for_job(&app2.pool, source_id).await?;
    let plan = stroem_server::restart::compute_restart_set(
        &app2.workspace.tasks["uses-conn"].flow,
        &source_steps,
        "b",
    )
    .unwrap();

    let created = stroem_server::job_creator::create_restart_job(
        &app2.mgr,
        &app2.pool,
        &app2.workspace,
        "default",
        &source,
        &plan,
        "b",
        None,
        None,
        JobDefaults::default(),
    )
    .await?;

    let new = JobRepo::get(&app2.pool, created.job_id).await?.unwrap();
    assert_eq!(
        new.input.as_ref().unwrap()["conn"]["host"],
        json!("new"),
        "raw_input is replayed through resolve_connection_inputs against today's workspace"
    );
    assert_eq!(new.raw_input, Some(json!({"conn": "db"})));

    let source_after = JobRepo::get(&app2.pool, source_id).await?.unwrap();
    assert_eq!(
        source_after.input.as_ref().unwrap()["conn"]["host"],
        json!("old"),
        "the source job is never rewritten"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn seed_failure_rolls_back_the_whole_restart_job() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    let source_steps = JobStepRepo::get_steps_for_job(&app.pool, source_id).await?;
    let mut plan = stroem_server::restart::compute_restart_set(
        &app.workspace.tasks["line"].flow,
        &source_steps,
        "b",
    )
    .unwrap();
    // A seed for a step the current flow does not have: its UPDATE matches no row.
    plan.carried.push(Seed {
        step_name: "ghost".to_string(),
        status: "completed".to_string(),
        output: None,
        error_message: None,
        skip_reason: None,
    });

    let before = JobRepo::list(&app.pool, Some("default"), None, None, None, 100, 0)
        .await?
        .len();

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
    let chain = format!("{err:#}");
    assert!(chain.contains("ghost"), "{chain}");

    let after = JobRepo::list(&app.pool, Some("default"), None, None, None, 100, 0)
        .await?
        .len();
    assert_eq!(
        before, after,
        "a failed seed must roll back the job row it was created with"
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 5: POST /api/jobs/{id}/restart — dry run, real run, rejections,
// redaction of carried output, and terminal-at-creation side effects.
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /api/jobs/{job_id}/restart`. The test server has `auth: None`, so no
/// token is needed and `check_job_acl` short-circuits to `Run`.
async fn restart_req(
    app: &TestApp,
    job_id: Uuid,
    body: JsonValue,
) -> Result<(StatusCode, JsonValue)> {
    api_post(app, &format!("/api/jobs/{}/restart", job_id), body).await
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_endpoint_dry_run_then_real_run() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;

    let (st, body) =
        restart_req(&app, source_id, json!({"from_step": "b", "dry_run": true})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["restart_steps"], json!(["b", "c"]));
    assert_eq!(body["carried_over"], json!(["a"]));
    assert_eq!(body["carried_failed"], json!([]));
    assert_eq!(body["carried_failed_tolerated"], json!([]));
    assert!(
        body.get("job_id").is_none(),
        "dry run must not report a job id: {body}"
    );
    assert_eq!(
        JobRepo::list(&app.pool, Some("default"), None, None, None, 100, 0)
            .await?
            .len(),
        1,
        "dry run creates nothing"
    );

    let (st, body) = restart_req(&app, source_id, json!({"from_step": "b"})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    assert_eq!(body["restart_steps"], json!(["b", "c"]));
    assert_eq!(body["carried_over"], json!(["a"]));
    assert_eq!(body["carried_failed"], json!([]));

    let new = get_job(&app, &new_id.to_string()).await?;
    assert_eq!(new["source_type"], "restart");
    assert_eq!(new["restart_from_step"], "b");
    assert_eq!(new["source_job_id"], source_id.to_string());
    let steps = new["steps"].as_array().unwrap();
    let a = steps.iter().find(|s| s["step_name"] == "a").unwrap();
    assert_eq!(a["carried_over"], true);
    assert_eq!(a["status"], "completed");
    let b = steps.iter().find(|s| s["step_name"] == "b").unwrap();
    assert_eq!(b["carried_over"], false);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_endpoint_rejections() -> Result<()> {
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;

    // A job that is still running cannot be restarted. Created after the source
    // run so its own `a` never competes for the worker's claim.
    let (st, body) = execute_task(&app, "default", "line", json!({"input": {"note": "x"}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let running: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let (st, body) = restart_req(&app, running, json!({"from_step": "a"})).await?;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("not in a terminal state"),
        "the 409 also covers `pending` and an unparseable status, so it must not \
         claim the job is running: {body}"
    );

    // Unknown step / loop instance / unknown job / malformed body.
    let (st, body) = restart_req(&app, source_id, json!({"from_step": "nope"})).await?;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("not in the current flow"),
        "{body}"
    );
    let (st, body) = restart_req(&app, source_id, json!({"from_step": "b[0]"})).await?;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("not an instance"),
        "{body}"
    );
    let (st, body) = restart_req(&app, Uuid::new_v4(), json!({"from_step": "a"})).await?;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    let (st, body) = restart_req(&app, source_id, json!({"nope": 1})).await?;
    assert_eq!(
        st,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body without from_step must not reach the handler: {body}"
    );

    // Legacy source without raw_input.
    sqlx::query("UPDATE job SET raw_input = NULL WHERE job_id = $1")
        .bind(source_id)
        .execute(&app.pool)
        .await?;
    let (st, body) = restart_req(&app, source_id, json!({"from_step": "b"})).await?;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("predates"),
        "{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_endpoint_redacts_carried_step_output() -> Result<()> {
    // A carried row's copied output goes through the same secret redaction as
    // a row the new job actually executed.
    let mut ws = line_workspace();
    ws.secrets
        .insert("token".to_string(), json!("s3cr3t-value"));
    let app = build_test_app("default", ws).await?;

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
        json!({"output": {"val": "A-OUT", "leak": "s3cr3t-value"}}),
    )
    .await?;
    complete_next(
        &app,
        &worker,
        source_id,
        "b",
        json!({"exit_code": 1, "error": "b broke"}),
    )
    .await?;

    let (st, body) = restart_req(&app, source_id, json!({"from_step": "b"})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id = body["job_id"].as_str().unwrap().to_string();

    // The row in the database keeps the real value (it is a verbatim copy)...
    let by = steps_by_name(&app.pool, new_id.parse()?).await?;
    assert_eq!(by["a"].output.as_ref().unwrap()["leak"], "s3cr3t-value");

    // ...but the API masks it, exactly as it does for the source job.
    let new = get_job(&app, &new_id).await?;
    let a = new["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["step_name"] == "a")
        .unwrap();
    assert_eq!(a["carried_over"], true);
    assert_eq!(a["output"]["leak"], "••••••");
    assert_eq!(a["output"]["val"], "A-OUT");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_endpoint_terminal_at_creation_fires_hooks() -> Result<()> {
    // A restart whose whole restart set cascade-skips is terminal the moment it
    // is created. The endpoint must still run terminal handling (hooks, metrics,
    // log archive) exactly once — that is `Settlement::job_created`.
    let mut ws = line_workspace();
    ws.actions
        .insert("notify".to_string(), script_action("true"));
    ws.on_error = vec![HookDef {
        action: "notify".to_string(),
        input: HashMap::new(),
    }];
    let app = build_test_app("default", ws).await?;

    // Source: `a` fails, `b`/`c` cascade-skip → job failed.
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
    assert_eq!(
        get_job(&app, &source_id.to_string()).await?["status"],
        "failed"
    );

    // Restart from `c`: `a` carries failed, `b` carries skipped, `c` is the
    // whole restart set and cascade-skips → failed at creation.
    let (st, body) = restart_req(&app, source_id, json!({"from_step": "c"})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    assert_eq!(body["carried_failed"], json!(["a"]));
    assert_eq!(
        get_job(&app, &new_id.to_string()).await?["status"],
        "failed"
    );

    // Hook jobs carry the triggering job's id in `source_id`.
    let hook_jobs: Vec<(Uuid,)> =
        sqlx::query_as("SELECT job_id FROM job WHERE source_type = 'hook' AND source_id = $1")
            .bind(new_id.to_string())
            .fetch_all(&app.pool)
            .await?;
    assert_eq!(
        hook_jobs.len(),
        1,
        "a restart that settles at creation must fire its on_error hook exactly once"
    );
    Ok(())
}

/// A restart that settles failed at creation carries its failure forward from
/// the source run rather than producing a fresh one. `hook.failed_steps` must
/// flag that failure `carried_over: true` so an `on_error` hook can
/// distinguish "this job just failed" from "an old failure was carried
/// forward by Restart-from-step" (spec 2026-09-07 §6.3).
#[tokio::test(flavor = "multi_thread")]
async fn restart_hook_context_flags_carried_over_failure() -> Result<()> {
    let mut ws = line_workspace();
    ws.actions
        .insert("notify".to_string(), script_action("true"));
    ws.on_error = vec![HookDef {
        action: "notify".to_string(),
        input: HashMap::from([(
            "failed".to_string(),
            json!("{{ hook.failed_steps | json_encode() }}"),
        )]),
    }];
    let app = build_test_app("default", ws).await?;

    // Source: `a` fails, `b`/`c` cascade-skip → job failed.
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
    assert_eq!(
        get_job(&app, &source_id.to_string()).await?["status"],
        "failed"
    );

    // Restart from `c`: `a` carries failed, `b` carries skipped, `c` is the
    // whole restart set and cascade-skips → failed at creation.
    let (st, body) = restart_req(&app, source_id, json!({"from_step": "c"})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    assert_eq!(body["carried_failed"], json!(["a"]));

    // Exactly one hook job, fired for the restarted job.
    let hook_job_ids: Vec<(Uuid,)> =
        sqlx::query_as("SELECT job_id FROM job WHERE source_type = 'hook' AND source_id = $1")
            .bind(new_id.to_string())
            .fetch_all(&app.pool)
            .await?;
    assert_eq!(hook_job_ids.len(), 1, "expected exactly one hook job");
    let hook_job_id = hook_job_ids[0].0;

    let hook_steps = JobStepRepo::get_steps_for_job(&app.pool, hook_job_id).await?;
    let hook_step = hook_steps
        .iter()
        .find(|s| s.step_name == "hook")
        .expect("hook job must have a 'hook' step");
    let rendered_failed = hook_step.input.as_ref().unwrap()["failed"]
        .as_str()
        .expect("rendered `failed` input must be a JSON-encoded string");
    let failed_steps: JsonValue = serde_json::from_str(rendered_failed)?;
    let failed_steps = failed_steps.as_array().unwrap();
    assert_eq!(
        failed_steps.len(),
        1,
        "only 'a' failed in the source run: {failed_steps:?}"
    );
    assert_eq!(failed_steps[0]["step_name"], "a");
    assert_eq!(
        failed_steps[0]["carried_over"], true,
        "step 'a' failed in the SOURCE run, not this restart — hook.failed_steps \
         must flag it carried_over: {failed_steps:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Top-level-only guard (final review, Important 1)
// ---------------------------------------------------------------------------

/// `line`, plus a `type: task` workspace-level `on_error` hook. A `type: task`
/// hook creates a FULL job (`source_type = "hook"`, `raw_input` recorded), which
/// is the discriminating shape: without the top-level guard it would sail past
/// every other precondition and be restarted into a `source_type = "restart"`
/// job — a source type `is_top_level_source` treats as top-level, re-enabling
/// exactly the workspace-hook fanout the `hook` source type exists to suppress.
fn line_workspace_with_task_hook() -> WorkspaceConfig {
    let mut ws = line_workspace();
    ws.actions
        .insert("notify-step".to_string(), script_action("true"));
    let mut notify = base_action("task");
    notify.task = Some("notify-task".to_string());
    ws.actions.insert("notify".to_string(), notify);
    ws.tasks.insert(
        "notify-task".to_string(),
        task_def(
            HashMap::new(),
            HashMap::from([(
                "notify-step".to_string(),
                flow_step("notify-step", &[], HashMap::new()),
            )]),
        ),
    );
    ws.on_error = vec![HookDef {
        action: "notify".to_string(),
        input: HashMap::new(),
    }];
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_rejects_hook_sourced_job() -> Result<()> {
    let app = build_test_app("default", line_workspace_with_task_hook()).await?;
    let worker = register_worker(&app).await?;

    // Source `line` fails at `a`, so the workspace `on_error` hook fires and
    // creates a full `source_type = "hook"` job for `notify-task`.
    let (st, body) =
        execute_task(&app, "default", "line", json!({"input": {"note": "n1"}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    complete_next(
        &app,
        &worker,
        source_id,
        "a",
        json!({"exit_code": 1, "error": "a broke"}),
    )
    .await?;
    assert_eq!(
        get_job(&app, &source_id.to_string()).await?["status"],
        "failed"
    );

    // Run the hook job to completion so it is terminal (409 is checked first).
    drain_steps(&app, &worker, |_| json!({"output": {"ok": true}})).await?;
    let hook_jobs: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT job_id FROM job WHERE source_type = 'hook' AND task_name = 'notify-task'",
    )
    .fetch_all(&app.pool)
    .await?;
    assert_eq!(hook_jobs.len(), 1, "expected exactly one hook task job");
    let hook_job_id = hook_jobs[0].0;
    let hook_job = JobRepo::get(&app.pool, hook_job_id).await?.unwrap();
    assert_eq!(hook_job.status, "completed");
    assert!(
        hook_job.raw_input.is_some(),
        "a type:task hook job records raw_input, so only the top-level guard \
         can reject this restart"
    );

    let (st, body) = restart_req(&app, hook_job_id, json!({"from_step": "notify-step"})).await?;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Only top-level jobs can be restarted"),
        "{body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_and_rerun_reject_a_type_task_child_job() -> Result<()> {
    let app = build_test_app("default", task_action_workspace()).await?;
    let worker = register_worker(&app).await?;
    let source_id = run_task_action_source(&app, &worker).await?;

    let children = children_of(&app.pool, source_id).await?;
    assert_eq!(children.len(), 1, "parent-task creates one child job");
    let child_id = children[0].0;
    let child = JobRepo::get(&app.pool, child_id).await?.unwrap();
    assert_eq!(child.status, "completed");
    assert_eq!(child.source_type, "task");
    assert!(child.raw_input.is_some());

    // Restart: a child restart would create a parentless job that never
    // propagates back to `parent-task`'s `sub` step.
    let (st, body) = restart_req(&app, child_id, json!({"from_step": "work"})).await?;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Only top-level jobs can be restarted"),
        "{body}"
    );

    // Re-run has the identical detachment problem and the identical rule.
    let (st, body) = execute_task(
        &app,
        "default",
        "child-task",
        json!({"input": {}, "source_job_id": child_id.to_string()}),
    )
    .await?;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Only top-level jobs can be re-run"),
        "{body}"
    );

    // Nothing was created by either rejected call.
    assert_eq!(
        JobRepo::list(&app.pool, Some("default"), None, None, None, 100, 0)
            .await?
            .len(),
        2,
        "only the source parent job and its child exist"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Settlement of carried rows, end to end (final review, Important 2)
// ---------------------------------------------------------------------------

/// `a → b`, `a → c` fan-out. `c` optionally tolerates its own failure, so the
/// same shape covers both the tolerated-failure and the cancelled settlement.
fn fan_out_workspace(c_tolerates_failure: bool) -> WorkspaceConfig {
    let mut ws = WorkspaceConfig::default();
    for name in ["a", "b", "c"] {
        ws.actions.insert(name.to_string(), script_action("true"));
    }
    let mut c = flow_step("c", &["a"], HashMap::new());
    c.continue_on_failure = c_tolerates_failure;
    ws.tasks.insert(
        "fan".to_string(),
        task_def(
            HashMap::from([("note".to_string(), input_field("string"))]),
            HashMap::from([
                ("a".to_string(), flow_step("a", &[], HashMap::new())),
                ("b".to_string(), flow_step("b", &["a"], HashMap::new())),
                ("c".to_string(), c),
            ]),
        ),
    );
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn carried_tolerated_failure_does_not_fail_the_restart_job() -> Result<()> {
    // Spec §4.3: a carried `failed` row whose CURRENT flow step is
    // `continue_on_failure: true` must not fail the restart job.
    let app = build_test_app("default", fan_out_workspace(true)).await?;
    let worker = register_worker(&app).await?;

    let (st, body) = execute_task(&app, "default", "fan", json!({"input": {"note": "n1"}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    complete_next(&app, &worker, source_id, "a", json!({"output": {"val": 1}})).await?;
    // Both `b` and `c` fail. `b` is untolerated, so the source job ends failed.
    drain_steps(
        &app,
        &worker,
        |step| json!({"exit_code": 1, "error": format!("{step} broke")}),
    )
    .await?;
    let by = steps_by_name(&app.pool, source_id).await?;
    assert_eq!(by["b"].status, "failed");
    assert_eq!(by["c"].status, "failed");
    assert_eq!(
        get_job(&app, &source_id.to_string()).await?["status"],
        "failed"
    );

    // Restart from `b` alone: `c` is carried failed but tolerated.
    let (st, body) = restart_req(&app, source_id, json!({"from_step": "b"})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    assert_eq!(body["restart_steps"], json!(["b"]));
    assert_eq!(body["carried_over"], json!(["a", "c"]));
    assert_eq!(
        body["carried_failed"],
        json!([]),
        "`c` is tolerated, so it must not be reported as job-failing: {body}"
    );

    complete_next(&app, &worker, new_id, "b", json!({"output": {"ok": true}})).await?;

    let by = steps_by_name(&app.pool, new_id).await?;
    assert_eq!(by["c"].status, "failed");
    assert!(by["c"].carried_over);
    assert_eq!(
        get_job(&app, &new_id.to_string()).await?["status"],
        "completed",
        "a carried failure the current flow tolerates must not fail the restart job"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn carried_cancelled_row_settles_the_restart_job_cancelled() -> Result<()> {
    // Spec §4.3: a carried `cancelled` row with no failures settles the job
    // `cancelled`, NOT `completed` — a behaviour the settlement unification adds.
    let app = build_test_app("default", fan_out_workspace(false)).await?;
    let worker = register_worker(&app).await?;

    let (st, body) = execute_task(&app, "default", "fan", json!({"input": {"note": "n1"}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    complete_next(&app, &worker, source_id, "a", json!({"output": {"val": 1}})).await?;

    // Claim one of the two ready steps and leave it in flight, then cancel the
    // whole job. The claimed step stays `running` (the worker is the one that
    // reports it done); its unclaimed sibling lands terminal `cancelled`.
    let claimed = claim_next(&app, &worker).await?;
    let (_, in_flight) = claimed.expect("b/c must be claimable after a completes");
    let sibling = if in_flight == "b" { "c" } else { "b" };
    let (st, body) = api_post(&app, &format!("/api/jobs/{}/cancel", source_id), json!({})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let by = steps_by_name(&app.pool, source_id).await?;
    assert_eq!(
        by[sibling].status, "cancelled",
        "the unclaimed sibling must be cancelled outright: {by:?}"
    );
    assert_eq!(
        get_job(&app, &source_id.to_string()).await?["status"],
        "cancelled"
    );

    // Restart from the step that was in flight, so the terminal `cancelled`
    // sibling is the carried row and nothing in the job ever failed.
    let (st, body) = restart_req(&app, source_id, json!({"from_step": in_flight})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    assert_eq!(body["restart_steps"], json!([in_flight]));
    assert_eq!(body["carried_over"], json!(["a", sibling]));
    assert_eq!(body["carried_failed"], json!([]));

    complete_next(
        &app,
        &worker,
        new_id,
        &in_flight,
        json!({"output": {"ok": true}}),
    )
    .await?;

    let by = steps_by_name(&app.pool, new_id).await?;
    assert_eq!(by[sibling].status, "cancelled");
    assert!(by[sibling].carried_over);
    assert_eq!(
        get_job(&app, &new_id.to_string()).await?["status"],
        "cancelled",
        "a carried cancelled row with no failures settles the job cancelled, not completed"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Input replay against a drifted schema (Codex should-fix 1)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn restart_rejects_input_missing_a_newly_required_field() -> Result<()> {
    // Restart replays `raw_input` with no form in front of it. If the task
    // schema has since gained a required field with no default, the replayed
    // input no longer satisfies it — that must be a 400, not a job that runs
    // with a hole in its input.
    let app = build_test_app("default", line_workspace()).await?;
    let source_id = run_source_job_failing_at_b(&app).await?;

    // Second app over the SAME pool whose `line` task requires `extra`.
    let mut drifted = line_workspace();
    let mut extra = input_field("string");
    extra.required = true;
    drifted
        .tasks
        .get_mut("line")
        .unwrap()
        .input
        .insert("extra".to_string(), extra);
    let app2 = build_test_app_with_pool(app.pool.clone(), "default", drifted).await?;

    let (st, body) = restart_req(&app2, source_id, json!({"from_step": "b"})).await?;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("required"), "{body}");
    assert!(msg.contains("extra"), "{body}");

    // No job was created and the source is untouched.
    assert_eq!(
        JobRepo::list(&app.pool, Some("default"), None, None, None, 100, 0)
            .await?
            .len(),
        1,
        "a rejected restart must create nothing"
    );
    let source = JobRepo::get(&app.pool, source_id).await?.unwrap();
    assert_eq!(source.status, "failed");
    assert_eq!(source.raw_input, Some(json!({"note": "n1"})));

    // The dry run still reports a plan: planning is pure and does not touch
    // input. The create is the authoritative step, and it is what rejects.
    let (st, body) =
        restart_req(&app2, source_id, json!({"from_step": "b", "dry_run": true})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Endpoint-level lifecycle coverage (Codex should-fix 2)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn restart_endpoint_from_approval_step_fires_suspended_hook() -> Result<()> {
    // The creator-level test asserts the row suspends; this one goes through
    // POST /restart and asserts the endpoint's own `fire_initial_suspended_hooks`
    // call actually fires the workspace `on_suspended` hook for the new job.
    let mut ws = approval_workspace();
    ws.actions
        .insert("notify".to_string(), script_action("true"));
    ws.on_suspended = vec![HookDef {
        action: "notify".to_string(),
        input: HashMap::new(),
    }];
    let app = build_test_app("default", ws).await?;
    let worker = register_worker(&app).await?;

    let (st, body) = execute_task(&app, "default", "gated", json!({"input": {}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    complete_next(
        &app,
        &worker,
        source_id,
        "a",
        json!({"output": {"val": "A"}}),
    )
    .await?;
    let (st, resp) = api_post(
        &app,
        &format!("/api/jobs/{}/steps/gate/approve", source_id),
        json!({"approved": true}),
    )
    .await?;
    assert_eq!(st, StatusCode::OK, "{resp}");
    // The SOURCE run already fired its own on_suspended hook when `gate`
    // suspended, so `b` and that hook job's step are both claimable in an
    // unspecified order — drain rather than assuming which comes first.
    drain_steps(&app, &worker, |step| match step {
        "b" => json!({"exit_code": 1, "error": "b broke"}),
        _ => json!({"output": {}}),
    })
    .await?;
    assert_eq!(
        get_job(&app, &source_id.to_string()).await?["status"],
        "failed"
    );

    let (st, body) = restart_req(&app, source_id, json!({"from_step": "gate"})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    let by = steps_by_name(&app.pool, new_id).await?;
    assert_eq!(
        by["gate"].status, "suspended",
        "restart from an approval step re-suspends at the gate"
    );
    assert!(by["a"].carried_over);

    let hook_jobs: Vec<(Uuid,)> =
        sqlx::query_as("SELECT job_id FROM job WHERE source_type = 'hook' AND source_id = $1")
            .bind(new_id.to_string())
            .fetch_all(&app.pool)
            .await?;
    assert_eq!(
        hook_jobs.len(),
        1,
        "the restart endpoint must fire on_suspended exactly once for the new job"
    );
    Ok(())
}

/// `parent-task` (`sub` → `tail`) where `sub`'s child task's only root step is
/// `when: "false"`, so the child job settles the moment it is created.
fn born_terminal_child_workspace() -> WorkspaceConfig {
    let mut ws = task_action_workspace();
    let mut work = flow_step("work", &[], HashMap::new());
    work.when = Some("false".to_string());
    ws.tasks.get_mut("child-task").unwrap().flow = HashMap::from([("work".to_string(), work)]);
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_from_task_step_with_born_terminal_child_settles_the_parent() -> Result<()> {
    // A child job whose only step is `when: false` is terminal at creation. The
    // restart job's `sub` step must still be told: `Settlement::reconcile`
    // propagates, `sub` completes, `tail` runs and the restart job settles.
    let app = build_test_app("default", born_terminal_child_workspace()).await?;
    let worker = register_worker(&app).await?;

    let (st, body) = execute_task(&app, "default", "parent-task", json!({"input": {}})).await?;
    assert_eq!(st, StatusCode::OK, "{body}");
    let source_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    drain_steps(
        &app,
        &worker,
        |_| json!({"exit_code": 1, "error": "tail broke"}),
    )
    .await?;
    let by = steps_by_name(&app.pool, source_id).await?;
    assert_eq!(
        by["sub"].status, "completed",
        "the born-terminal child must complete the parent's task step: {by:?}"
    );
    assert_eq!(by["tail"].status, "failed");

    let (st, body) = restart_req(&app, source_id, json!({"from_step": "sub"})).await?;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    assert_eq!(body["restart_steps"], json!(["sub", "tail"]));

    let by = steps_by_name(&app.pool, new_id).await?;
    assert_eq!(
        by["sub"].status, "completed",
        "the re-dispatched child settles at creation and propagates: {by:?}"
    );
    assert!(!by["sub"].carried_over, "`sub` was rerun, not carried");

    drain_steps(&app, &worker, |_| json!({"output": {"ok": true}})).await?;
    assert_eq!(
        get_job(&app, &new_id.to_string()).await?["status"],
        "completed",
        "the restart job must settle rather than hang on the born-terminal child"
    );
    Ok(())
}
