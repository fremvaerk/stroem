use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::Request;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{
    ActionDef, FlowStep, InputFieldDef, TaskDef, WorkspaceConfig,
};
use stroem_db::{
    create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep, UserRepo, WorkerRepo,
};
use stroem_server::auth::hash_password;
use stroem_server::config::{
    AuthConfig, DbConfig, InitialUserConfig, LogStorageConfig, ServerConfig, WorkspaceSourceConfig,
};
use stroem_server::orchestrator;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

// ─── Test helpers ───────────────────────────────────────────────────────

fn test_workspace() -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();

    // Action: greet (shell)
    let mut greet_input = HashMap::new();
    greet_input.insert(
        "name".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: None,
        },
    );
    workspace.actions.insert(
        "greet".to_string(),
        ActionDef {
            action_type: "shell".to_string(),
            cmd: Some("echo Hello $NAME".to_string()),
            script: None,
            image: None,
            command: None,
            env: None,
            workdir: None,
            resources: None,
            input: greet_input,
            output: None,
        },
    );

    // Action: shout (shell)
    workspace.actions.insert(
        "shout".to_string(),
        ActionDef {
            action_type: "shell".to_string(),
            cmd: Some("echo $MSG | tr a-z A-Z".to_string()),
            script: None,
            image: None,
            command: None,
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
        },
    );

    // Action: docker-build (docker type)
    workspace.actions.insert(
        "docker-build".to_string(),
        ActionDef {
            action_type: "docker".to_string(),
            cmd: None,
            script: None,
            image: Some("docker:latest".to_string()),
            command: Some(vec![
                "docker".to_string(),
                "build".to_string(),
                ".".to_string(),
            ]),
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
        },
    );

    // Task: hello-world (single step)
    let mut hello_flow = HashMap::new();
    let mut hello_input = HashMap::new();
    hello_input.insert("name".to_string(), json!("{{ input.name }}"));
    hello_flow.insert(
        "greet".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: hello_input,
            continue_on_failure: false,
        },
    );
    let mut task_input = HashMap::new();
    task_input.insert(
        "name".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: None,
        },
    );
    workspace.tasks.insert(
        "hello-world".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: task_input,
            flow: hello_flow,
        },
    );

    // Task: greet-and-shout (2-step linear: greet → shout)
    let mut gs_flow = HashMap::new();
    let mut greet_step_input = HashMap::new();
    greet_step_input.insert("name".to_string(), json!("{{ input.name }}"));
    gs_flow.insert(
        "greet".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: greet_step_input,
            continue_on_failure: false,
        },
    );
    let mut shout_step_input = HashMap::new();
    shout_step_input.insert("msg".to_string(), json!("{{ greet.output.greeting }}"));
    gs_flow.insert(
        "shout".to_string(),
        FlowStep {
            action: "shout".to_string(),
            depends_on: vec!["greet".to_string()],
            input: shout_step_input,
            continue_on_failure: false,
        },
    );
    let mut gs_task_input = HashMap::new();
    gs_task_input.insert(
        "name".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: None,
        },
    );
    workspace.tasks.insert(
        "greet-and-shout".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: gs_task_input,
            flow: gs_flow,
        },
    );

    // Task: linear-3 (3-step linear: step1 → step2 → step3)
    let mut l3_flow = HashMap::new();
    l3_flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    l3_flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    l3_flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step2".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    workspace.tasks.insert(
        "linear-3".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: HashMap::new(),
            flow: l3_flow,
        },
    );

    // Task: diamond (step1 → step2, step1 → step3, step2+step3 → step4)
    let mut d_flow = HashMap::new();
    d_flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    d_flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    d_flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    d_flow.insert(
        "step4".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step2".to_string(), "step3".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    workspace.tasks.insert(
        "diamond".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: HashMap::new(),
            flow: d_flow,
        },
    );

    // Task: docker-build-task (single step using docker action)
    let mut dbt_flow = HashMap::new();
    dbt_flow.insert(
        "build".to_string(),
        FlowStep {
            action: "docker-build".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    workspace.tasks.insert(
        "docker-build-task".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: HashMap::new(),
            flow: dbt_flow,
        },
    );

    // Task: mixed-input (2-step with both static and template input values)
    let mut mi_flow = HashMap::new();
    let mut mi_greet_input = HashMap::new();
    mi_greet_input.insert("name".to_string(), json!("{{ input.name }}"));
    mi_flow.insert(
        "greet".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: mi_greet_input,
            continue_on_failure: false,
        },
    );
    let mut mi_process_input = HashMap::new();
    mi_process_input.insert("msg".to_string(), json!("{{ greet.output.greeting }}"));
    mi_process_input.insert("static_key".to_string(), json!("fixed-value"));
    mi_process_input.insert("number".to_string(), json!(42));
    mi_flow.insert(
        "process".to_string(),
        FlowStep {
            action: "shout".to_string(),
            depends_on: vec!["greet".to_string()],
            input: mi_process_input,
            continue_on_failure: false,
        },
    );
    let mut mi_task_input = HashMap::new();
    mi_task_input.insert(
        "name".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: None,
        },
    );
    workspace.tasks.insert(
        "mixed-input".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: mi_task_input,
            flow: mi_flow,
        },
    );

    // Task: wide-fan-in (3 parallel steps → 1 join step with 3 dependencies)
    let mut wfi_flow = HashMap::new();
    wfi_flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    wfi_flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    wfi_flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    wfi_flow.insert(
        "step4".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![
                "step1".to_string(),
                "step2".to_string(),
                "step3".to_string(),
            ],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    workspace.tasks.insert(
        "wide-fan-in".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: HashMap::new(),
            flow: wfi_flow,
        },
    );

    // Action: db-backup (shell with env templates)
    let mut db_input = HashMap::new();
    db_input.insert(
        "host".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: None,
        },
    );
    let mut db_env = HashMap::new();
    db_env.insert("DB_HOST".to_string(), "{{ input.host }}".to_string());
    db_env.insert("DB_PASSWORD".to_string(), "{{ secret.db_pw }}".to_string());
    db_env.insert(
        "DB_PASSWORD_NESTED".to_string(),
        "{{ secret.db.password }}".to_string(),
    );
    db_env.insert(
        "DB_HOST_NESTED".to_string(),
        "{{ secret.db.host }}".to_string(),
    );
    db_env.insert("STATIC_VAR".to_string(), "no-template".to_string());
    workspace.actions.insert(
        "db-backup".to_string(),
        ActionDef {
            action_type: "shell".to_string(),
            cmd: Some("pg_dump -h {{ input.host }}".to_string()),
            script: None,
            image: None,
            command: None,
            env: Some(db_env),
            workdir: None,
            resources: None,
            input: db_input,
            output: None,
        },
    );

    // Task: backup-task (single step using db-backup action)
    let mut bt_flow = HashMap::new();
    let mut bt_input = HashMap::new();
    bt_input.insert("host".to_string(), json!("{{ input.host }}"));
    bt_flow.insert(
        "backup".to_string(),
        FlowStep {
            action: "db-backup".to_string(),
            depends_on: vec![],
            input: bt_input,
            continue_on_failure: false,
        },
    );
    let mut bt_task_input = HashMap::new();
    bt_task_input.insert(
        "host".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: None,
        },
    );
    workspace.tasks.insert(
        "backup-task".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: bt_task_input,
            flow: bt_flow,
        },
    );

    // Add secrets to workspace (flat + nested)
    workspace
        .secrets
        .insert("db_pw".to_string(), json!("ref+vault://secret/db#password"));
    workspace.secrets.insert(
        "db".to_string(),
        json!({
            "password": "ref+sops://secrets.enc.yaml#/db/password",
            "host": "db.internal.prod"
        }),
    );

    // Action: transform (shell with output)
    let mut transform_input = HashMap::new();
    transform_input.insert(
        "data".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: None,
        },
    );
    workspace.actions.insert(
        "transform".to_string(),
        ActionDef {
            action_type: "shell".to_string(),
            cmd: Some(
                "echo Processing $DATA && echo 'OUTPUT: {\"result\": \"processed-'$DATA'\"}'"
                    .to_string(),
            ),
            script: None,
            image: None,
            command: None,
            env: None,
            workdir: None,
            resources: None,
            input: transform_input,
            output: None,
        },
    );

    // Action: summarize (shell with output)
    let mut summarize_input = HashMap::new();
    summarize_input.insert(
        "value".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: None,
        },
    );
    workspace.actions.insert(
        "summarize".to_string(),
        ActionDef {
            action_type: "shell".to_string(),
            cmd: Some(
                "echo Summarizing $VALUE && echo 'OUTPUT: {\"summary\": \"'$VALUE' done\"}'"
                    .to_string(),
            ),
            script: None,
            image: None,
            command: None,
            env: None,
            workdir: None,
            resources: None,
            input: summarize_input,
            output: None,
        },
    );

    // Task: data-pipeline (2-step: transform → summarize, terminal step produces output)
    let mut dp_flow = HashMap::new();
    let mut dp_transform_input = HashMap::new();
    dp_transform_input.insert("data".to_string(), json!("{{ input.data }}"));
    dp_flow.insert(
        "transform".to_string(),
        FlowStep {
            action: "transform".to_string(),
            depends_on: vec![],
            input: dp_transform_input,
            continue_on_failure: false,
        },
    );
    let mut dp_summarize_input = HashMap::new();
    dp_summarize_input.insert("value".to_string(), json!("{{ transform.output.result }}"));
    dp_flow.insert(
        "summarize".to_string(),
        FlowStep {
            action: "summarize".to_string(),
            depends_on: vec!["transform".to_string()],
            input: dp_summarize_input,
            continue_on_failure: false,
        },
    );
    let mut dp_task_input = HashMap::new();
    dp_task_input.insert(
        "data".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            required: true,
            default: Some(json!("test")),
        },
    );
    workspace.tasks.insert(
        "data-pipeline".to_string(),
        TaskDef {
            mode: "distributed".to_string(),
            input: dp_task_input,
            flow: dp_flow,
        },
    );

    workspace
}

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
        },
        workspace: WorkspaceSourceConfig {
            source_type: "folder".to_string(),
            path: temp_dir.path().to_string_lossy().to_string(),
        },
        worker_token: "test-token-secret".to_string(),
        auth: None,
    };

    let workspace = test_workspace();
    let state = AppState::new(pool.clone(), workspace, config);
    let router = build_router(state);

    Ok((router, pool, temp_dir, container))
}

fn worker_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

fn api_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
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

/// Helper to build a log push request body using the JSONL `lines` format.
fn log_lines_body(step_name: &str, lines: &[(&str, &str)]) -> Value {
    let entries: Vec<Value> = lines
        .iter()
        .map(|(stream, line)| {
            json!({
                "ts": "2025-02-12T10:00:00Z",
                "stream": stream,
                "line": line,
            })
        })
        .collect();
    json!({ "step_name": step_name, "lines": entries })
}

// ─── Test 1: Execute task creates job and steps ───────────────────────

#[tokio::test]
async fn test_execute_task_creates_job_and_steps() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "Alice"}}),
        ))
        .await?;

    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Verify job was created
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.task_name, "hello-world");
    assert_eq!(job.status, "pending");
    assert_eq!(job.input, Some(json!({"name": "Alice"})));

    // Verify step was created
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].step_name, "greet");
    assert_eq!(steps[0].action_name, "greet");
    assert_eq!(steps[0].status, "ready");

    Ok(())
}

// ─── Test 2: Worker register and claim ────────────────────────────────

#[tokio::test]
async fn test_worker_register_and_claim() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Create a job first
    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        Some(json!({"name": "Bob"})),
        "api",
        None,
    )
    .await?;

    let steps = vec![NewJobStep {
        job_id,
        step_name: "greet".to_string(),
        action_name: "greet".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "echo Hello"})),
        input: Some(json!({"name": "{{ input.name }}"})),
        status: "ready".to_string(),
    }];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Register worker via API
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "worker-1", "capabilities": ["shell"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    // Claim step via API
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["shell"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["job_id"].as_str().unwrap(), job_id.to_string());
    assert_eq!(body["step_name"].as_str().unwrap(), "greet");
    assert!(body["action_spec"].is_object());

    Ok(())
}

// ─── Test 3: Step output flows to next step (template rendering) ──────

#[tokio::test]
async fn test_step_output_flows_to_next_step() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute greet-and-shout task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/greet-and-shout/execute",
            json!({"input": {"name": "World"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Register worker
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    // Claim the first step (greet)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "greet");

    // Verify input was rendered with job input
    let input = &body["input"];
    assert_eq!(input["name"], "World");

    // Complete greet with output
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({"output": {"greeting": "hello world"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Now claim the shout step
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "shout");

    // Verify shout's input was rendered with greet's output
    let input = &body["input"];
    assert_eq!(input["msg"], "hello world");

    Ok(())
}

// ─── Test 4: Step failure marks job failed ────────────────────────────

#[tokio::test]
async fn test_step_failure_marks_job_failed() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute single-step task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "Fail"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Complete step with failure
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({"exit_code": 1, "error": "Command failed with exit code 1"}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Verify step is marked failed
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "failed");
    assert_eq!(
        steps[0].error_message.as_deref(),
        Some("Command failed with exit code 1")
    );

    // Verify job is marked failed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test 5: Step failure blocks dependents ───────────────────────────

#[tokio::test]
async fn test_step_failure_blocks_dependents() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute 2-step task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/greet-and-shout/execute",
            json!({"input": {"name": "Blocked"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Fail greet step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({"exit_code": 1, "error": "Failed to greet"}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Verify shout step was skipped (unreachable due to failed dependency)
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let shout = steps.iter().find(|s| s.step_name == "shout").unwrap();
    assert_eq!(shout.status, "skipped");

    // Verify job is marked failed (all steps are terminal now)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    // Verify no steps are claimable
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert!(body["job_id"].is_null());

    Ok(())
}

// ─── Test 6: Orchestrator linear flow (3-step) ───────────────────────

#[tokio::test]
async fn test_orchestrator_linear_flow() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute 3-step linear task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/linear-3/execute",
            json!({"input": {}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Verify initial state: step1=ready, step2=pending, step3=pending
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let statuses: HashMap<_, _> = steps
        .iter()
        .map(|s| (s.step_name.as_str(), s.status.as_str()))
        .collect();
    assert_eq!(statuses["step1"], "ready");
    assert_eq!(statuses["step2"], "pending");
    assert_eq!(statuses["step3"], "pending");

    // Complete step1
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step1/complete", job_id),
            json!({"output": {"result": "s1"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Verify step2 promoted to ready
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let statuses: HashMap<_, _> = steps
        .iter()
        .map(|s| (s.step_name.as_str(), s.status.as_str()))
        .collect();
    assert_eq!(statuses["step2"], "ready");
    assert_eq!(statuses["step3"], "pending");

    // Complete step2
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step2/complete", job_id),
            json!({"output": {"result": "s2"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Verify step3 promoted to ready
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let statuses: HashMap<_, _> = steps
        .iter()
        .map(|s| (s.step_name.as_str(), s.status.as_str()))
        .collect();
    assert_eq!(statuses["step3"], "ready");

    // Complete step3
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step3/complete", job_id),
            json!({"output": {"result": "s3"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Verify job completed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test 7: Diamond DAG ─────────────────────────────────────────────

#[tokio::test]
async fn test_orchestrator_diamond_dag() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/diamond/execute",
            json!({"input": {}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Initial: step1=ready, others=pending
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let statuses: HashMap<_, _> = steps
        .iter()
        .map(|s| (s.step_name.as_str(), s.status.as_str()))
        .collect();
    assert_eq!(statuses["step1"], "ready");
    assert_eq!(statuses["step2"], "pending");
    assert_eq!(statuses["step3"], "pending");
    assert_eq!(statuses["step4"], "pending");

    // Complete step1 → step2 and step3 become ready
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step1/complete", job_id),
            json!({"output": {"v": 1}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let statuses: HashMap<_, _> = steps
        .iter()
        .map(|s| (s.step_name.as_str(), s.status.as_str()))
        .collect();
    assert_eq!(statuses["step2"], "ready");
    assert_eq!(statuses["step3"], "ready");
    assert_eq!(statuses["step4"], "pending");

    // Complete step2
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step2/complete", job_id),
            json!({"output": {"v": 2}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // step4 still pending (needs step3 too)
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let statuses: HashMap<_, _> = steps
        .iter()
        .map(|s| (s.step_name.as_str(), s.status.as_str()))
        .collect();
    assert_eq!(statuses["step4"], "pending");

    // Complete step3 → step4 becomes ready
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step3/complete", job_id),
            json!({"output": {"v": 3}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let statuses: HashMap<_, _> = steps
        .iter()
        .map(|s| (s.step_name.as_str(), s.status.as_str()))
        .collect();
    assert_eq!(statuses["step4"], "ready");

    // Complete step4 → job completed
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step4/complete", job_id),
            json!({"output": {"v": 4}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test 8: Log append and retrieve ─────────────────────────────────

#[tokio::test]
async fn test_log_append_and_retrieve() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Append log chunks via worker API (JSONL format)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "Line 1")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "Line 2")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Retrieve via public API
    let response = router
        .oneshot(api_get(&format!("/api/jobs/{}/logs", job_id)))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let logs_str = body["logs"].as_str().unwrap();
    // Logs are now JSONL — each line is a JSON object
    let log_lines: Vec<Value> = logs_str
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(log_lines.len(), 2);
    assert_eq!(log_lines[0]["line"], "Line 1");
    assert_eq!(log_lines[0]["step"], "build");
    assert_eq!(log_lines[1]["line"], "Line 2");

    Ok(())
}

// ─── Test 9: Worker auth required ────────────────────────────────────

#[tokio::test]
async fn test_worker_auth_required() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Call worker endpoint without auth header
    let request = Request::builder()
        .method("POST")
        .uri("/worker/register")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&json!({"name": "bad", "capabilities": ["shell"]})).unwrap(),
        ))
        .unwrap();

    let response = router.oneshot(request).await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test 10: Worker auth invalid token ──────────────────────────────

#[tokio::test]
async fn test_worker_auth_invalid_token() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/worker/register")
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer wrong-token")
        .body(Body::from(
            serde_json::to_string(&json!({"name": "bad", "capabilities": ["shell"]})).unwrap(),
        ))
        .unwrap();

    let response = router.oneshot(request).await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test 11: List tasks from workspace ──────────────────────────────

#[tokio::test]
async fn test_list_tasks_from_workspace() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router.oneshot(api_get("/api/tasks")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let tasks = body.as_array().unwrap();
    let task_names: Vec<&str> = tasks.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(task_names.contains(&"hello-world"));
    assert!(task_names.contains(&"greet-and-shout"));
    assert!(task_names.contains(&"linear-3"));
    assert!(task_names.contains(&"diamond"));

    Ok(())
}

// ─── Test 12: Get job with steps ─────────────────────────────────────

#[tokio::test]
async fn test_get_job_with_steps() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Create and execute job
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/greet-and-shout/execute",
            json!({"input": {"name": "Detail"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id = body["job_id"].as_str().unwrap();

    // Get job detail
    let response = router
        .oneshot(api_get(&format!("/api/jobs/{}", job_id)))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    assert_eq!(body["task_name"], "greet-and-shout");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["input"], json!({"name": "Detail"}));

    let steps = body["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);

    let step_names: Vec<&str> = steps
        .iter()
        .map(|s| s["step_name"].as_str().unwrap())
        .collect();
    assert!(step_names.contains(&"greet"));
    assert!(step_names.contains(&"shout"));

    Ok(())
}

// ─── Test 13: Orchestrator with failure (via DB directly) ─────────────

#[tokio::test]
async fn test_orchestrator_with_failure_db() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );

    let task = TaskDef {
        mode: "distributed".to_string(),
        input: HashMap::new(),
        flow,
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![NewJobStep {
        job_id,
        step_name: "step1".to_string(),
        action_name: "greet".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "exit 1"})),
        input: None,
        status: "ready".to_string(),
    }];
    JobStepRepo::create_steps(&pool, &steps).await?;

    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test 14: Orchestrator linear flow (via DB directly) ──────────────

#[tokio::test]
async fn test_orchestrator_linear_flow_db() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step2".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );

    let task = TaskDef {
        mode: "distributed".to_string(),
        input: HashMap::new(),
        flow,
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step3".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 3"})),
            input: None,
            status: "pending".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete step1
    JobStepRepo::mark_completed(&pool, job_id, "step1", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task).await?;

    let job_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step2 = job_steps.iter().find(|s| s.step_name == "step2").unwrap();
    assert_eq!(step2.status, "ready");

    // Complete step2
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task).await?;

    let job_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step3 = job_steps.iter().find(|s| s.step_name == "step3").unwrap();
    assert_eq!(step3.status, "ready");

    // Complete step3
    JobStepRepo::mark_completed(&pool, job_id, "step3", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step3", &task).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test 15: Execute nonexistent task ────────────────────────────────

#[tokio::test]
async fn test_execute_nonexistent_task() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/tasks/nonexistent/execute",
            json!({"input": {}}),
        ))
        .await?;

    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Test 16: Get task detail ─────────────────────────────────────────

#[tokio::test]
async fn test_get_task_detail() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .clone()
        .oneshot(api_get("/api/tasks/hello-world"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    assert_eq!(body["name"], "hello-world");
    assert_eq!(body["mode"], "distributed");
    assert!(body["input"].is_object());
    assert!(body["input"]["name"].is_object());
    assert!(body["flow"].is_object());
    assert!(body["flow"]["greet"].is_object());

    // Nonexistent task returns 404
    let response = router
        .oneshot(api_get("/api/tasks/nonexistent-task"))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Test 17: List jobs ───────────────────────────────────────────────

#[tokio::test]
async fn test_list_jobs() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Initially empty
    let response = router.clone().oneshot(api_get("/api/jobs")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body.as_array().unwrap().len(), 0);

    // Create two jobs
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "A"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "B"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // List should have 2 jobs
    let response = router.clone().oneshot(api_get("/api/jobs")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body.as_array().unwrap().len(), 2);

    // List with limit
    let response = router.oneshot(api_get("/api/jobs?limit=1")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    Ok(())
}

// ─── Test 18: Worker heartbeat ────────────────────────────────────────

#[tokio::test]
async fn test_worker_heartbeat() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Register worker
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "hb-worker", "capabilities": ["shell"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    // Send heartbeat
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/heartbeat",
            json!({"worker_id": worker_id}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["status"], "ok");

    Ok(())
}

// ─── Test 19: Worker start step ───────────────────────────────────────

#[tokio::test]
async fn test_worker_start_step() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "Start"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Register worker
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "start-worker", "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    // Claim step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["shell"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Start step
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/start", job_id),
            json!({"worker_id": worker_id}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["status"], "ok");

    // Verify step has started_at set
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "running");
    assert!(steps[0].started_at.is_some());

    // Verify job is now running with started_at set
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "running");
    assert!(
        job.started_at.is_some(),
        "job.started_at should be set after first step starts"
    );

    Ok(())
}

// ─── Test: Job started_at set via API after step start ────────────────

#[tokio::test]
async fn test_job_started_at_visible_in_api() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Execute task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "TimingTest"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id = body["job_id"].as_str().unwrap().to_string();

    // Job should be pending with no started_at
    let response = router
        .clone()
        .oneshot(api_get(&format!("/api/jobs/{}", job_id)))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["status"], "pending");
    assert!(
        body["started_at"].is_null(),
        "started_at should be null before step starts"
    );

    // Register worker
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "timing-worker", "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    // Claim step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["shell"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Start step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/start", job_id),
            json!({"worker_id": worker_id}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Job should now be running with started_at set
    let response = router
        .oneshot(api_get(&format!("/api/jobs/{}", job_id)))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["status"], "running");
    assert!(
        body["started_at"].is_string(),
        "started_at should be set in API response after step starts, got: {:?}",
        body["started_at"]
    );

    Ok(())
}

// ─── Test 20: Worker complete job (local mode) ────────────────────────

#[tokio::test]
async fn test_worker_complete_job() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Create job directly
    let job_id =
        JobRepo::create(&pool, "default", "hello-world", "local", None, "api", None).await?;

    // Complete job via worker API
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/complete", job_id),
            json!({"output": {"result": "done"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["status"], "ok");

    // Verify job is completed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    assert_eq!(job.output, Some(json!({"result": "done"})));

    Ok(())
}

// ─── Test 21: Docker action type flow ─────────────────────────────────

#[tokio::test]
async fn test_docker_action_type_flow() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute docker-build-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/docker-build-task/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Verify step has docker action type and image
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].action_type, "docker");
    assert_eq!(steps[0].action_image.as_deref(), Some("docker:latest"));

    // Register docker-capable worker and claim
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "docker-worker", "capabilities": ["docker"]}),
        ))
        .await?;
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["docker"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["action_type"].as_str().unwrap(), "docker");
    assert_eq!(body["action_image"].as_str().unwrap(), "docker:latest");
    assert!(body["action_spec"].is_object());

    Ok(())
}

// ─── Test 22: Capability mismatch - shell worker can't claim docker ───

#[tokio::test]
async fn test_capability_mismatch_no_claim() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Execute docker-build-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/docker-build-task/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register shell-only worker
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "shell-only", "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    // Try to claim with shell capability - should get null (no matching docker step)
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["shell"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert!(body["job_id"].is_null());

    Ok(())
}

// ─── Test 23: Multi-capability worker ─────────────────────────────────

#[tokio::test]
async fn test_multi_capability_worker() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Create one shell job and one docker job
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "Multi"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/docker-build-task/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register multi-capability worker
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "multi-worker", "capabilities": ["shell", "docker"]}),
        ))
        .await?;
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    // Claim first step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["shell", "docker"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert!(body["job_id"].is_string());
    let first_type = body["action_type"].as_str().unwrap().to_string();

    // Claim second step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["shell", "docker"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert!(body["job_id"].is_string());
    let second_type = body["action_type"].as_str().unwrap().to_string();

    // Both types should have been claimed
    let mut types = vec![first_type, second_type];
    types.sort();
    assert_eq!(types, vec!["docker", "shell"]);

    // No more steps
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "capabilities": ["shell", "docker"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert!(body["job_id"].is_null());

    Ok(())
}

// ─── Test 24: Exit code 0 with error message → step fails ─────────────

#[tokio::test]
async fn test_exit_code_zero_with_error() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "ErrorTest"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Complete with exit_code=0 but error message present
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({"exit_code": 0, "error": "Unexpected error occurred"}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Step should be failed because error is present
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "failed");
    assert_eq!(
        steps[0].error_message.as_deref(),
        Some("Unexpected error occurred")
    );

    Ok(())
}

// ─── Test 25: Exit code nonzero, no error message → generates default ──

#[tokio::test]
async fn test_exit_code_nonzero_no_error_message() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "ExitCode"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Complete with exit_code=127, no error message
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({"exit_code": 127}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Step should be failed with auto-generated error message
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "failed");
    assert_eq!(
        steps[0].error_message.as_deref(),
        Some("Process exited with code 127")
    );

    Ok(())
}

// ─── Test 26: Complete step with no output, no error → success ─────────

#[tokio::test]
async fn test_complete_step_success_no_output() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "NoOutput"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Complete with empty body (no output, no exit_code, no error)
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Step should be completed (success)
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "completed");
    assert!(steps[0].output.is_none());
    assert!(steps[0].error_message.is_none());

    // Job should be completed too (single step task)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test 27: Mixed static and template input ─────────────────────────

#[tokio::test]
async fn test_mixed_static_and_template_input() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute mixed-input task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/mixed-input/execute",
            json!({"input": {"name": "MixedTest"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Register worker
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    // Claim greet step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "greet");

    // Complete greet with output
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({"output": {"greeting": "Hello MixedTest"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Claim process step - should have mixed static and rendered template values
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "process");

    let input = &body["input"];
    assert_eq!(input["msg"], "Hello MixedTest"); // template rendered
    assert_eq!(input["static_key"], "fixed-value"); // static preserved
    assert_eq!(input["number"], 42); // numeric static preserved

    Ok(())
}

// ─── Test 28: First step template rendering (no completed deps) ────────

#[tokio::test]
async fn test_first_step_template_rendering() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute hello-world - first step has template input {{ input.name }}
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/hello-world/execute",
            json!({"input": {"name": "FirstStep"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "greet");

    // Input should be rendered with job input (no prior step outputs needed)
    let input = &body["input"];
    assert_eq!(input["name"], "FirstStep");

    Ok(())
}

// ─── Test 29: Dependency with null output ──────────────────────────────

#[tokio::test]
async fn test_dependency_with_null_output() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute greet-and-shout (shout depends on greet.output.greeting)
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/greet-and-shout/execute",
            json!({"input": {"name": "NullOutput"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    // Claim greet step
    let _response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;

    // Complete greet with NO output
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Claim shout step - template {{ greet.output.greeting }} can't resolve
    // because greet has no output. Should fall back to raw stored input.
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "shout");
    // The step should still be claimable (fallback to raw input, not a crash)
    assert!(body["input"].is_object() || body["input"].is_null());

    Ok(())
}

// ─── Test 30: Invalid UUID in path ────────────────────────────────────

#[tokio::test]
async fn test_invalid_uuid_in_path() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Invalid UUID for complete_step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/not-a-uuid/steps/greet/complete",
            json!({"output": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 400);

    // Invalid UUID for get_job
    let response = router
        .clone()
        .oneshot(api_get("/api/jobs/not-a-uuid"))
        .await?;
    assert_eq!(response.status(), 400);

    // Valid UUID but nonexistent job
    let fake_id = Uuid::new_v4();
    let response = router
        .oneshot(api_get(&format!("/api/jobs/{}", fake_id)))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Test 31: Auth with wrong header format (no Bearer prefix) ─────────

#[tokio::test]
async fn test_auth_no_bearer_prefix() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Use "Token" prefix instead of "Bearer"
    let request = Request::builder()
        .method("POST")
        .uri("/worker/register")
        .header("Content-Type", "application/json")
        .header("Authorization", "Token test-token-secret")
        .body(Body::from(
            serde_json::to_string(&json!({"name": "bad", "capabilities": ["shell"]})).unwrap(),
        ))
        .unwrap();

    let response = router.oneshot(request).await?;
    assert_eq!(response.status(), 401);
    let body = body_json(response).await;
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Invalid authorization header format"));

    Ok(())
}

// ─── Test 32: Complete step on nonexistent job → 404 ──────────────────

#[tokio::test]
async fn test_complete_step_nonexistent_job() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let fake_id = Uuid::new_v4();
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", fake_id),
            json!({"output": {"result": "phantom"}}),
        ))
        .await?;
    // mark_completed updates 0 rows (no error), then JobRepo::get returns None → 404
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Test 33: Wide fan-in (3 dependencies) ────────────────────────────

#[tokio::test]
async fn test_wide_fan_in_three_deps() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/wide-fan-in/execute",
            json!({"input": {}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Initial state: step1, step2, step3 = ready, step4 = pending
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let statuses: HashMap<_, _> = steps
        .iter()
        .map(|s| (s.step_name.as_str(), s.status.as_str()))
        .collect();
    assert_eq!(statuses["step1"], "ready");
    assert_eq!(statuses["step2"], "ready");
    assert_eq!(statuses["step3"], "ready");
    assert_eq!(statuses["step4"], "pending");

    // Complete step1 → step4 still pending (needs step2 + step3)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step1/complete", job_id),
            json!({"output": {"v": 1}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step4 = steps.iter().find(|s| s.step_name == "step4").unwrap();
    assert_eq!(step4.status, "pending");

    // Complete step2 → step4 still pending (needs step3)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step2/complete", job_id),
            json!({"output": {"v": 2}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step4 = steps.iter().find(|s| s.step_name == "step4").unwrap();
    assert_eq!(step4.status, "pending");

    // Complete step3 → step4 now ready (all 3 deps done)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step3/complete", job_id),
            json!({"output": {"v": 3}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step4 = steps.iter().find(|s| s.step_name == "step4").unwrap();
    assert_eq!(step4.status, "ready");

    // Complete step4 → job completed
    let response = router
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/step4/complete", job_id),
            json!({"output": {"v": 4}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test 34: Heartbeat with invalid worker ID ────────────────────────

#[tokio::test]
async fn test_heartbeat_invalid_worker_id() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/heartbeat",
            json!({"worker_id": "not-a-uuid"}),
        ))
        .await?;
    assert_eq!(response.status(), 400);

    Ok(())
}

// ─── Test 35: Action env rendering at claim time ──────────────────────

#[tokio::test]
async fn test_action_env_rendering_at_claim() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute backup-task with input host=localhost
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/backup-task/execute",
            json!({"input": {"host": "localhost"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "backup");

    // Verify action_spec env was rendered
    let action_spec = &body["action_spec"];
    let env = &action_spec["env"];
    assert_eq!(env["DB_HOST"], "localhost"); // template rendered with step input
    assert_eq!(env["STATIC_VAR"], "no-template"); // static value preserved

    Ok(())
}

// ─── Test 36: Secret reference in env ─────────────────────────────────

#[tokio::test]
async fn test_secret_reference_in_env() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute backup-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/backup-task/execute",
            json!({"input": {"host": "db.example.com"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;

    // Verify secret ref was injected into env (not resolved yet - resolution happens worker-side)
    let action_spec = &body["action_spec"];
    let env = &action_spec["env"];
    assert_eq!(env["DB_PASSWORD"], "ref+vault://secret/db#password");

    Ok(())
}

// ─── Test 36b: Nested secret reference in env at claim time ───────────

#[tokio::test]
async fn test_nested_secret_reference_in_env() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute backup-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/backup-task/execute",
            json!({"input": {"host": "db.example.com"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;

    // Verify nested secret refs were resolved through the secret object hierarchy
    let action_spec = &body["action_spec"];
    let env = &action_spec["env"];

    // {{ secret.db.password }} should resolve to the nested value
    assert_eq!(
        env["DB_PASSWORD_NESTED"],
        "ref+sops://secrets.enc.yaml#/db/password"
    );
    // {{ secret.db.host }} should resolve to the nested value
    assert_eq!(env["DB_HOST_NESTED"], "db.internal.prod");

    // Flat secret still works alongside nested
    assert_eq!(env["DB_PASSWORD"], "ref+vault://secret/db#password");

    Ok(())
}

// ─── Test 37: Cmd rendering at claim time ─────────────────────────────

#[tokio::test]
async fn test_cmd_rendering_at_claim() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute backup-task - the action has cmd: "pg_dump -h {{ input.host }}"
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/backup-task/execute",
            json!({"input": {"host": "myhost.local"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;

    // Verify cmd was rendered
    let action_spec = &body["action_spec"];
    assert_eq!(action_spec["cmd"], "pg_dump -h myhost.local");

    Ok(())
}

// ─── Test 38: Env and input rendering together ────────────────────────

#[tokio::test]
async fn test_env_and_input_rendering_together() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute backup-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/backup-task/execute",
            json!({"input": {"host": "prod-db.internal"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(&pool, worker_id, "test-worker", &["shell".to_string()]).await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "capabilities": ["shell"]}),
        ))
        .await?;
    let body = body_json(response).await;

    // Verify step input was rendered
    let input = &body["input"];
    assert_eq!(input["host"], "prod-db.internal");

    // Verify action_spec env was rendered
    let action_spec = &body["action_spec"];
    let env = &action_spec["env"];
    assert_eq!(env["DB_HOST"], "prod-db.internal");
    assert_eq!(env["DB_PASSWORD"], "ref+vault://secret/db#password");
    assert_eq!(env["STATIC_VAR"], "no-template");

    // Verify cmd was rendered
    assert_eq!(action_spec["cmd"], "pg_dump -h prod-db.internal");

    Ok(())
}

// ─── Test 39: Secret not leaked in job output ─────────────────────────

#[tokio::test]
async fn test_secret_not_leaked_in_job_output() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Execute backup-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/tasks/backup-task/execute",
            json!({"input": {"host": "db.example.com"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id = body["job_id"].as_str().unwrap();

    // Get job detail via public API
    let response = router
        .oneshot(api_get(&format!("/api/jobs/{}", job_id)))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    // The job response should contain action_spec with raw templates (not resolved secrets)
    // Since the action_spec stored in the DB has the raw templates, the public API
    // should not expose resolved secret values (those are only resolved at claim time
    // for the worker and at execution time via vals)
    let body_str = serde_json::to_string(&body).unwrap();
    // The raw vault ref should be in the stored action_spec (that's fine - it's just a ref)
    // But actual secret values should never appear
    assert!(!body_str.contains("actual-secret-password"));

    Ok(())
}

// ─── Auth helpers ─────────────────────────────────────────────────────

const AUTH_JWT_SECRET: &str = "test-jwt-secret-key";
const AUTH_REFRESH_SECRET: &str = "test-refresh-secret-key";
const AUTH_USER_EMAIL: &str = "admin@test.com";
const AUTH_USER_PASSWORD: &str = "test-password-123";

async fn setup_with_auth() -> Result<(
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
        },
        workspace: WorkspaceSourceConfig {
            source_type: "folder".to_string(),
            path: temp_dir.path().to_string_lossy().to_string(),
        },
        worker_token: "test-token-secret".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: AUTH_JWT_SECRET.to_string(),
            refresh_secret: AUTH_REFRESH_SECRET.to_string(),
            providers: HashMap::new(),
            initial_user: Some(InitialUserConfig {
                email: AUTH_USER_EMAIL.to_string(),
                password: AUTH_USER_PASSWORD.to_string(),
            }),
        }),
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

    let workspace = test_workspace();
    let state = AppState::new(pool.clone(), workspace, config);
    let router = build_router(state);

    Ok((router, pool, temp_dir, container))
}

// ─── Test 40: Login success ───────────────────────────────────────────

#[tokio::test]
async fn test_auth_login_success() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/auth/login",
            json!({"email": AUTH_USER_EMAIL, "password": AUTH_USER_PASSWORD}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert!(body["access_token"].is_string());
    assert!(body["refresh_token"].is_string());

    Ok(())
}

// ─── Test 41: Login wrong password ────────────────────────────────────

#[tokio::test]
async fn test_auth_login_wrong_password() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/auth/login",
            json!({"email": AUTH_USER_EMAIL, "password": "wrong-password"}),
        ))
        .await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test 42: Login nonexistent email ─────────────────────────────────

#[tokio::test]
async fn test_auth_login_nonexistent_email() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/auth/login",
            json!({"email": "nobody@test.com", "password": "any-password"}),
        ))
        .await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test 43: Refresh success ─────────────────────────────────────────

#[tokio::test]
async fn test_auth_refresh_success() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    // Login first
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/auth/login",
            json!({"email": AUTH_USER_EMAIL, "password": AUTH_USER_PASSWORD}),
        ))
        .await?;
    let body = body_json(response).await;
    let refresh_token = body["refresh_token"].as_str().unwrap().to_string();

    // Refresh
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/auth/refresh",
            json!({"refresh_token": refresh_token}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert!(body["access_token"].is_string());
    let new_refresh = body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(new_refresh, refresh_token); // new token issued

    // Old refresh token should be invalid now (rotation)
    let response = router
        .oneshot(api_request(
            "POST",
            "/api/auth/refresh",
            json!({"refresh_token": refresh_token}),
        ))
        .await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test 44: Refresh invalid token ───────────────────────────────────

#[tokio::test]
async fn test_auth_refresh_invalid_token() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/auth/refresh",
            json!({"refresh_token": "bogus-token"}),
        ))
        .await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test 45: Logout revokes refresh token ────────────────────────────

#[tokio::test]
async fn test_auth_logout() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    // Login
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/auth/login",
            json!({"email": AUTH_USER_EMAIL, "password": AUTH_USER_PASSWORD}),
        ))
        .await?;
    let body = body_json(response).await;
    let refresh_token = body["refresh_token"].as_str().unwrap().to_string();

    // Logout
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/auth/logout",
            json!({"refresh_token": refresh_token}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Refresh should now fail
    let response = router
        .oneshot(api_request(
            "POST",
            "/api/auth/refresh",
            json!({"refresh_token": refresh_token}),
        ))
        .await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test 46: GET /api/auth/me with valid token ───────────────────────

#[tokio::test]
async fn test_auth_me_with_valid_token() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    // Login
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/auth/login",
            json!({"email": AUTH_USER_EMAIL, "password": AUTH_USER_PASSWORD}),
        ))
        .await?;
    let body = body_json(response).await;
    let access_token = body["access_token"].as_str().unwrap().to_string();

    // GET /api/auth/me with Bearer token
    let request = Request::builder()
        .method("GET")
        .uri("/api/auth/me")
        .header("Authorization", format!("Bearer {}", access_token))
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(request).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["email"], AUTH_USER_EMAIL);

    Ok(())
}

// ─── Test 47: GET /api/auth/me without token ──────────────────────────

#[tokio::test]
async fn test_auth_me_without_token() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router.oneshot(api_get("/api/auth/me")).await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test 48: Existing API routes still work without auth ─────────────

#[tokio::test]
async fn test_existing_routes_work_without_auth() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    // /api/tasks should work without auth (no AuthUser extractor)
    let response = router.oneshot(api_get("/api/tasks")).await?;
    assert_eq!(response.status(), 200);

    Ok(())
}

// ─── Test 49: Login when auth not configured ──────────────────────────

#[tokio::test]
async fn test_login_when_auth_not_configured() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/auth/login",
            json!({"email": "any@test.com", "password": "any"}),
        ))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── WebSocket integration tests ──────────────────────────────────────

#[tokio::test]
async fn test_ws_backfill_existing_logs() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Append logs via worker API (JSONL format)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "existing line 1")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "existing line 2")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Start server on a real port for WS
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Connect via WebSocket
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream",
        addr.port(),
        job_id
    );
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect to WebSocket");

    use futures_util::StreamExt;
    // Should receive backfill (JSONL lines)
    let msg = ws_stream.next().await.unwrap()?;
    let text = msg.into_text()?;
    assert!(text.contains("existing line 1"));
    assert!(text.contains("existing line 2"));

    drop(ws_stream);
    server.abort();

    Ok(())
}

#[tokio::test]
async fn test_ws_live_log_streaming() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let router_for_push = router.clone();

    // Start server on a real port for WS
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Connect via WebSocket
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream",
        addr.port(),
        job_id
    );
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect to WebSocket");

    // Push a log chunk via worker API (through a separate router instance, JSONL format)
    let response = router_for_push
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "live line")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    use futures_util::StreamExt;
    // Should receive the live message (JSONL)
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
        .await?
        .unwrap()?;
    let text = msg.into_text()?;
    assert!(text.contains("live line"));

    drop(ws_stream);
    server.abort();

    Ok(())
}

#[tokio::test]
async fn test_ws_backfill_plus_live() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Append initial log (JSONL format)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "backfill")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let router_for_push = router.clone();

    // Start server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Connect via WebSocket
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream",
        addr.port(),
        job_id
    );
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect to WebSocket");

    use futures_util::StreamExt;

    // Should receive backfill first (JSONL)
    let msg = ws_stream.next().await.unwrap()?;
    let backfill_text = msg.into_text()?;
    assert!(backfill_text.contains("backfill"));

    // Now push a live chunk
    let response = router_for_push
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "live")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Should receive the live message (JSONL)
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
        .await?
        .unwrap()?;
    let live_text = msg.into_text()?;
    assert!(live_text.contains("live"));

    drop(ws_stream);
    server.abort();

    Ok(())
}

// ─── Test: Job output from terminal step ──────────────────────────────

#[tokio::test]
async fn test_job_output_from_terminal_step() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    // 2-step linear: step1 → step2 (step2 is terminal)
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );

    let task = TaskDef {
        mode: "distributed".to_string(),
        input: HashMap::new(),
        flow,
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete step1 with output
    JobStepRepo::mark_completed(&pool, job_id, "step1", Some(json!({"x": 1}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task).await?;

    // Complete step2 (terminal) with output
    JobStepRepo::mark_completed(&pool, job_id, "step2", Some(json!({"y": 2}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    // Job output should aggregate terminal step output: {"step2": {"y": 2}}
    assert_eq!(job.output, Some(json!({"step2": {"y": 2}})));

    Ok(())
}

// ─── Test: Job output null when terminal step has no output ───────────

#[tokio::test]
async fn test_job_output_null_when_terminal_has_no_output() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    // 2-step linear: step1 → step2 (step2 is terminal)
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );

    let task = TaskDef {
        mode: "distributed".to_string(),
        input: HashMap::new(),
        flow,
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete step1 with output
    JobStepRepo::mark_completed(&pool, job_id, "step1", Some(json!({"x": 1}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task).await?;

    // Complete step2 (terminal) with NO output
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    // Terminal step has no output → job output should be None
    assert_eq!(job.output, None);

    Ok(())
}

// ─── Test: Job output from multiple terminal steps ────────────────────

#[tokio::test]
async fn test_job_output_multiple_terminal_steps() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    // Diamond without join: step1 → step2, step1 → step3
    // step2 and step3 are both terminal
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );
    flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
        },
    );

    let task = TaskDef {
        mode: "distributed".to_string(),
        input: HashMap::new(),
        flow,
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step3".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 3"})),
            input: None,
            status: "pending".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete step1
    JobStepRepo::mark_completed(&pool, job_id, "step1", Some(json!({"x": 1}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task).await?;

    // Complete step2 (terminal) with output
    JobStepRepo::mark_completed(&pool, job_id, "step2", Some(json!({"a": 1}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task).await?;

    // Complete step3 (terminal) with output
    JobStepRepo::mark_completed(&pool, job_id, "step3", Some(json!({"b": 2}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step3", &task).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");
    // Both terminal steps have output → job output includes both
    let output = job.output.unwrap();
    assert_eq!(output["step2"], json!({"a": 1}));
    assert_eq!(output["step3"], json!({"b": 2}));

    Ok(())
}

// ─── Test: JSONL logs contain stderr stream field ──────────────────────

#[tokio::test]
async fn test_jsonl_logs_contain_stderr_stream() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Push logs with both stdout and stderr
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body(
                "build",
                &[
                    ("stdout", "compiling..."),
                    ("stderr", "warning: unused var"),
                    ("stdout", "done"),
                ],
            ),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Retrieve full logs via API
    let response = router
        .clone()
        .oneshot(api_get(&format!("/api/jobs/{}/logs", job_id)))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let logs_str = body["logs"].as_str().unwrap();

    let log_lines: Vec<Value> = logs_str
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(log_lines.len(), 3);
    assert_eq!(log_lines[0]["stream"], "stdout");
    assert_eq!(log_lines[1]["stream"], "stderr");
    assert_eq!(log_lines[1]["line"], "warning: unused var");
    assert_eq!(log_lines[2]["stream"], "stdout");

    // Retrieve step-filtered logs
    let response = router
        .oneshot(api_get(&format!("/api/jobs/{}/steps/build/logs", job_id)))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let step_logs: Vec<Value> = body["logs"]
        .as_str()
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(step_logs.len(), 3);
    assert_eq!(step_logs[0]["step"], "build");

    Ok(())
}

// ─── Test: Failing job status and error message ───────────────────────

#[tokio::test]
async fn test_failing_job_status_with_jsonl_logs() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    // Create a single step
    let steps = vec![NewJobStep {
        job_id,
        step_name: "doom".to_string(),
        action_name: "greet".to_string(),
        action_type: "shell".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "exit 1"})),
        input: None,
        status: "ready".to_string(),
    }];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Push stderr logs before step failure
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body(
                "doom",
                &[("stdout", "About to fail"), ("stderr", "Error details")],
            ),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Simulate worker completing step with failure
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/doom/complete", job_id),
            json!({"exit_code": 1, "error": "Exit code: 1\nStderr: Error details"}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Check step is failed with error
    let all_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(all_steps.len(), 1);
    assert_eq!(all_steps[0].status, "failed");
    assert!(all_steps[0]
        .error_message
        .as_ref()
        .unwrap()
        .contains("Error details"));

    // Check job is failed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    // Check logs contain stderr
    let response = router
        .oneshot(api_get(&format!("/api/jobs/{}/steps/doom/logs", job_id)))
        .await?;
    let body = body_json(response).await;
    let logs_str = body["logs"].as_str().unwrap();
    let log_lines: Vec<Value> = logs_str
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(log_lines.iter().any(|l| l["stream"] == "stderr"));

    Ok(())
}

// ─── Test: Fail in chain stops job, first step OK ─────────────────────

#[tokio::test]
async fn test_fail_in_chain_stops_job() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let task = {
        let mut flow = HashMap::new();
        flow.insert(
            "step-ok".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        flow.insert(
            "step-fail".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec!["step-ok".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        TaskDef {
            mode: "distributed".to_string(),
            input: HashMap::new(),
            flow,
        }
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "greet-and-shout",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step-ok".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "ready".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step-fail".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "pending".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete first step successfully
    JobStepRepo::mark_running(&pool, job_id, "step-ok", Uuid::new_v4()).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step-ok", Some(json!({"result": "ok"}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step-ok", &task).await?;

    // Verify step-fail was promoted to ready
    let mid_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let fail_step = mid_steps
        .iter()
        .find(|s| s.step_name == "step-fail")
        .unwrap();
    assert_eq!(fail_step.status, "ready");

    // Fail the second step
    JobStepRepo::mark_running(&pool, job_id, "step-fail", Uuid::new_v4()).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step-fail", "Exit code: 1\nStderr: boom").await?;
    orchestrator::on_step_completed(&pool, job_id, "step-fail", &task).await?;

    // Check final state
    let final_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let ok_step = final_steps
        .iter()
        .find(|s| s.step_name == "step-ok")
        .unwrap();
    let fail_step = final_steps
        .iter()
        .find(|s| s.step_name == "step-fail")
        .unwrap();
    assert_eq!(ok_step.status, "completed");
    assert_eq!(fail_step.status, "failed");
    assert!(fail_step.error_message.as_ref().unwrap().contains("boom"));

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test: Step failure skips dependents ───────────────────────────────

#[tokio::test]
async fn test_step_failure_skips_dependents() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let task = {
        let mut flow = HashMap::new();
        flow.insert(
            "step1".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        flow.insert(
            "step2".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec!["step1".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        TaskDef {
            mode: "distributed".to_string(),
            input: HashMap::new(),
            flow,
        }
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "pending".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Fail step1
    JobStepRepo::mark_running(&pool, job_id, "step1", Uuid::new_v4()).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task).await?;

    // step2 should be skipped
    let final_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step2 = final_steps.iter().find(|s| s.step_name == "step2").unwrap();
    assert_eq!(step2.status, "skipped");

    // Job should be failed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test: continue_on_failure promotes after fail ────────────────────

#[tokio::test]
async fn test_continue_on_failure_promotes_after_fail() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let task = {
        let mut flow = HashMap::new();
        flow.insert(
            "step1".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        flow.insert(
            "step2".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec!["step1".to_string()],
                input: HashMap::new(),
                continue_on_failure: true,
            },
        );
        TaskDef {
            mode: "distributed".to_string(),
            input: HashMap::new(),
            flow,
        }
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "pending".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Fail step1
    JobStepRepo::mark_running(&pool, job_id, "step1", Uuid::new_v4()).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task).await?;

    // step2 should be promoted to ready (continue_on_failure = true)
    let mid_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step2 = mid_steps.iter().find(|s| s.step_name == "step2").unwrap();
    assert_eq!(step2.status, "ready");

    // Complete step2 successfully
    JobStepRepo::mark_running(&pool, job_id, "step2", Uuid::new_v4()).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task).await?;

    // Job should be failed (step1 failed)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test: Cascading skip ─────────────────────────────────────────────

#[tokio::test]
async fn test_cascading_skip() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let task = {
        let mut flow = HashMap::new();
        flow.insert(
            "step1".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        flow.insert(
            "step2".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec!["step1".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        flow.insert(
            "step3".to_string(),
            FlowStep {
                action: "greet".to_string(),
                depends_on: vec!["step2".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        TaskDef {
            mode: "distributed".to_string(),
            input: HashMap::new(),
            flow,
        }
    };

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        None,
        "api",
        None,
    )
    .await?;

    let steps = vec![
        NewJobStep {
            job_id,
            step_name: "step1".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "pending".to_string(),
        },
        NewJobStep {
            job_id,
            step_name: "step3".to_string(),
            action_name: "greet".to_string(),
            action_type: "shell".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "pending".to_string(),
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Fail step1
    JobStepRepo::mark_running(&pool, job_id, "step1", Uuid::new_v4()).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task).await?;

    // Both step2 and step3 should be skipped (cascading)
    let final_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step2 = final_steps.iter().find(|s| s.step_name == "step2").unwrap();
    let step3 = final_steps.iter().find(|s| s.step_name == "step3").unwrap();
    assert_eq!(step2.status, "skipped");
    assert_eq!(step3.status, "skipped");

    // Job should be failed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}
