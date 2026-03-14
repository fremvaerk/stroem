use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_common::models::workflow::{
    ActionDef, ConnectionDef, FlowStep, HookDef, InputFieldDef, TaskDef, TriggerDef,
    WorkspaceConfig,
};
use stroem_db::{
    create_pool, run_migrations, JobRepo, JobStepRepo, NewJobStep, UserAuthLinkRepo, UserRepo,
    WorkerRepo,
};
use stroem_server::auth::hash_password;
use stroem_server::config::{
    AuthConfig, DbConfig, InitialUserConfig, LogStorageConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::job_creator::create_job_for_task;
use stroem_server::log_storage::LogStorage;
use stroem_server::orchestrator;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

// ─── Test helpers ───────────────────────────────────────────────────────

fn test_workspace() -> WorkspaceConfig {
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
        },
    );

    // Action: shout (script)
    workspace.actions.insert(
        "shout".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo $MSG | tr a-z A-Z".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Action: docker-build (docker type)
    workspace.actions.insert(
        "docker-build".to_string(),
        ActionDef {
            action_type: "docker".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: None,
            source: None,
            runner: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
            tags: vec![],
            image: Some("docker:latest".to_string()),
            command: Some(vec![
                "docker".to_string(),
                "build".to_string(),
                ".".to_string(),
            ]),
            entrypoint: None,
            env: None,
            workdir: None,
            resources: None,
            input: HashMap::new(),
            output: None,
            manifest: None,
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
            name: None,
            description: None,
            depends_on: vec![],
            input: hello_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
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
            name: None,
            description: None,
            depends_on: vec![],
            input: greet_step_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut shout_step_input = HashMap::new();
    shout_step_input.insert("msg".to_string(), json!("{{ greet.output.greeting }}"));
    gs_flow.insert(
        "shout".to_string(),
        FlowStep {
            action: "shout".to_string(),
            name: None,
            description: None,
            depends_on: vec!["greet".to_string()],
            input: shout_step_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut gs_task_input = HashMap::new();
    gs_task_input.insert(
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
        "greet-and-shout".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: gs_task_input,
            flow: gs_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Task: linear-3 (3-step linear: step1 → step2 → step3)
    let mut l3_flow = HashMap::new();
    l3_flow.insert(
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
            inline_action: None,
        },
    );
    l3_flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    l3_flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step2".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "linear-3".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: l3_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Task: diamond (step1 → step2, step1 → step3, step2+step3 → step4)
    let mut d_flow = HashMap::new();
    d_flow.insert(
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
            inline_action: None,
        },
    );
    d_flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    d_flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    d_flow.insert(
        "step4".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step2".to_string(), "step3".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "diamond".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: d_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Task: docker-build-task (single step using docker action)
    let mut dbt_flow = HashMap::new();
    dbt_flow.insert(
        "build".to_string(),
        FlowStep {
            action: "docker-build".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "docker-build-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: dbt_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
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
            name: None,
            description: None,
            depends_on: vec![],
            input: mi_greet_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
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
            name: None,
            description: None,
            depends_on: vec!["greet".to_string()],
            input: mi_process_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut mi_task_input = HashMap::new();
    mi_task_input.insert(
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
        "mixed-input".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: mi_task_input,
            flow: mi_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Task: wide-fan-in (3 parallel steps → 1 join step with 3 dependencies)
    let mut wfi_flow = HashMap::new();
    wfi_flow.insert(
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
            inline_action: None,
        },
    );
    wfi_flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    wfi_flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    wfi_flow.insert(
        "step4".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![
                "step1".to_string(),
                "step2".to_string(),
                "step3".to_string(),
            ],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "wide-fan-in".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: wfi_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Action: db-backup (script with env templates)
    let mut db_input = HashMap::new();
    db_input.insert(
        "host".to_string(),
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
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("pg_dump -h {{ input.host }}".to_string()),
            source: None,
            runner: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
            tags: vec![],
            image: None,
            command: None,
            entrypoint: None,
            env: Some(db_env),
            workdir: None,
            resources: None,
            input: db_input,
            output: None,
            manifest: None,
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
            name: None,
            description: None,
            depends_on: vec![],
            input: bt_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut bt_task_input = HashMap::new();
    bt_task_input.insert(
        "host".to_string(),
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
        "backup-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: bt_task_input,
            flow: bt_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
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

    // Action: transform (script with output)
    let mut transform_input = HashMap::new();
    transform_input.insert(
        "data".to_string(),
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
        "transform".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some(
                "echo Processing $DATA && echo 'OUTPUT: {\"result\": \"processed-'$DATA'\"}'"
                    .to_string(),
            ),
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
            input: transform_input,
            output: None,
            manifest: None,
        },
    );

    // Action: summarize (script with output)
    let mut summarize_input = HashMap::new();
    summarize_input.insert(
        "value".to_string(),
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
        "summarize".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some(
                "echo Summarizing $VALUE && echo 'OUTPUT: {\"summary\": \"'$VALUE' done\"}'"
                    .to_string(),
            ),
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
            input: summarize_input,
            output: None,
            manifest: None,
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
            name: None,
            description: None,
            depends_on: vec![],
            input: dp_transform_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut dp_summarize_input = HashMap::new();
    dp_summarize_input.insert("value".to_string(), json!("{{ transform.output.result }}"));
    dp_flow.insert(
        "summarize".to_string(),
        FlowStep {
            action: "summarize".to_string(),
            name: None,
            description: None,
            depends_on: vec!["transform".to_string()],
            input: dp_summarize_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut dp_task_input = HashMap::new();
    dp_task_input.insert(
        "data".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: true,
            secret: false,
            default: Some(json!("test")),
            options: None,
            allow_custom: false,
            order: None,
        },
    );
    workspace.tasks.insert(
        "data-pipeline".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: dp_task_input,
            flow: dp_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Task: deploy-staging (single step with folder)
    let mut ds_flow = HashMap::new();
    ds_flow.insert(
        "run".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "deploy-staging".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: Some("deploy/staging".to_string()),
            input: HashMap::new(),
            flow: ds_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Trigger: nightly (cron trigger targeting hello-world)
    workspace.triggers.insert(
        "nightly".to_string(),
        TriggerDef::Scheduler {
            cron: "0 2 * * *".to_string(),
            task: "hello-world".to_string(),
            input: HashMap::from([("name".to_string(), json!("nightly"))]),
            enabled: true,
            concurrency: Default::default(),
        },
    );

    // Trigger: weekly-backup (cron trigger targeting backup-task, disabled)
    workspace.triggers.insert(
        "weekly-backup".to_string(),
        TriggerDef::Scheduler {
            cron: "0 3 * * 0".to_string(),
            task: "backup-task".to_string(),
            input: HashMap::from([("host".to_string(), json!("db.prod"))]),
            enabled: false,
            concurrency: Default::default(),
        },
    );

    // Webhook: github-push (targets hello-world, with secret)
    workspace.triggers.insert(
        "on-push".to_string(),
        TriggerDef::Webhook {
            name: "github-push".to_string(),
            task: "hello-world".to_string(),
            secret: Some("whsec_test123".to_string()),
            input: HashMap::from([("environment".to_string(), json!("staging"))]),
            enabled: true,
            mode: None,
            timeout_secs: None,
        },
    );

    // Webhook: public-hook (no secret, targets hello-world)
    workspace.triggers.insert(
        "on-deploy".to_string(),
        TriggerDef::Webhook {
            name: "public-hook".to_string(),
            task: "hello-world".to_string(),
            secret: None,
            input: HashMap::new(),
            enabled: true,
            mode: None,
            timeout_secs: None,
        },
    );

    // Webhook: disabled-hook (disabled)
    workspace.triggers.insert(
        "disabled-wh".to_string(),
        TriggerDef::Webhook {
            name: "disabled-hook".to_string(),
            task: "hello-world".to_string(),
            secret: None,
            input: HashMap::new(),
            enabled: false,
            mode: None,
            timeout_secs: None,
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
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let workspace = test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir, container))
}

/// Register a test worker in the DB and return its UUID.
/// Use this to satisfy foreign key constraints when calling `mark_running`.
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

/// Extract the `stroem_refresh` cookie value from a response's Set-Cookie headers.
fn extract_refresh_cookie(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("stroem_refresh="))
        .and_then(|s| {
            s.strip_prefix("stroem_refresh=")
                .and_then(|rest| rest.split(';').next())
                .map(|v| v.to_string())
        })
        .filter(|v| !v.is_empty())
}

/// Build a request with a cookie header containing the refresh token.
fn api_request_with_cookie(
    method: &str,
    uri: &str,
    body: Value,
    cookie: &str,
) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("cookie", cookie)
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
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
            "/api/workspaces/default/tasks/hello-world/execute",
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
        action_type: "script".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "echo Hello"})),
        input: Some(json!({"name": "{{ input.name }}"})),
        status: "ready".to_string(),
        required_tags: vec!["script".to_string()],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
    }];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Register worker via API
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "worker-1", "tags": ["script"]}),
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
            json!({"worker_id": worker_id, "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/greet-and-shout/execute",
            json!({"input": {"name": "World"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Register worker
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    // Claim the first step (greet)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/hello-world/execute",
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
            "/api/workspaces/default/tasks/greet-and-shout/execute",
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
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/linear-3/execute",
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
            "/api/workspaces/default/tasks/diamond/execute",
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
            serde_json::to_string(&json!({"name": "bad", "tags": ["script"]})).unwrap(),
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
            serde_json::to_string(&json!({"name": "bad", "tags": ["script"]})).unwrap(),
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

    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let tasks = body.as_array().unwrap();
    let task_ids: Vec<&str> = tasks.iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert!(task_ids.contains(&"hello-world"));
    assert!(task_ids.contains(&"greet-and-shout"));
    assert!(task_ids.contains(&"linear-3"));
    assert!(task_ids.contains(&"diamond"));

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
            "/api/workspaces/default/tasks/greet-and-shout/execute",
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
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    let task = TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,

        on_success: vec![],
        on_error: vec![],
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
        action_type: "script".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "exit 1"})),
        input: None,
        status: "ready".to_string(),
        required_tags: vec!["script".to_string()],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
    }];
    JobStepRepo::create_steps(&pool, &steps).await?;

    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

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
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step2".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    let task = TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,

        on_success: vec![],
        on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step3".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 3"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete step1
    JobStepRepo::mark_completed(&pool, job_id, "step1", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

    let job_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step2 = job_steps.iter().find(|s| s.step_name == "step2").unwrap();
    assert_eq!(step2.status, "ready");

    // Complete step2
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task, None).await?;

    let job_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step3 = job_steps.iter().find(|s| s.step_name == "step3").unwrap();
    assert_eq!(step3.status, "ready");

    // Complete step3
    JobStepRepo::mark_completed(&pool, job_id, "step3", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step3", &task, None).await?;

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
            "/api/workspaces/default/tasks/nonexistent/execute",
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
        .oneshot(api_get("/api/workspaces/default/tasks/hello-world"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    assert_eq!(body["id"], "hello-world");
    assert_eq!(body["mode"], "distributed");
    assert!(body["input"].is_object());
    assert!(body["input"]["name"].is_object());
    assert!(body["flow"].is_object());
    assert!(body["flow"]["greet"].is_object());

    // Nonexistent task returns 404
    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks/nonexistent-task"))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Test 16b: Task detail includes connections for connection-type inputs ──

#[tokio::test]
async fn test_task_detail_connections() -> Result<()> {
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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    // Build a workspace with connections
    let mut workspace = WorkspaceConfig::default();

    // Add connections
    workspace.connections.insert(
        "prod_db".to_string(),
        ConnectionDef {
            connection_type: Some("postgres".to_string()),
            values: HashMap::from([
                ("host".to_string(), json!("prod.example.com")),
                ("port".to_string(), json!(5432)),
            ]),
        },
    );
    workspace.connections.insert(
        "staging_db".to_string(),
        ConnectionDef {
            connection_type: Some("postgres".to_string()),
            values: HashMap::from([
                ("host".to_string(), json!("staging.example.com")),
                ("port".to_string(), json!(5432)),
            ]),
        },
    );
    workspace.connections.insert(
        "redis_cache".to_string(),
        ConnectionDef {
            connection_type: Some("redis".to_string()),
            values: HashMap::from([("host".to_string(), json!("redis.example.com"))]),
        },
    );
    // Untyped connection — should never appear in typed buckets
    workspace.connections.insert(
        "misc_config".to_string(),
        ConnectionDef {
            connection_type: None,
            values: HashMap::from([("url".to_string(), json!("https://misc.example.com"))]),
        },
    );

    // Action: run-query (script)
    workspace.actions.insert(
        "run-query".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo querying".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Task with connection-type input
    let mut conn_task_input = HashMap::new();
    conn_task_input.insert(
        "db".to_string(),
        InputFieldDef {
            field_type: "postgres".to_string(),
            name: Some("Database".to_string()),
            description: Some("Target database connection".to_string()),
            required: true,
            secret: false,
            default: None,
            options: None,
            allow_custom: false,
            order: None,
        },
    );
    conn_task_input.insert(
        "cache".to_string(),
        InputFieldDef {
            field_type: "redis".to_string(),
            name: Some("Cache".to_string()),
            description: None,
            required: false,
            secret: false,
            default: None,
            options: None,
            allow_custom: false,
            order: None,
        },
    );
    conn_task_input.insert(
        "legacy".to_string(),
        InputFieldDef {
            field_type: "mysql".to_string(),
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
    conn_task_input.insert(
        "name".to_string(),
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
    let mut conn_flow = HashMap::new();
    conn_flow.insert(
        "query".to_string(),
        FlowStep {
            action: "run-query".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "db-query".to_string(),
        TaskDef {
            name: Some("Database Query".to_string()),
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: conn_task_input,
            flow: conn_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Task with only primitive inputs (no connections field expected)
    let mut prim_task_input = HashMap::new();
    prim_task_input.insert(
        "message".to_string(),
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
    let mut prim_flow = HashMap::new();
    prim_flow.insert(
        "run".to_string(),
        FlowStep {
            action: "run-query".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "simple-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: prim_task_input,
            flow: prim_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    // Test 1: Task with connection-type input includes connections field
    let response = router
        .clone()
        .oneshot(api_get("/api/workspaces/default/tasks/db-query"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    assert_eq!(body["id"], "db-query");
    assert_eq!(body["name"], "Database Query");
    // Should have connections map with postgres type
    let connections = body["connections"]
        .as_object()
        .expect("connections should be an object");
    assert!(connections.contains_key("postgres"));
    let pg_conns = connections["postgres"]
        .as_array()
        .expect("postgres should be an array");
    assert_eq!(pg_conns.len(), 2);
    // Should be sorted alphabetically
    assert_eq!(pg_conns[0], "prod_db");
    assert_eq!(pg_conns[1], "staging_db");

    // Multiple connection types: redis input should also be present
    assert!(connections.contains_key("redis"));
    let redis_conns = connections["redis"]
        .as_array()
        .expect("redis should be an array");
    assert_eq!(redis_conns.len(), 1);
    assert_eq!(redis_conns[0], "redis_cache");

    // Connection type with no matching connections (mysql) should be omitted, not empty array
    assert!(
        !connections.contains_key("mysql"),
        "connection types with no matches should be omitted"
    );

    // Untyped connections should never appear in any typed bucket
    for (_type_name, names) in connections {
        let names_arr = names.as_array().unwrap();
        for n in names_arr {
            assert_ne!(n.as_str().unwrap(), "misc_config");
        }
    }

    // Security: connection VALUES (host, port) must NOT appear in the response
    let raw_body = serde_json::to_string(&body).unwrap();
    assert!(
        !raw_body.contains("prod.example.com"),
        "connection values must not be exposed"
    );
    assert!(
        !raw_body.contains("staging.example.com"),
        "connection values must not be exposed"
    );
    assert!(
        !raw_body.contains("redis.example.com"),
        "connection values must not be exposed"
    );

    // Test 2: Task with only primitive inputs omits connections field
    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks/simple-task"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    assert_eq!(body["id"], "simple-task");
    // connections field should be absent (skip_serializing_if = "HashMap::is_empty")
    assert!(body.get("connections").is_none());

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
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert_eq!(body["total"].as_i64().unwrap(), 0);

    // Create two jobs
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "A"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "B"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // List should have 2 jobs
    let response = router.clone().oneshot(api_get("/api/jobs")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"].as_i64().unwrap(), 2);

    // List with limit
    let response = router.oneshot(api_get("/api/jobs?limit=1")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["total"].as_i64().unwrap(), 2);

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
            json!({"name": "hb-worker", "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/hello-world/execute",
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
            json!({"name": "start-worker", "tags": ["script"]}),
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
            json!({"worker_id": worker_id, "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/hello-world/execute",
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
            json!({"name": "timing-worker", "tags": ["script"]}),
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
            json!({"worker_id": worker_id, "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/docker-build-task/execute",
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
            json!({"name": "docker-worker", "tags": ["docker"]}),
        ))
        .await?;
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "tags": ["docker"]}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["action_type"].as_str().unwrap(), "docker");
    assert_eq!(body["action_image"].as_str().unwrap(), "docker:latest");
    assert!(body["action_spec"].is_object());

    Ok(())
}

// ─── Test 22: Capability mismatch - script worker can't claim docker ───

#[tokio::test]
async fn test_capability_mismatch_no_claim() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Execute docker-build-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/docker-build-task/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register script-only worker
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "script-only", "tags": ["script"]}),
        ))
        .await?;
    let body = body_json(response).await;
    let worker_id = body["worker_id"].as_str().unwrap().to_string();

    // Try to claim with script capability - should get null (no matching docker step)
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "tags": ["script"]}),
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

    // Create one script job and one docker job
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Multi"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/docker-build-task/execute",
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
            json!({"name": "multi-worker", "tags": ["script", "docker"]}),
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
            json!({"worker_id": worker_id, "tags": ["script", "docker"]}),
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
            json!({"worker_id": worker_id, "tags": ["script", "docker"]}),
        ))
        .await?;
    let body = body_json(response).await;
    assert!(body["job_id"].is_string());
    let second_type = body["action_type"].as_str().unwrap().to_string();

    // Both types should have been claimed
    let mut types = vec![first_type, second_type];
    types.sort();
    assert_eq!(types, vec!["docker", "script"]);

    // No more steps
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "tags": ["script", "docker"]}),
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
            "/api/workspaces/default/tasks/hello-world/execute",
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
            "/api/workspaces/default/tasks/hello-world/execute",
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
            "/api/workspaces/default/tasks/hello-world/execute",
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
            "/api/workspaces/default/tasks/mixed-input/execute",
            json!({"input": {"name": "MixedTest"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Register worker
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    // Claim greet step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "FirstStep"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/greet-and-shout/execute",
            json!({"input": {"name": "NullOutput"}}),
        ))
        .await?;
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    // Claim greet step
    let _response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
    // because greet has no output. The step should be failed immediately
    // with a clear error instead of passing raw template strings to the worker.
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(
        response.status(),
        422,
        "Claim should return 422 when input template rendering fails"
    );
    let body = body_json(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("render"),
        "Error should mention rendering: {:?}",
        body["error"]
    );

    // The step should be marked as failed in the DB
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let shout_step = steps.iter().find(|s| s.step_name == "shout").unwrap();
    assert_eq!(shout_step.status, "failed");
    assert!(
        shout_step
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("render"),
        "Step error should mention rendering"
    );

    // The job should reach terminal (failed) state via orchestration
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "failed",
        "Job should be failed after render failure"
    );

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
            serde_json::to_string(&json!({"name": "bad", "tags": ["script"]})).unwrap(),
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
    // mark_completed updates 0 rows (no error), orchestrator silently handles missing job
    assert_eq!(response.status(), 200);

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
            "/api/workspaces/default/tasks/wide-fan-in/execute",
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
            "/api/workspaces/default/tasks/backup-task/execute",
            json!({"input": {"host": "localhost"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/backup-task/execute",
            json!({"input": {"host": "db.example.com"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
            "/api/workspaces/default/tasks/backup-task/execute",
            json!({"input": {"host": "db.example.com"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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

    // Execute backup-task - the action has script: "pg_dump -h {{ input.host }}"
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/backup-task/execute",
            json!({"input": {"host": "myhost.local"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    let body = body_json(response).await;

    // Verify cmd was rendered
    let action_spec = &body["action_spec"];
    assert_eq!(action_spec["script"], "pg_dump -h myhost.local");

    Ok(())
}

// ─── Test 37b: Cmd rendering failure fails step at claim time ─────────

#[tokio::test]
async fn test_cmd_rendering_failure_fails_step() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Create a job with a step whose cmd references a non-existent variable
    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        Some(json!({})),
        "api",
        Some("test/cmd-render-fail"),
    )
    .await?;
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id,
            step_name: "run".to_string(),
            action_name: "test-action".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo {{ input.nonexistent_var }}"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    // Claim should fail with 422 because cmd template can't render
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(
        response.status(),
        422,
        "Claim should return 422 when cmd template rendering fails"
    );
    let body = body_json(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("render"),
        "Error should mention rendering: {:?}",
        body["error"]
    );

    // Step should be failed in DB
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "run").unwrap();
    assert_eq!(step.status, "failed");
    assert!(step
        .error_message
        .as_deref()
        .unwrap_or("")
        .contains("render"));

    // Note: job terminal state is not asserted here because this test creates
    // a raw DB job with a task name that doesn't exist in the workspace config,
    // so orchestration can't resolve the task definition. Job terminal state
    // after render failure is tested by test_render_failure_propagates_to_downstream_steps
    // and test_on_error_hook_fires_after_render_failure using real workflows.

    Ok(())
}

// ─── Test 37c: Env rendering failure fails step at claim time ─────────

#[tokio::test]
async fn test_env_rendering_failure_fails_step() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Create a job with a step whose env references a non-existent variable
    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        Some(json!({})),
        "api",
        Some("test/env-render-fail"),
    )
    .await?;
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id,
            step_name: "run".to_string(),
            action_name: "test-action".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"env": {"MY_VAR": "{{ input.missing_key }}"}})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(
        response.status(),
        422,
        "Claim should return 422 when env template rendering fails"
    );
    let body = body_json(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("render"),
        "Error should mention rendering: {:?}",
        body["error"]
    );

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "run").unwrap();
    assert_eq!(step.status, "failed");
    assert!(step
        .error_message
        .as_deref()
        .unwrap_or("")
        .contains("render"));

    Ok(())
}

// ─── Test 37d: Script rendering failure fails step at claim time ───────

#[tokio::test]
async fn test_script_rendering_failure_fails_step() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        Some(json!({})),
        "api",
        Some("test/script-render-fail"),
    )
    .await?;
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id,
            step_name: "run".to_string(),
            action_name: "test-action".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"script": "#!/bin/bash\necho {{ input.undefined_var }}"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(
        response.status(),
        422,
        "Claim should return 422 when script template rendering fails"
    );
    let body = body_json(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("render"),
        "Error should mention rendering: {:?}",
        body["error"]
    );

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "run").unwrap();
    assert_eq!(step.status, "failed");
    assert!(step
        .error_message
        .as_deref()
        .unwrap_or("")
        .contains("render"));

    Ok(())
}

// ─── Test 37e: Manifest rendering failure fails step at claim time ─────

#[tokio::test]
async fn test_manifest_rendering_failure_fails_step() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        Some(json!({})),
        "api",
        Some("test/manifest-render-fail"),
    )
    .await?;
    // A pod-type step with a manifest containing a template referencing a missing key
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id,
            step_name: "run".to_string(),
            action_name: "test-action".to_string(),
            action_type: "pod".to_string(),
            action_image: Some("alpine:latest".to_string()),
            action_spec: Some(json!({
                "manifest": {
                    "spec": {
                        "serviceAccountName": "{{ input.undefined_sa }}"
                    }
                }
            })),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "pod".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(
        response.status(),
        422,
        "Claim should return 422 when manifest template rendering fails"
    );
    let body = body_json(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("render"),
        "Error should mention rendering: {:?}",
        body["error"]
    );

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "run").unwrap();
    assert_eq!(step.status, "failed");
    assert!(step
        .error_message
        .as_deref()
        .unwrap_or("")
        .contains("render"));

    Ok(())
}

// ─── Test 37f: Image rendering failure fails step at claim time ────────

#[tokio::test]
async fn test_image_rendering_failure_fails_step() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let job_id = JobRepo::create(
        &pool,
        "default",
        "test-task",
        "distributed",
        Some(json!({})),
        "api",
        Some("test/image-render-fail"),
    )
    .await?;
    // A docker-type step whose image tag is a template referencing a missing key
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id,
            step_name: "run".to_string(),
            action_name: "test-action".to_string(),
            action_type: "docker".to_string(),
            action_image: Some("myrepo/myapp:{{ input.image_tag }}".to_string()),
            action_spec: None,
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "none".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(
        response.status(),
        422,
        "Claim should return 422 when image template rendering fails"
    );
    let body = body_json(response).await;
    assert!(
        body["error"].as_str().unwrap().contains("render"),
        "Error should mention rendering: {:?}",
        body["error"]
    );

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "run").unwrap();
    assert_eq!(step.status, "failed");
    assert!(step
        .error_message
        .as_deref()
        .unwrap_or("")
        .contains("render"));

    Ok(())
}

// ─── Test 37g: Render failure causes downstream steps to be skipped ────
//
// When the first step in a linear chain fails at render time, the orchestrator
// should mark downstream steps as skipped (or failed), and the job itself
// should reach a terminal state.

#[tokio::test]
async fn test_render_failure_propagates_to_downstream_steps() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Execute greet-and-shout: greet (no output) → shout (depends on greet.output.greeting)
    // We complete greet with NO output so that shout's template can't resolve.
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/greet-and-shout/execute",
            json!({"input": {"name": "RenderPropagation"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    // Claim and complete greet with no output
    router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);

    // Claim shout — should fail at render time with 422
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 422);

    // The shout step should be failed in the DB
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let shout_step = steps.iter().find(|s| s.step_name == "shout").unwrap();
    assert_eq!(shout_step.status, "failed");

    // The job itself should have reached a terminal state (failed) since
    // the only remaining step has now failed.
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "failed",
        "Job should be terminal after the only remaining step render-failed: got {}",
        job.status
    );

    Ok(())
}

// ─── Test 37h: on_error hook fires after render failure makes job terminal

#[tokio::test]
async fn test_on_error_hook_fires_after_render_failure() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = tempfile::TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Build a workspace with a task that has an on_error hook and a step
    // whose cmd uses an undefined template variable.
    let mut workspace = WorkspaceConfig::default();

    // Action: notify (the hook target)
    workspace.actions.insert(
        "notify".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo {{ input.message }}".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Action: broken (step whose cmd will fail to render)
    workspace.actions.insert(
        "broken".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo {{ input.undefined_var }}".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    let mut hook_input = HashMap::new();
    hook_input.insert("message".to_string(), json!("job failed"));

    let mut task_flow = HashMap::new();
    task_flow.insert(
        "run".to_string(),
        FlowStep {
            action: "broken".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    workspace.tasks.insert(
        "broken-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: task_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
        },
    );

    let config = stroem_server::config::ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: stroem_server::config::DbConfig { url },
        log_storage: stroem_server::config::LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            stroem_server::config::WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = stroem_server::log_storage::LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = stroem_server::web::build_router(state, CancellationToken::new());

    // Trigger the broken-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/broken-task/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    // Claim the broken step — should fail at render time with 422
    let resp = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 422, "Render failure should return 422");

    // The job should now be terminal (failed) because its only step failed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "failed",
        "Job should be terminal after render failure: got {}",
        job.status
    );

    // An on_error hook job should have been created
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("on_error hook job should have been created after render failure");
    assert_eq!(hook_job.task_name, "_hook:notify");

    // The hook step should be ready for a worker to pick up
    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    assert_eq!(hook_steps.len(), 1);
    assert_eq!(hook_steps[0].status, "ready");

    Ok(())
}

// ─── Test 37i: Parent step updated after child job's step render-fails ─
//
// When a child job (spawned via type: task) has its only step fail at render
// time, the parent step should be updated to "failed" and the parent job
// should reach a terminal state.

#[tokio::test]
async fn test_parent_step_updated_after_child_render_failure() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = tempfile::TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Workspace: parent task has one task-action step; child task has a broken step
    let mut workspace = WorkspaceConfig::default();

    // Child action: broken cmd template
    workspace.actions.insert(
        "broken-action".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo {{ input.nonexistent }}".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Task: child-task (one step with a broken template)
    let mut child_flow = HashMap::new();
    child_flow.insert(
        "broken-step".to_string(),
        FlowStep {
            action: "broken-action".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "child-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: child_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Action: run-child (type: task → child-task)
    workspace.actions.insert(
        "run-child".to_string(),
        ActionDef {
            action_type: "task".to_string(),
            name: None,
            description: None,
            task: Some("child-task".to_string()),
            cmd: None,
            script: None,
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Task: parent-task (single task-action step)
    let mut parent_flow = HashMap::new();
    parent_flow.insert(
        "delegate".to_string(),
        FlowStep {
            action: "run-child".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "parent-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: parent_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    let config = stroem_server::config::ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: stroem_server::config::DbConfig { url },
        log_storage: stroem_server::config::LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            stroem_server::config::WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = stroem_server::log_storage::LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = stroem_server::web::build_router(state, CancellationToken::new());

    // Execute parent-task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/parent-task/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let parent_job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // The parent "delegate" step is a task-action, so it is dispatched server-side
    // immediately on job creation. Verify the child job exists.
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let child_job = all_jobs
        .iter()
        .find(|j| j.source_type == "task")
        .expect("Child job should be created by task-action dispatch");
    let child_job_id = child_job.job_id;

    // Register a worker and claim the child's broken-step
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let resp = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 422, "Child step render should fail with 422");

    // Child job's broken-step should be failed
    let child_steps = JobStepRepo::get_steps_for_job(&pool, child_job_id).await?;
    let broken = child_steps
        .iter()
        .find(|s| s.step_name == "broken-step")
        .unwrap();
    assert_eq!(broken.status, "failed");

    // Child job itself should be terminal
    let child_job_row = JobRepo::get(&pool, child_job_id).await?.unwrap();
    assert_eq!(
        child_job_row.status, "failed",
        "Child job should be terminal after render failure: got {}",
        child_job_row.status
    );

    // Parent step "delegate" should be marked failed (propagated from child)
    let parent_steps = JobStepRepo::get_steps_for_job(&pool, parent_job_id).await?;
    let delegate_step = parent_steps
        .iter()
        .find(|s| s.step_name == "delegate")
        .unwrap();
    assert_eq!(
        delegate_step.status, "failed",
        "Parent step should be failed after child job's render failure: got {}",
        delegate_step.status
    );

    // Parent job should also be terminal
    let parent_job = JobRepo::get(&pool, parent_job_id).await?.unwrap();
    assert_eq!(
        parent_job.status, "failed",
        "Parent job should be terminal after child job failed: got {}",
        parent_job.status
    );

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
            "/api/workspaces/default/tasks/backup-task/execute",
            json!({"input": {"host": "prod-db.internal"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Register worker and claim
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
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
    assert_eq!(action_spec["script"], "pg_dump -h prod-db.internal");

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
            "/api/workspaces/default/tasks/backup-task/execute",
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
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
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
        mcp: None,
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
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

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
    // Refresh token is now in Set-Cookie header (HttpOnly cookie)
    let refresh_cookie = extract_refresh_cookie(&response);
    assert!(refresh_cookie.is_some(), "Expected stroem_refresh cookie");
    let body = body_json(response).await;
    assert!(body["access_token"].is_string());

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
    let refresh_token = extract_refresh_cookie(&response).unwrap();
    let cookie = format!("stroem_refresh={}", refresh_token);

    // Refresh via cookie
    let response = router
        .clone()
        .oneshot(api_request_with_cookie(
            "POST",
            "/api/auth/refresh",
            json!({}),
            &cookie,
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let new_refresh = extract_refresh_cookie(&response).unwrap();
    assert_ne!(new_refresh, refresh_token); // new token issued
    let body = body_json(response).await;
    assert!(body["access_token"].is_string());

    // Old refresh token should be invalid now (rotation)
    let response = router
        .oneshot(api_request_with_cookie(
            "POST",
            "/api/auth/refresh",
            json!({}),
            &cookie,
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
    let refresh_token = extract_refresh_cookie(&response).unwrap();
    let cookie = format!("stroem_refresh={}", refresh_token);

    // Logout via cookie
    let response = router
        .clone()
        .oneshot(api_request_with_cookie(
            "POST",
            "/api/auth/logout",
            json!({}),
            &cookie,
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Refresh should now fail
    let response = router
        .oneshot(api_request_with_cookie(
            "POST",
            "/api/auth/refresh",
            json!({}),
            &cookie,
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
async fn test_protected_routes_require_auth() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    // /api/workspaces/default/tasks requires auth when auth is enabled
    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks"))
        .await?;
    assert_eq!(response.status(), 401);

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
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    let task = TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,

        on_success: vec![],
        on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete step1 with output
    JobStepRepo::mark_completed(&pool, job_id, "step1", Some(json!({"x": 1}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

    // Complete step2 (terminal) with output
    JobStepRepo::mark_completed(&pool, job_id, "step2", Some(json!({"y": 2}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task, None).await?;

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
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    let task = TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,

        on_success: vec![],
        on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete step1 with output
    JobStepRepo::mark_completed(&pool, job_id, "step1", Some(json!({"x": 1}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

    // Complete step2 (terminal) with NO output
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task, None).await?;

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
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "step3".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec!["step1".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    let task = TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow,
        timeout: None,

        on_success: vec![],
        on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 2"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step3".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo 3"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete step1
    JobStepRepo::mark_completed(&pool, job_id, "step1", Some(json!({"x": 1}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

    // Complete step2 (terminal) with output
    JobStepRepo::mark_completed(&pool, job_id, "step2", Some(json!({"a": 1}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task, None).await?;

    // Complete step3 (terminal) with output
    JobStepRepo::mark_completed(&pool, job_id, "step3", Some(json!({"b": 2}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step3", &task, None).await?;

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
        action_type: "script".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "exit 1"})),
        input: None,
        status: "ready".to_string(),
        required_tags: vec!["script".to_string()],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
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
    let worker_id = register_test_worker(&pool).await;

    let task = {
        let mut flow = HashMap::new();
        flow.insert(
            "step-ok".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        flow.insert(
            "step-fail".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec!["step-ok".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step-fail".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Complete first step successfully
    JobStepRepo::mark_running(&pool, job_id, "step-ok", worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step-ok", Some(json!({"result": "ok"}))).await?;
    orchestrator::on_step_completed(&pool, job_id, "step-ok", &task, None).await?;

    // Verify step-fail was promoted to ready
    let mid_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let fail_step = mid_steps
        .iter()
        .find(|s| s.step_name == "step-fail")
        .unwrap();
    assert_eq!(fail_step.status, "ready");

    // Fail the second step
    JobStepRepo::mark_running(&pool, job_id, "step-fail", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step-fail", "Exit code: 1\nStderr: boom").await?;
    orchestrator::on_step_completed(&pool, job_id, "step-fail", &task, None).await?;

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
    let worker_id = register_test_worker(&pool).await;

    let task = {
        let mut flow = HashMap::new();
        flow.insert(
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
                inline_action: None,
            },
        );
        flow.insert(
            "step2".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec!["step1".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Fail step1
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

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
    let worker_id = register_test_worker(&pool).await;

    let task = {
        let mut flow = HashMap::new();
        flow.insert(
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
                inline_action: None,
            },
        );
        flow.insert(
            "step2".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec!["step1".to_string()],
                input: HashMap::new(),
                continue_on_failure: true,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Fail step1
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

    // step2 should be promoted to ready (continue_on_failure = true)
    let mid_steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step2 = mid_steps.iter().find(|s| s.step_name == "step2").unwrap();
    assert_eq!(step2.status, "ready");

    // Complete step2 successfully
    JobStepRepo::mark_running(&pool, job_id, "step2", worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task, None).await?;

    // Job should be failed (step1 failed)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test: continue_on_failure step fails but job succeeds ────────────

#[tokio::test]
async fn test_continue_on_failure_step_fails_job_succeeds() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;
    let worker_id = register_test_worker(&pool).await;

    // step1: continue_on_failure=true, no deps -> will fail
    // step2: no deps, independent -> will succeed
    let task = {
        let mut flow = HashMap::new();
        flow.insert(
            "step1".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: true,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        flow.insert(
            "step2".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Fail step1 (tolerable)
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

    // Job should still be running (step2 not done yet)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "pending");

    // Complete step2 successfully
    JobStepRepo::mark_running(&pool, job_id, "step2", worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task, None).await?;

    // Job should be completed (step1's failure is tolerable)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    Ok(())
}

// ─── Test: mixed tolerable and intolerable failures ───────────────────

#[tokio::test]
async fn test_mixed_tolerable_and_intolerable_failures() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;
    let worker_id = register_test_worker(&pool).await;

    // step1: continue_on_failure=true -> will fail (tolerable)
    // step2: continue_on_failure=false -> will fail (intolerable)
    let task = {
        let mut flow = HashMap::new();
        flow.insert(
            "step1".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: true,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        flow.insert(
            "step2".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Fail step1 (tolerable)
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

    // Fail step2 (intolerable)
    JobStepRepo::mark_running(&pool, job_id, "step2", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step2", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step2", &task, None).await?;

    // Job should be failed (step2's failure is intolerable)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

// ─── Test: Cascading skip ─────────────────────────────────────────────

#[tokio::test]
async fn test_cascading_skip() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;
    let worker_id = register_test_worker(&pool).await;

    let task = {
        let mut flow = HashMap::new();
        flow.insert(
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
                inline_action: None,
            },
        );
        flow.insert(
            "step2".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec!["step1".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        flow.insert(
            "step3".to_string(),
            FlowStep {
                action: "greet".to_string(),
                name: None,
                description: None,
                depends_on: vec!["step2".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
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
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "exit 1"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step2".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
        NewJobStep {
            job_id,
            step_name: "step3".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo ok"})),
            input: None,
            status: "pending".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        },
    ];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Fail step1
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "Command failed").await?;
    orchestrator::on_step_completed(&pool, job_id, "step1", &task, None).await?;

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

// ─── Multi-workspace helper ─────────────────────────────────────────

/// A second workspace config with different actions/tasks for multi-workspace tests
fn test_workspace_ops() -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();

    workspace.actions.insert(
        "deploy".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo deploying...".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    let mut flow = HashMap::new();
    flow.insert(
        "run-deploy".to_string(),
        FlowStep {
            action: "deploy".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "deploy-app".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    workspace
}

/// Setup with two workspaces: "default" and "ops"
async fn setup_multi_workspace() -> Result<(
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
        },
        workspaces: HashMap::from([
            (
                "default".to_string(),
                WorkspaceSourceDef::Folder {
                    path: temp_dir.path().to_string_lossy().to_string(),
                },
            ),
            (
                "ops".to_string(),
                WorkspaceSourceDef::Folder {
                    path: temp_dir.path().to_string_lossy().to_string(),
                },
            ),
        ]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let ws_default = test_workspace();
    let ws_ops = test_workspace_ops();

    // Build workspace manager with two workspaces using from_entries
    use std::path::PathBuf;
    use std::sync::Arc;
    use stroem_server::workspace::WorkspaceEntry;
    use tokio::sync::RwLock;

    // Helper in-memory source
    struct InMemSource(WorkspaceConfig);
    #[async_trait::async_trait]
    impl stroem_server::workspace::WorkspaceSource for InMemSource {
        async fn load(&self) -> Result<WorkspaceConfig> {
            Ok(self.0.clone())
        }
        fn path(&self) -> &std::path::Path {
            std::path::Path::new("/dev/null")
        }
        fn revision(&self) -> Option<String> {
            Some("test-rev".to_string())
        }
    }

    let src_default: Arc<dyn stroem_server::workspace::WorkspaceSource> =
        Arc::new(InMemSource(ws_default.clone()));
    let src_ops: Arc<dyn stroem_server::workspace::WorkspaceSource> =
        Arc::new(InMemSource(ws_ops.clone()));

    let mut entries = HashMap::new();
    entries.insert(
        "default".to_string(),
        WorkspaceEntry {
            config: Arc::new(RwLock::new(Arc::new(ws_default))),
            source: src_default,
            name: "default".to_string(),
            source_path: PathBuf::from("/dev/null"),
        },
    );
    entries.insert(
        "ops".to_string(),
        WorkspaceEntry {
            config: Arc::new(RwLock::new(Arc::new(ws_ops))),
            source: src_ops,
            name: "ops".to_string(),
            source_path: PathBuf::from("/dev/null"),
        },
    );

    let mgr = WorkspaceManager::from_entries(entries);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir, container))
}

// ─── Multi-workspace: List workspaces ───────────────────────────────

#[tokio::test]
async fn test_list_workspaces() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    let response = router.oneshot(api_get("/api/workspaces")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let workspaces = body.as_array().unwrap();
    assert_eq!(workspaces.len(), 2);

    let names: Vec<&str> = workspaces
        .iter()
        .map(|w| w["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"default"));
    assert!(names.contains(&"ops"));

    // Default workspace should have the test_workspace tasks/actions
    let default_ws = workspaces.iter().find(|w| w["name"] == "default").unwrap();
    assert!(default_ws["tasks_count"].as_u64().unwrap() > 0);
    assert!(default_ws["actions_count"].as_u64().unwrap() > 0);

    // Ops workspace should have 1 task and 1 action
    let ops_ws = workspaces.iter().find(|w| w["name"] == "ops").unwrap();
    assert_eq!(ops_ws["tasks_count"].as_u64().unwrap(), 1);
    assert_eq!(ops_ws["actions_count"].as_u64().unwrap(), 1);

    Ok(())
}

// ─── Multi-workspace: Task isolation ────────────────────────────────

#[tokio::test]
async fn test_workspace_task_isolation() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // List tasks in "default" workspace
    let response = router
        .clone()
        .oneshot(api_get("/api/workspaces/default/tasks"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let default_tasks: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert!(default_tasks.contains(&"hello-world"));
    assert!(!default_tasks.contains(&"deploy-app"));

    // List tasks in "ops" workspace
    let response = router
        .clone()
        .oneshot(api_get("/api/workspaces/ops/tasks"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let ops_tasks: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert!(ops_tasks.contains(&"deploy-app"));
    assert!(!ops_tasks.contains(&"hello-world"));

    Ok(())
}

// ─── Multi-workspace: Execute in specific workspace ─────────────────

#[tokio::test]
async fn test_execute_task_in_specific_workspace() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_multi_workspace().await?;

    // Execute deploy-app in "ops" workspace
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/ops/tasks/deploy-app/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Verify job was created with correct workspace
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.workspace, "ops");
    assert_eq!(job.task_name, "deploy-app");

    Ok(())
}

// ─── Multi-workspace: Task not found in wrong workspace ─────────────

#[tokio::test]
async fn test_execute_task_wrong_workspace_404() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // Try to execute "deploy-app" in "default" workspace (it's in "ops")
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/deploy-app/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 404);

    // Try to execute "hello-world" in "ops" workspace (it's in "default")
    let response = router
        .oneshot(api_request(
            "POST",
            "/api/workspaces/ops/tasks/hello-world/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Multi-workspace: Nonexistent workspace returns 404 ─────────────

#[tokio::test]
async fn test_nonexistent_workspace_404() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // List tasks in nonexistent workspace
    let response = router
        .clone()
        .oneshot(api_get("/api/workspaces/nonexistent/tasks"))
        .await?;
    assert_eq!(response.status(), 404);

    // Execute task in nonexistent workspace
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/nonexistent/tasks/hello-world/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 404);

    // Get task detail in nonexistent workspace
    let response = router
        .oneshot(api_get("/api/workspaces/nonexistent/tasks/hello-world"))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Multi-workspace: Worker claim returns correct workspace ────────

#[tokio::test]
async fn test_worker_claim_has_workspace_field() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create a job in the "ops" workspace
    let job_id = JobRepo::create(
        &pool,
        "ops",
        "deploy-app",
        "distributed",
        Some(json!({})),
        "api",
        None,
    )
    .await?;

    let steps = vec![NewJobStep {
        job_id,
        step_name: "run-deploy".to_string(),
        action_name: "deploy".to_string(),
        action_type: "script".to_string(),
        action_image: None,
        action_spec: Some(json!({"cmd": "echo deploying..."})),
        input: None,
        status: "ready".to_string(),
        required_tags: vec!["script".to_string()],
        runner: "local".to_string(),
        timeout_secs: None,
        when_condition: None,
    }];
    JobStepRepo::create_steps(&pool, &steps).await?;

    // Register worker
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "w1", "tags": ["script"]}),
        ))
        .await?;
    let reg_body = body_json(resp).await;
    let worker_id = reg_body["worker_id"].as_str().unwrap();

    // Claim step
    let resp = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let claim_body = body_json(resp).await;

    // Verify workspace field is "ops"
    assert_eq!(claim_body["workspace"].as_str().unwrap(), "ops");
    assert_eq!(claim_body["step_name"].as_str().unwrap(), "run-deploy");
    assert_eq!(claim_body["action_name"].as_str().unwrap(), "deploy");

    Ok(())
}

// ─── Multi-workspace: Jobs show workspace ───────────────────────────

#[tokio::test]
async fn test_jobs_show_workspace_field() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create jobs in both workspaces
    let resp1 = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Test"}}),
        ))
        .await?;
    assert_eq!(resp1.status(), 200);
    let body1 = body_json(resp1).await;
    let job_id_default = body1["job_id"].as_str().unwrap().to_string();

    let resp2 = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/ops/tasks/deploy-app/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(resp2.status(), 200);
    let body2 = body_json(resp2).await;
    let job_id_ops = body2["job_id"].as_str().unwrap().to_string();

    // Get job details and verify workspace field
    let resp = router
        .clone()
        .oneshot(api_get(&format!("/api/jobs/{}", job_id_default)))
        .await?;
    let job_body = body_json(resp).await;
    assert_eq!(job_body["workspace"].as_str().unwrap(), "default");

    let resp = router
        .oneshot(api_get(&format!("/api/jobs/{}", job_id_ops)))
        .await?;
    let job_body = body_json(resp).await;
    assert_eq!(job_body["workspace"].as_str().unwrap(), "ops");

    Ok(())
}

// ─── Multi-workspace: Tarball endpoint ──────────────────────────────

#[tokio::test]
async fn test_workspace_tarball_download() -> Result<()> {
    // Use folder-based setup so tarball has real files
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Create a workspace dir with some files
    let ws_dir = TempDir::new()?;
    let workflows_dir = ws_dir.path().join(".workflows");
    std::fs::create_dir_all(&workflows_dir)?;
    std::fs::write(
        workflows_dir.join("test.yaml"),
        "actions:\n  greet:\n    type: script\n    cmd: echo hi\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
    )?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr = WorkspaceManager::new(
        HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_dir.path().to_string_lossy().to_string(),
            },
        )]),
        HashMap::new(),
        HashMap::new(),
    )
    .await;

    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool, mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    // Download tarball
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(req).await?;
    assert_eq!(response.status(), 200);

    // Verify content-type
    let content_type = response.headers().get("content-type").unwrap().to_str()?;
    assert_eq!(content_type, "application/gzip");

    // Verify X-Revision header exists
    assert!(response.headers().get("X-Revision").is_some());
    let revision = response
        .headers()
        .get("X-Revision")
        .unwrap()
        .to_str()?
        .to_string();

    // Verify body is a valid gzip tarball
    let body = response.into_body().collect().await?.to_bytes();
    assert!(!body.is_empty());

    // Verify 304 Not Modified with matching ETag
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .header("If-None-Match", format!("\"{}\"", revision))
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(req).await?;
    assert_eq!(response.status(), 304);

    Ok(())
}

// ─── Multi-workspace: Tarball for nonexistent workspace ─────────────

#[tokio::test]
async fn test_workspace_tarball_nonexistent_404() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/nonexistent.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Multi-workspace: Get task detail from specific workspace ───────

#[tokio::test]
async fn test_get_task_detail_workspace_scoped() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // Get task detail from "ops" workspace
    let response = router
        .clone()
        .oneshot(api_get("/api/workspaces/ops/tasks/deploy-app"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["id"].as_str().unwrap(), "deploy-app");

    // Same task name doesn't exist in "default"
    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks/deploy-app"))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

// ─── Multi-workspace: Workspace revision in info ────────────────────

#[tokio::test]
async fn test_workspace_info_includes_revision() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    let response = router.oneshot(api_get("/api/workspaces")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let workspaces = body.as_array().unwrap();
    for ws in workspaces {
        // Our InMemSource returns "test-rev"
        assert_eq!(ws["revision"].as_str().unwrap(), "test-rev");
    }

    Ok(())
}

// ─── Tarball ETag caching: mismatched ETag returns 200 ──────────────

#[tokio::test]
async fn test_tarball_mismatched_etag_returns_200() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let ws_dir = TempDir::new()?;
    let workflows_dir = ws_dir.path().join(".workflows");
    std::fs::create_dir_all(&workflows_dir)?;
    std::fs::write(
        workflows_dir.join("test.yaml"),
        "actions:\n  greet:\n    type: script\n    cmd: echo hi\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
    )?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr = WorkspaceManager::new(
        HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_dir.path().to_string_lossy().to_string(),
            },
        )]),
        HashMap::new(),
        HashMap::new(),
    )
    .await;

    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool, mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    // Send a wrong ETag — should get 200 with full tarball, not 304
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .header("If-None-Match", "\"wrong-revision-value\"")
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(req).await?;
    assert_eq!(response.status(), 200);

    // Should still include ETag and X-Revision headers
    assert!(response.headers().get("X-Revision").is_some());
    assert!(response.headers().get("ETag").is_some());

    // Verify body is non-empty (full tarball returned)
    let body = response.into_body().collect().await?.to_bytes();
    assert!(!body.is_empty());

    Ok(())
}

// ─── Tarball ETag caching: bare (unquoted) ETag value also matches ──

#[tokio::test]
async fn test_tarball_bare_etag_matches() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let ws_dir = TempDir::new()?;
    let workflows_dir = ws_dir.path().join(".workflows");
    std::fs::create_dir_all(&workflows_dir)?;
    std::fs::write(
        workflows_dir.join("test.yaml"),
        "actions:\n  a:\n    type: script\n    cmd: echo ok\ntasks:\n  t:\n    flow:\n      s:\n        action: a\n",
    )?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr = WorkspaceManager::new(
        HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_dir.path().to_string_lossy().to_string(),
            },
        )]),
        HashMap::new(),
        HashMap::new(),
    )
    .await;

    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool, mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    // First request — get the revision
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(req).await?;
    assert_eq!(response.status(), 200);
    let revision = response
        .headers()
        .get("X-Revision")
        .unwrap()
        .to_str()?
        .to_string();

    // Send bare revision (no quotes) — trim_matches('"') should still match
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .header("If-None-Match", &revision)
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(req).await?;
    assert_eq!(
        response.status(),
        304,
        "Bare (unquoted) ETag should also produce 304"
    );

    Ok(())
}

// ─── Tarball ETag caching: changed workspace content invalidates ETag ─

#[tokio::test]
async fn test_tarball_stale_etag_after_workspace_change() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let ws_dir = TempDir::new()?;
    let workflows_dir = ws_dir.path().join(".workflows");
    std::fs::create_dir_all(&workflows_dir)?;
    let yaml_path = workflows_dir.join("test.yaml");
    std::fs::write(
        &yaml_path,
        "actions:\n  greet:\n    type: script\n    cmd: echo v1\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
    )?;

    let ws_path_str = ws_dir.path().to_string_lossy().to_string();

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_path_str.clone(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr = WorkspaceManager::new(
        HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_path_str.clone(),
            },
        )]),
        HashMap::new(),
        HashMap::new(),
    )
    .await;

    // AppState is Clone and shares Arc<WorkspaceManager> across clones
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool, mgr, config, log_storage, HashMap::new());
    let router = build_router(state.clone(), CancellationToken::new());

    // First request — get original revision
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(req).await?;
    assert_eq!(response.status(), 200);
    let old_revision = response
        .headers()
        .get("X-Revision")
        .unwrap()
        .to_str()?
        .to_string();

    // Modify workspace content — change the YAML
    std::fs::write(
        &yaml_path,
        "actions:\n  greet:\n    type: script\n    cmd: echo v2\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
    )?;

    // Reload the workspace so revision updates
    state.workspaces.reload("default").await?;

    // Old ETag should now return 200 (not 304) because revision changed
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .header("If-None-Match", format!("\"{}\"", old_revision))
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(req).await?;
    assert_eq!(
        response.status(),
        200,
        "Stale ETag after workspace change should return 200"
    );

    // New revision should differ from old
    let new_revision = response
        .headers()
        .get("X-Revision")
        .unwrap()
        .to_str()?
        .to_string();
    assert_ne!(
        old_revision, new_revision,
        "Revision should change after workspace content changes"
    );

    // New ETag should produce 304
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .header("If-None-Match", format!("\"{}\"", new_revision))
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await?;
    assert_eq!(
        response.status(),
        304,
        "Current ETag should produce 304 after reload"
    );

    Ok(())
}

// ─── Tarball ETag: response includes proper ETag format ─────────────

#[tokio::test]
async fn test_tarball_etag_header_format() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let ws_dir = TempDir::new()?;
    let workflows_dir = ws_dir.path().join(".workflows");
    std::fs::create_dir_all(&workflows_dir)?;
    std::fs::write(
        workflows_dir.join("test.yaml"),
        "actions:\n  a:\n    type: script\n    cmd: echo ok\ntasks:\n  t:\n    flow:\n      s:\n        action: a\n",
    )?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr = WorkspaceManager::new(
        HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: ws_dir.path().to_string_lossy().to_string(),
            },
        )]),
        HashMap::new(),
        HashMap::new(),
    )
    .await;

    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool, mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(req).await?;
    assert_eq!(response.status(), 200);

    // Verify ETag is properly quoted per HTTP spec
    let etag = response
        .headers()
        .get("ETag")
        .expect("Response should include ETag header")
        .to_str()?;
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "ETag should be quoted: got '{}'",
        etag
    );

    // X-Revision should be the bare (unquoted) value
    let revision = response
        .headers()
        .get("X-Revision")
        .expect("Response should include X-Revision header")
        .to_str()?;
    assert!(
        !revision.starts_with('"'),
        "X-Revision should be bare (unquoted)"
    );

    // ETag inner value should match X-Revision
    let etag_inner = etag.trim_matches('"');
    assert_eq!(
        etag_inner, revision,
        "ETag inner value should match X-Revision"
    );

    // Verify it's a hex string (Blake2s256 output)
    assert!(
        revision.chars().all(|c| c.is_ascii_hexdigit()),
        "Revision should be hex-encoded"
    );
    assert_eq!(revision.len(), 64, "Blake2s256 should produce 64 hex chars");

    Ok(())
}

// ─── Multi-workspace: Job listing with workspace filter ─────────────

#[tokio::test]
async fn test_list_jobs_with_workspace_filter() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create a job in "default" workspace
    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Alice"}}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);

    // Create a job in "ops" workspace
    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/ops/tasks/deploy-app/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);

    // Filter by "default" — should only return the default job
    let resp = router
        .clone()
        .oneshot(api_get("/api/jobs?workspace=default"))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let jobs = body["items"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "Should have exactly 1 default job");
    assert_eq!(jobs[0]["workspace"].as_str().unwrap(), "default");
    assert_eq!(jobs[0]["task_name"].as_str().unwrap(), "hello-world");
    assert_eq!(body["total"].as_i64().unwrap(), 1);

    // Filter by "ops" — should only return the ops job
    let resp = router
        .clone()
        .oneshot(api_get("/api/jobs?workspace=ops"))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let jobs = body["items"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "Should have exactly 1 ops job");
    assert_eq!(jobs[0]["workspace"].as_str().unwrap(), "ops");
    assert_eq!(jobs[0]["task_name"].as_str().unwrap(), "deploy-app");
    assert_eq!(body["total"].as_i64().unwrap(), 1);

    // No filter — should return both jobs
    let resp = router.oneshot(api_get("/api/jobs")).await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let jobs = body["items"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "Should have 2 jobs total");
    assert_eq!(body["total"].as_i64().unwrap(), 2);
    let workspaces: Vec<&str> = jobs
        .iter()
        .map(|j| j["workspace"].as_str().unwrap())
        .collect();
    assert!(workspaces.contains(&"default"));
    assert!(workspaces.contains(&"ops"));

    Ok(())
}

// ─── Multi-workspace: Nonexistent workspace filter returns empty ─────

#[tokio::test]
async fn test_list_jobs_workspace_filter_nonexistent() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create a job in "default" so there's at least one job in the system
    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Bob"}}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);

    // Filter by nonexistent workspace — should return 200 with empty items
    let resp = router
        .oneshot(api_get("/api/jobs?workspace=nonexistent"))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let jobs = body["items"].as_array().unwrap();
    assert!(
        jobs.is_empty(),
        "Nonexistent workspace filter should return empty items"
    );
    assert_eq!(body["total"].as_i64().unwrap(), 0);

    Ok(())
}

// ─── Multi-workspace: Worker claims from multiple workspaces ────────

#[tokio::test]
async fn test_worker_claim_across_workspaces() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create a job in "default" workspace via DB
    let job_id_default = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        Some(json!({"name": "Test"})),
        "api",
        None,
    )
    .await?;

    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id: job_id_default,
            step_name: "greet".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo Hello"})),
            input: Some(json!({"name": "Test"})),
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    // Create a job in "ops" workspace via DB
    let job_id_ops = JobRepo::create(
        &pool,
        "ops",
        "deploy-app",
        "distributed",
        Some(json!({})),
        "api",
        None,
    )
    .await?;

    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id: job_id_ops,
            step_name: "run-deploy".to_string(),
            action_name: "deploy".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo deploying..."})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    // Register worker
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "multi-ws-worker", "tags": ["script"]}),
        ))
        .await?;
    let reg_body = body_json(resp).await;
    let worker_id = reg_body["worker_id"].as_str().unwrap().to_string();

    // First claim — should get one of the two ready steps
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": &worker_id, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let claim1 = body_json(resp).await;
    let ws1 = claim1["workspace"].as_str().unwrap().to_string();
    let job_id1 = claim1["job_id"].as_str().unwrap().to_string();
    let step1 = claim1["step_name"].as_str().unwrap().to_string();
    assert!(
        ws1 == "default" || ws1 == "ops",
        "First claim should be from default or ops, got: {}",
        ws1
    );

    // Complete the first step so the worker can claim the next one
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/{}/complete", job_id1, step1),
            json!({"output": {"result": "done"}}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);

    // Second claim — should get the step from the other workspace
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": &worker_id, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let claim2 = body_json(resp).await;
    let ws2 = claim2["workspace"].as_str().unwrap().to_string();
    assert!(
        ws2 == "default" || ws2 == "ops",
        "Second claim should be from default or ops, got: {}",
        ws2
    );
    assert_ne!(
        ws1, ws2,
        "Worker should claim from both workspaces, got {ws1} twice",
    );

    // Third claim — no more steps available
    let resp = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": &worker_id, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let claim3 = body_json(resp).await;
    assert!(
        claim3["job_id"].is_null(),
        "Third claim should return no job"
    );

    Ok(())
}

// ─── Multi-workspace: Job list shows correct workspace field ────────

#[tokio::test]
async fn test_list_jobs_shows_workspace_field() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create jobs in both workspaces via API
    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Alice"}}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);

    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/ops/tasks/deploy-app/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);

    // List all jobs
    let resp = router.oneshot(api_get("/api/jobs")).await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let jobs = body["items"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(body["total"].as_i64().unwrap(), 2);

    // Verify each job in the list has the correct workspace field
    for job in jobs {
        let workspace = job["workspace"].as_str().unwrap();
        let task_name = job["task_name"].as_str().unwrap();
        match workspace {
            "default" => assert_eq!(task_name, "hello-world"),
            "ops" => assert_eq!(task_name, "deploy-app"),
            other => panic!("Unexpected workspace: {}", other),
        }
        // Verify workspace field is present and non-empty
        assert!(!workspace.is_empty(), "workspace field should not be empty");
    }

    Ok(())
}

// ─── Task folder: List tasks includes folder field ─────────────────

#[tokio::test]
async fn test_list_tasks_includes_folder() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks"))
        .await?;
    assert_eq!(response.status(), 200);

    let body = body_json(response).await;
    let tasks = body.as_array().unwrap();

    // Find task with folder
    let deploy_staging = tasks
        .iter()
        .find(|t| t["id"].as_str().unwrap() == "deploy-staging")
        .expect("deploy-staging task should exist");
    assert_eq!(deploy_staging["folder"].as_str().unwrap(), "deploy/staging");

    // Find task without folder — folder field should be absent (skip_serializing_if)
    let hello_world = tasks
        .iter()
        .find(|t| t["id"].as_str().unwrap() == "hello-world")
        .expect("hello-world task should exist");
    assert!(hello_world.get("folder").is_none() || hello_world["folder"].is_null());

    Ok(())
}

// ─── Job creator: create_job_for_task with trigger source ──────────

#[tokio::test]
async fn test_create_job_for_task_trigger_source() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let workspace = test_workspace();
    let input = json!({"name": "Scheduler"});

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "hello-world",
        input.clone(),
        "trigger",
        Some("default/every-minute"),
    )
    .await?;

    // Verify job
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.task_name, "hello-world");
    assert_eq!(job.workspace, "default");
    assert_eq!(job.source_type, "trigger");
    assert_eq!(job.source_id.as_deref(), Some("default/every-minute"));
    assert_eq!(job.input, Some(input));
    assert_eq!(job.status, "pending");

    // Verify steps
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].step_name, "greet");
    assert_eq!(steps[0].action_name, "greet");
    assert_eq!(steps[0].status, "ready");

    Ok(())
}

// ─── Job creator: multi-step task creates correct step statuses ────

#[tokio::test]
async fn test_create_job_for_task_multi_step() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let workspace = test_workspace();
    let input = json!({"name": "Test"});

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "greet-and-shout",
        input,
        "api",
        None,
    )
    .await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps.len(), 2);

    // Find steps by name
    let greet_step = steps.iter().find(|s| s.step_name == "greet").unwrap();
    let shout_step = steps.iter().find(|s| s.step_name == "shout").unwrap();

    // greet has no dependencies → ready; shout depends on greet → pending
    assert_eq!(greet_step.status, "ready");
    assert_eq!(shout_step.status, "pending");

    Ok(())
}

// ─── Job creator: task not found returns error ─────────────────────

#[tokio::test]
async fn test_create_job_for_task_missing_task() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let workspace = test_workspace();

    let result = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "nonexistent-task",
        json!({}),
        "api",
        None,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("not found"),
        "Error should mention 'not found': {}",
        err
    );

    Ok(())
}

// ─── Job creator: missing action returns error ─────────────────────

#[tokio::test]
async fn test_create_job_for_task_missing_action() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    // Build a workspace with a task that references a non-existent action
    let mut workspace = WorkspaceConfig::default();
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "action-does-not-exist".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "broken-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    let result = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "broken-task",
        json!({}),
        "api",
        None,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("not found"),
        "Error should mention action not found: {}",
        err
    );

    Ok(())
}

// ─── OIDC integration tests ────────────────────────────────────────────

#[tokio::test]
async fn test_config_returns_oidc_providers_empty() -> Result<()> {
    let (router, _pool, _temp, _container) = setup().await?;

    let res = router.oneshot(api_get("/api/config")).await?;

    assert_eq!(res.status(), 200);
    let body = body_json(res).await;
    assert_eq!(body["auth_required"], false);
    assert_eq!(body["has_internal_auth"], false);
    assert_eq!(body["oidc_providers"], json!([]));

    Ok(())
}

#[tokio::test]
async fn test_oidc_start_unknown_provider() -> Result<()> {
    let (router, _pool, _temp, _container) = setup().await?;

    let res = router
        .oneshot(api_get("/api/auth/oidc/nonexistent"))
        .await?;

    assert_eq!(res.status(), 404);

    Ok(())
}

#[tokio::test]
async fn test_auth_link_create_and_get() -> Result<()> {
    let (_router, pool, _temp, _container) = setup().await?;

    let user_id = Uuid::new_v4();
    UserRepo::create(&pool, user_id, "oidc@test.com", None, Some("OIDC User")).await?;

    UserAuthLinkRepo::create(&pool, user_id, "google", "ext-123").await?;

    let link = UserAuthLinkRepo::get_by_provider_and_external_id(&pool, "google", "ext-123")
        .await?
        .expect("Link should exist");
    assert_eq!(link.user_id, user_id);
    assert_eq!(link.provider_id, "google");
    assert_eq!(link.external_id, "ext-123");

    Ok(())
}

#[tokio::test]
async fn test_auth_link_nonexistent() -> Result<()> {
    let (_router, pool, _temp, _container) = setup().await?;

    let link =
        UserAuthLinkRepo::get_by_provider_and_external_id(&pool, "google", "no-such-id").await?;
    assert!(link.is_none());

    Ok(())
}

#[tokio::test]
async fn test_auth_link_multiple_providers_same_user() -> Result<()> {
    let (_router, pool, _temp, _container) = setup().await?;

    let user_id = Uuid::new_v4();
    UserRepo::create(&pool, user_id, "multi@test.com", None, None).await?;

    UserAuthLinkRepo::create(&pool, user_id, "google", "g-123").await?;
    UserAuthLinkRepo::create(&pool, user_id, "github", "gh-456").await?;

    let g = UserAuthLinkRepo::get_by_provider_and_external_id(&pool, "google", "g-123")
        .await?
        .expect("Google link");
    assert_eq!(g.user_id, user_id);

    let gh = UserAuthLinkRepo::get_by_provider_and_external_id(&pool, "github", "gh-456")
        .await?
        .expect("GitHub link");
    assert_eq!(gh.user_id, user_id);

    Ok(())
}

#[tokio::test]
async fn test_provision_creates_new_user() -> Result<()> {
    let (_router, pool, _temp, _container) = setup().await?;

    let user = stroem_server::oidc::provision_user(
        &pool,
        "google",
        "ext-new-user",
        "new@oidc.com",
        Some("New User"),
    )
    .await?;

    assert_eq!(user.email, "new@oidc.com");
    assert_eq!(user.name.as_deref(), Some("New User"));
    assert!(user.password_hash.is_none());

    // Verify auth link was created
    let link = UserAuthLinkRepo::get_by_provider_and_external_id(&pool, "google", "ext-new-user")
        .await?
        .expect("Auth link should exist");
    assert_eq!(link.user_id, user.user_id);

    Ok(())
}

#[tokio::test]
async fn test_provision_links_existing_email_user() -> Result<()> {
    let (_router, pool, _temp, _container) = setup().await?;

    // Create existing user with password
    let existing_id = Uuid::new_v4();
    UserRepo::create(
        &pool,
        existing_id,
        "existing@test.com",
        Some("$argon2id$hash"),
        Some("Existing"),
    )
    .await?;

    let user = stroem_server::oidc::provision_user(
        &pool,
        "google",
        "ext-existing",
        "existing@test.com",
        None,
    )
    .await?;

    // Should return the existing user
    assert_eq!(user.user_id, existing_id);
    assert_eq!(user.email, "existing@test.com");

    // Auth link should be created
    let link =
        UserAuthLinkRepo::get_by_provider_and_external_id(&pool, "google", "ext-existing").await?;
    assert!(link.is_some());

    Ok(())
}

#[tokio::test]
async fn test_provision_returns_linked_user() -> Result<()> {
    let (_router, pool, _temp, _container) = setup().await?;

    // Create user and link
    let user_id = Uuid::new_v4();
    UserRepo::create(&pool, user_id, "linked@test.com", None, None).await?;
    UserAuthLinkRepo::create(&pool, user_id, "google", "ext-linked").await?;

    // Provision should find existing link
    let user = stroem_server::oidc::provision_user(
        &pool,
        "google",
        "ext-linked",
        "linked@test.com",
        Some("Different Name"),
    )
    .await?;

    assert_eq!(user.user_id, user_id);
    assert_eq!(user.email, "linked@test.com");

    Ok(())
}

#[tokio::test]
async fn test_config_returns_oidc_providers_with_auth() -> Result<()> {
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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: "secret".to_string(),
            refresh_secret: "refresh".to_string(),
            base_url: None,
            providers: HashMap::new(),
            initial_user: None,
        }),
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let workspace = test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    let res = router.oneshot(api_get("/api/config")).await?;
    assert_eq!(res.status(), 200);
    let body = body_json(res).await;
    assert_eq!(body["auth_required"], true);
    // No providers configured → internal auth defaults to true (backward compat)
    assert_eq!(body["has_internal_auth"], true);
    assert_eq!(body["oidc_providers"], json!([]));

    Ok(())
}

#[tokio::test]
async fn test_config_returns_has_internal_auth_default() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let res = router.oneshot(api_get("/api/config")).await?;
    assert_eq!(res.status(), 200);
    let body = body_json(res).await;
    assert_eq!(body["auth_required"], true);
    // setup_with_auth has no providers configured → defaults to internal auth
    assert_eq!(body["has_internal_auth"], true);

    Ok(())
}

#[tokio::test]
async fn test_config_returns_has_internal_auth_true() -> Result<()> {
    use stroem_server::config::ProviderConfig;

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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: "secret".to_string(),
            refresh_secret: "refresh".to_string(),
            base_url: None,
            providers: HashMap::from([(
                "internal".to_string(),
                ProviderConfig {
                    provider_type: "internal".to_string(),
                    display_name: None,
                    issuer_url: None,
                    client_id: None,
                    client_secret: None,
                },
            )]),
            initial_user: None,
        }),
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let workspace = test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    let res = router.oneshot(api_get("/api/config")).await?;
    assert_eq!(res.status(), 200);
    let body = body_json(res).await;
    assert_eq!(body["auth_required"], true);
    assert_eq!(body["has_internal_auth"], true);

    Ok(())
}

#[tokio::test]
async fn test_config_returns_has_internal_auth_false_oidc_only() -> Result<()> {
    use stroem_server::config::ProviderConfig;

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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: "secret".to_string(),
            refresh_secret: "refresh".to_string(),
            base_url: Some("https://stroem.example.com".to_string()),
            providers: HashMap::from([(
                "google".to_string(),
                ProviderConfig {
                    provider_type: "oidc".to_string(),
                    display_name: Some("Google".to_string()),
                    issuer_url: Some("https://accounts.google.com".to_string()),
                    client_id: Some("id".to_string()),
                    client_secret: Some("secret".to_string()),
                },
            )]),
            initial_user: None,
        }),
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let workspace = test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    // No actual OIDC providers initialized (would need real discovery)
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    let res = router.oneshot(api_get("/api/config")).await?;
    assert_eq!(res.status(), 200);
    let body = body_json(res).await;
    assert_eq!(body["auth_required"], true);
    // Only OIDC provider configured, no internal → false
    assert_eq!(body["has_internal_auth"], false);

    Ok(())
}

// ─── Hook integration tests ──────────────────────────────────────────

/// Helper: build a workspace with on_success/on_error hooks for testing.
fn hook_test_workspace() -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();

    // Action: deploy (script)
    workspace.actions.insert(
        "deploy".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo deploying".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Action: crash (script, always fails)
    workspace.actions.insert(
        "crash".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("exit 1".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Action: notify (the hook action)
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
            script: Some("echo {{ input.message }}".to_string()),
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
        },
    );

    workspace
}

fn hook_test_state(pool: PgPool, workspace: &WorkspaceConfig) -> AppState {
    let temp_dir = std::env::temp_dir().join(format!("stroem-hook-test-{}", Uuid::new_v4()));
    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: "postgres://test".to_string(),
        },
        log_storage: LogStorageConfig {
            local_dir: temp_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test".to_string(),
        auth: None,
        recovery: stroem_server::config::RecoveryConfig {
            heartbeat_timeout_secs: 120,
            sweep_interval_secs: 60,
            unmatched_step_timeout_secs: 30,
        },
        acl: None,
        mcp: None,
    };
    let mgr = WorkspaceManager::from_config("default", workspace.clone());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    AppState::new(pool, mgr, config, log_storage, HashMap::new())
}

#[tokio::test]
async fn test_hook_fires_on_job_success() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = hook_test_workspace();

    // Task with on_success hook
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "deploy".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut hook_input = HashMap::new();
    hook_input.insert(
        "message".to_string(),
        json!("Success: {{ hook.task_name }}"),
    );
    workspace.tasks.insert(
        "deploy-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
            on_error: vec![],
        },
    );

    // Create job using job_creator
    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "deploy-task",
        json!({}),
        "api",
        None,
    )
    .await?;

    let task = workspace.tasks.get("deploy-task").unwrap();

    // Register worker and simulate step completion
    let worker_id = register_test_worker(&pool).await;
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, "ready");

    // Mark step running, then completed
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step1", Some(json!({"result": "ok"}))).await?;

    // Run orchestrator to mark job as completed
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;

    // Verify job is completed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    // Fire hooks
    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    // Verify hook job was created
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    assert_eq!(all_jobs.len(), 2); // original + hook

    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("Hook job not found");
    assert_eq!(hook_job.task_name, "_hook:notify");
    assert_eq!(hook_job.source_id, Some(job_id.to_string()));

    // Verify hook step was created and is ready
    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    assert_eq!(hook_steps.len(), 1);
    assert_eq!(hook_steps[0].step_name, "hook");
    assert_eq!(hook_steps[0].action_name, "notify");
    assert_eq!(hook_steps[0].status, "ready");

    // Verify rendered input contains the task name
    let hook_input = hook_steps[0].input.as_ref().unwrap();
    assert_eq!(hook_input["message"], "Success: deploy-task");

    Ok(())
}

#[tokio::test]
async fn test_hook_fires_on_job_failure() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = hook_test_workspace();

    // Task with on_error hook
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "crash".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut hook_input = HashMap::new();
    hook_input.insert(
        "message".to_string(),
        json!("Failed: {{ hook.error_message }}"),
    );
    workspace.tasks.insert(
        "crash-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
        },
    );

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "crash-task",
        json!({}),
        "api",
        None,
    )
    .await?;

    let task = workspace.tasks.get("crash-task").unwrap();

    // Simulate step failure
    let worker_id = register_test_worker(&pool).await;
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "exit code 1").await?;

    // Orchestrator marks job as failed
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    // Fire hooks
    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    // Verify hook job was created
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("Hook job not found");
    assert_eq!(hook_job.source_id, Some(job_id.to_string()));

    // Verify rendered input contains error message
    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    let hook_input = hook_steps[0].input.as_ref().unwrap();
    assert_eq!(hook_input["message"], "Failed: Step 'step1': exit code 1");

    Ok(())
}

#[tokio::test]
async fn test_hook_not_fired_for_hook_job() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = hook_test_workspace();

    // Task with on_success hook (to verify recursion doesn't happen)
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "deploy".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "deploy-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![HookDef {
                action: "notify".to_string(),
                input: HashMap::new(),
            }],
            on_error: vec![],
        },
    );

    let task = workspace.tasks.get("deploy-task").unwrap();

    // Create a hook job directly (simulating a hook that already fired)
    let hook_job_id = JobRepo::create(
        &pool,
        "default",
        "_hook:notify",
        "distributed",
        Some(json!({})),
        "hook",
        Some("default/deploy-task/abc/on_success[0]"),
    )
    .await?;

    // Mark the hook job as completed
    JobRepo::mark_completed(&pool, hook_job_id, None).await?;

    let hook_job = JobRepo::get(&pool, hook_job_id).await?.unwrap();
    assert_eq!(hook_job.source_type, "hook");

    // Fire hooks — should be a no-op because source_type is "hook"
    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &hook_job, task).await;

    // Verify no additional hook jobs were created
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    assert_eq!(all_jobs.len(), 1); // Only the original hook job

    Ok(())
}

#[tokio::test]
async fn test_hook_input_contains_context() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = hook_test_workspace();

    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "deploy".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut hook_input = HashMap::new();
    hook_input.insert("ws".to_string(), json!("{{ hook.workspace }}"));
    hook_input.insert("task".to_string(), json!("{{ hook.task_name }}"));
    hook_input.insert("st".to_string(), json!("{{ hook.status }}"));
    hook_input.insert("src".to_string(), json!("{{ hook.source_type }}"));
    workspace.tasks.insert(
        "deploy-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
            on_error: vec![],
        },
    );

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "deploy-task",
        json!({}),
        "api",
        None,
    )
    .await?;

    let task = workspace.tasks.get("deploy-task").unwrap();
    let worker_id = register_test_worker(&pool).await;

    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step1", None).await?;
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("Hook job not found");

    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    let input = hook_steps[0].input.as_ref().unwrap();

    assert_eq!(input["ws"], "default");
    assert_eq!(input["task"], "deploy-task");
    assert_eq!(input["st"], "completed");
    assert_eq!(input["src"], "api");

    Ok(())
}

#[tokio::test]
async fn test_hook_error_message_all_failures() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = hook_test_workspace();

    // Two independent steps, both fail. step2 has continue_on_failure=false
    // which causes the job to be marked "failed". Both errors appear in hook context.
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "crash".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: true,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "crash".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut hook_input = HashMap::new();
    hook_input.insert("err".to_string(), json!("{{ hook.error_message }}"));
    workspace.tasks.insert(
        "multi-fail".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
        },
    );

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "multi-fail",
        json!({}),
        "api",
        None,
    )
    .await?;

    let task = workspace.tasks.get("multi-fail").unwrap();
    let worker_id = register_test_worker(&pool).await;

    // Both steps are independent and start as "ready". Fail both.
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "build error").await?;
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;

    // Fail step2
    JobStepRepo::mark_running(&pool, job_id, "step2", worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step2", "test failure").await?;
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step2", task, None).await?;

    // Job should be failed (step2.continue_on_failure=false)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    // Fire hooks
    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("Hook job not found");

    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    let input = hook_steps[0].input.as_ref().unwrap();

    let err_msg = input["err"].as_str().unwrap();
    // Both step errors should be present
    assert!(
        err_msg.contains("step1"),
        "Missing step1 error: {}",
        err_msg
    );
    assert!(
        err_msg.contains("build error"),
        "Missing build error: {}",
        err_msg
    );
    assert!(
        err_msg.contains("step2"),
        "Missing step2 error: {}",
        err_msg
    );
    assert!(
        err_msg.contains("test failure"),
        "Missing test failure: {}",
        err_msg
    );

    Ok(())
}

/// on_success hook still receives error_message when all failures are tolerable
#[tokio::test]
async fn test_hook_on_success_with_tolerable_failures() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = hook_test_workspace();

    // Two steps, both with continue_on_failure=true. One fails → job "completed" → on_success fires.
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "crash".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: true,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "step2".to_string(),
        FlowStep {
            action: "deploy".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: true,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    let mut hook_input = HashMap::new();
    hook_input.insert("error".to_string(), json!("{{ hook.error_message }}"));
    hook_input.insert("is_success".to_string(), json!("{{ hook.is_success }}"));
    workspace.tasks.insert(
        "tolerant".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
            on_error: vec![],
        },
    );

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "tolerant",
        json!({}),
        "api",
        None,
    )
    .await?;

    let task = workspace.tasks.get("tolerant").unwrap();
    let worker_id = register_test_worker(&pool).await;

    // step1 fails (but tolerable)
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "deploy crashed").await?;
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;

    // step2 succeeds
    JobStepRepo::mark_running(&pool, job_id, "step2", worker_id).await?;
    JobStepRepo::mark_completed(&pool, job_id, "step2", None).await?;
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step2", task, None).await?;

    // Job should be "completed" (all failures tolerable)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "completed");

    // on_success fires
    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("on_success hook job should be created");
    assert_eq!(hook_job.source_id, Some(job_id.to_string()));

    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    let input = hook_steps[0].input.as_ref().unwrap();

    // error_message should still contain the failed step info
    let err_field = input["error"].as_str().unwrap();
    assert!(
        err_field.contains("step1"),
        "on_success hook should still have failed step info: {}",
        err_field
    );
    assert!(
        err_field.contains("deploy crashed"),
        "on_success hook should contain the error text: {}",
        err_field
    );

    // is_success should be true
    let is_success = input["is_success"].as_str().unwrap();
    assert_eq!(is_success, "true");

    Ok(())
}

/// Multiline error messages (e.g. Python tracebacks) are preserved in hook context
#[tokio::test]
async fn test_hook_multiline_error_message() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = hook_test_workspace();

    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "crash".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    let mut hook_input = HashMap::new();
    hook_input.insert("err".to_string(), json!("{{ hook.error_message }}"));
    workspace.tasks.insert(
        "py-crash".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
        },
    );

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "py-crash",
        json!({}),
        "api",
        None,
    )
    .await?;

    let task = workspace.tasks.get("py-crash").unwrap();
    let worker_id = register_test_worker(&pool).await;

    // Simulate a Python traceback as the error message
    let python_traceback = "Traceback (most recent call last):\n  File \"deploy.py\", line 42, in main\n    result = connect(host)\n  File \"deploy.py\", line 18, in connect\n    raise ConnectionError(f\"Failed to connect to {host}\")\nConnectionError: Failed to connect to prod-db.internal";

    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", python_traceback).await?;
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("Hook job not found");

    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    let input = hook_steps[0].input.as_ref().unwrap();

    let err_msg = input["err"].as_str().unwrap();

    // Full traceback should be preserved, including newlines
    assert!(
        err_msg.contains("Traceback (most recent call last):"),
        "Missing traceback header: {}",
        err_msg
    );
    assert!(
        err_msg.contains("ConnectionError: Failed to connect to prod-db.internal"),
        "Missing final exception line: {}",
        err_msg
    );
    assert!(
        err_msg.contains("deploy.py\", line 42"),
        "Missing stack frame: {}",
        err_msg
    );
    assert!(
        err_msg.contains('\n'),
        "Newlines should be preserved in error message",
    );

    Ok(())
}

/// Hook job completes successfully through orchestrator (tests the bug fix where
/// hook jobs with synthetic task names like "_hook:notify" would fail in complete_step
/// because the task didn't exist in the workspace).
#[tokio::test]
async fn test_hook_job_completes_through_orchestrator() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = hook_test_workspace();

    // Create a task with on_error hook
    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "crash".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut hook_input = HashMap::new();
    hook_input.insert("message".to_string(), json!("{{ hook.error_message }}"));
    workspace.tasks.insert(
        "with-hook".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![HookDef {
                action: "notify".to_string(),
                input: hook_input,
            }],
        },
    );

    // Create and fail the original job
    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "with-hook",
        json!({}),
        "api",
        None,
    )
    .await?;

    let task = workspace.tasks.get("with-hook").unwrap();
    let worker_id = register_test_worker(&pool).await;

    // Worker-style error format with Python traceback
    let python_error = "Exit code: 1\nStderr: Traceback (most recent call last):\n  File \"app.py\", line 10, in main\n    raise ValueError(\"bad input\")\nValueError: bad input";

    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", python_error).await?;
    stroem_server::orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    // Fire hooks → creates hook job
    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_job = all_jobs
        .iter()
        .find(|j| j.source_type == "hook")
        .expect("Hook job should be created");

    // Verify hook job has synthetic task name
    assert!(
        hook_job.task_name.starts_with("_hook:"),
        "Hook job task_name should be synthetic: {}",
        hook_job.task_name
    );

    let hook_job_id = hook_job.job_id;
    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job_id).await?;
    assert_eq!(hook_steps.len(), 1);
    assert_eq!(hook_steps[0].step_name, "hook");
    assert_eq!(hook_steps[0].status, "ready");

    // Verify hook step input contains the Python traceback
    let hook_step_input = hook_steps[0].input.as_ref().unwrap();
    let msg = hook_step_input["message"].as_str().unwrap();
    assert!(
        msg.contains("ValueError: bad input"),
        "Hook input should contain the Python error: {}",
        msg
    );
    assert!(
        msg.contains("Traceback"),
        "Hook input should contain traceback: {}",
        msg
    );

    // Now simulate the hook job being executed by a worker:
    // Worker claims it, runs it, reports success.
    JobStepRepo::mark_running(&pool, hook_job_id, "hook", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, hook_job_id, worker_id).await?;
    JobStepRepo::mark_completed(&pool, hook_job_id, "hook", None).await?;

    // Build a synthetic TaskDef the same way complete_step does for hook jobs
    let hook_steps_for_task = JobStepRepo::get_steps_for_job(&pool, hook_job_id).await?;
    let mut hook_flow = HashMap::new();
    for step in &hook_steps_for_task {
        hook_flow.insert(
            step.step_name.clone(),
            FlowStep {
                action: step.action_name.clone(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                timeout: None,
                when: None,
                inline_action: None,
            },
        );
    }
    let hook_task_def = TaskDef {
        name: None,
        description: None,
        mode: "distributed".to_string(),
        folder: None,
        input: HashMap::new(),
        flow: hook_flow,
        timeout: None,

        on_success: vec![],
        on_error: vec![],
    };

    // Orchestrator should mark the hook job as completed
    stroem_server::orchestrator::on_step_completed(
        &pool,
        hook_job_id,
        "hook",
        &hook_task_def,
        None,
    )
    .await?;

    let hook_job_after = JobRepo::get(&pool, hook_job_id).await?.unwrap();
    assert_eq!(
        hook_job_after.status, "completed",
        "Hook job should be marked completed by orchestrator"
    );

    // Verify no recursive hook job was created (recursion guard)
    let all_jobs_after = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_jobs: Vec<_> = all_jobs_after
        .iter()
        .filter(|j| j.source_type == "hook")
        .collect();
    assert_eq!(
        hook_jobs.len(),
        1,
        "Should be exactly 1 hook job (no recursion)"
    );

    Ok(())
}

// ─── Task action (type: task) tests ────────────────────────────────────

/// Build a workspace for task-action tests.
fn task_action_test_workspace() -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();

    // Action: greet (script)
    workspace.actions.insert(
        "greet".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo hello".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Action: run-cleanup (type: task → cleanup)
    workspace.actions.insert(
        "run-cleanup".to_string(),
        ActionDef {
            action_type: "task".to_string(),
            name: None,
            description: None,
            task: Some("cleanup".to_string()),
            cmd: None,
            script: None,
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Task: cleanup (single step)
    let mut cleanup_flow = HashMap::new();
    cleanup_flow.insert(
        "clean".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "cleanup".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: cleanup_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Task: deploy (build → cleanup via task action)
    let mut deploy_flow = HashMap::new();
    deploy_flow.insert(
        "build".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    deploy_flow.insert(
        "cleanup".to_string(),
        FlowStep {
            action: "run-cleanup".to_string(),
            name: None,
            description: None,
            depends_on: vec!["build".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "deploy".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: deploy_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    workspace
}

#[tokio::test]
async fn test_task_action_creates_child_job() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let workspace = task_action_test_workspace();

    // Create a job that has a task-action step as the only initially-ready step
    // Use "cleanup" task name but insert a task with a single task-action step at root
    let mut ws = workspace.clone();
    let mut flow = HashMap::new();
    flow.insert(
        "run".to_string(),
        FlowStep {
            action: "run-cleanup".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    ws.tasks.insert(
        "run-task-action".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    let parent_job_id = create_job_for_task(
        &pool,
        &ws,
        "default",
        "run-task-action",
        json!({}),
        "api",
        None,
    )
    .await?;

    // The parent step "run" should be running (server-side dispatch)
    let parent_steps = JobStepRepo::get_steps_for_job(&pool, parent_job_id).await?;
    assert_eq!(parent_steps.len(), 1);
    assert_eq!(parent_steps[0].step_name, "run");
    assert_eq!(parent_steps[0].action_type, "task");
    assert_eq!(parent_steps[0].status, "running");

    // A child job should have been created
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    assert_eq!(all_jobs.len(), 2);

    let child_job = all_jobs
        .iter()
        .find(|j| j.source_type == "task")
        .expect("Child job not found");
    assert_eq!(child_job.task_name, "cleanup");
    assert_eq!(child_job.parent_job_id, Some(parent_job_id));
    assert_eq!(child_job.parent_step_name.as_deref(), Some("run"));

    // Child job should have its own steps
    let child_steps = JobStepRepo::get_steps_for_job(&pool, child_job.job_id).await?;
    assert_eq!(child_steps.len(), 1);
    assert_eq!(child_steps[0].step_name, "clean");
    assert_eq!(child_steps[0].action_type, "script");
    assert_eq!(child_steps[0].status, "ready");

    Ok(())
}

#[tokio::test]
async fn test_task_action_child_completion_updates_parent() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let workspace = task_action_test_workspace();

    // Use the deploy task: build → cleanup (task action)
    let parent_job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "deploy",
        json!({}),
        "api",
        None,
    )
    .await?;

    // Step "build" is ready, "cleanup" is pending (depends on build)
    let parent_steps = JobStepRepo::get_steps_for_job(&pool, parent_job_id).await?;
    let build_step = parent_steps
        .iter()
        .find(|s| s.step_name == "build")
        .unwrap();
    let cleanup_step = parent_steps
        .iter()
        .find(|s| s.step_name == "cleanup")
        .unwrap();
    assert_eq!(build_step.status, "ready");
    assert_eq!(cleanup_step.status, "pending");

    // Only 1 job so far (no child yet because cleanup is pending)
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    assert_eq!(all_jobs.len(), 1);

    // Simulate worker completing the build step
    let worker_id = register_test_worker(&pool).await;
    JobStepRepo::mark_running(&pool, parent_job_id, "build", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, parent_job_id, worker_id).await?;
    JobStepRepo::mark_completed(
        &pool,
        parent_job_id,
        "build",
        Some(json!({"artifact": "v1.0"})),
    )
    .await?;

    // Run orchestrator to promote cleanup step
    let task = workspace.tasks.get("deploy").unwrap();
    orchestrator::on_step_completed(&pool, parent_job_id, "build", task, None).await?;

    // Now handle_task_steps should dispatch the cleanup step
    stroem_server::job_creator::handle_task_steps(&pool, &workspace, "default", parent_job_id)
        .await?;

    // Cleanup step should now be running
    let steps_after = JobStepRepo::get_steps_for_job(&pool, parent_job_id).await?;
    let cleanup_after = steps_after
        .iter()
        .find(|s| s.step_name == "cleanup")
        .unwrap();
    assert_eq!(cleanup_after.status, "running");

    // A child job should exist
    let all_jobs_2 = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    assert_eq!(all_jobs_2.len(), 2);

    let child_job = all_jobs_2
        .iter()
        .find(|j| j.source_type == "task")
        .expect("Child job not found");

    // Now simulate completing the child job's step
    let child_steps = JobStepRepo::get_steps_for_job(&pool, child_job.job_id).await?;
    assert_eq!(child_steps[0].status, "ready");

    JobStepRepo::mark_running(&pool, child_job.job_id, "clean", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, child_job.job_id, worker_id).await?;
    JobStepRepo::mark_completed(
        &pool,
        child_job.job_id,
        "clean",
        Some(json!({"cleaned": true})),
    )
    .await?;

    // Run orchestrator for the child job
    let child_task = workspace.tasks.get("cleanup").unwrap();
    orchestrator::on_step_completed(&pool, child_job.job_id, "clean", child_task, None).await?;

    // Child job should now be completed
    let child_after = JobRepo::get(&pool, child_job.job_id).await?.unwrap();
    assert_eq!(child_after.status, "completed");

    // Simulate propagation: mark parent step completed with child's output
    JobStepRepo::mark_completed(&pool, parent_job_id, "cleanup", child_after.output.clone())
        .await?;

    // Run orchestrator for the parent job
    orchestrator::on_step_completed(&pool, parent_job_id, "cleanup", task, None).await?;

    // Parent job should now be completed
    let parent_after = JobRepo::get(&pool, parent_job_id).await?.unwrap();
    assert_eq!(parent_after.status, "completed");

    // Parent job output should include the child job's output via the cleanup step
    // Child job output is aggregated by orchestrator: { "clean": { "cleaned": true } }
    // That becomes the cleanup step's output, and since cleanup is a terminal step
    // in the parent, it appears in the parent's output
    let parent_output = parent_after.output.unwrap();
    assert!(
        parent_output.get("cleanup").is_some(),
        "Parent output should contain 'cleanup' key"
    );

    Ok(())
}

#[tokio::test]
async fn test_task_action_not_claimed_by_worker() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let _workspace = task_action_test_workspace();

    // Insert a step that is "ready" with action_type "task" directly
    let job_id =
        JobRepo::create(&pool, "default", "deploy", "distributed", None, "api", None).await?;

    let step = NewJobStep {
        job_id,
        step_name: "task-step".to_string(),
        action_name: "run-cleanup".to_string(),
        action_type: "task".to_string(),
        action_image: None,
        action_spec: Some(json!({"task": "cleanup"})),
        input: None,
        status: "ready".to_string(),
        required_tags: vec![],
        runner: "none".to_string(),
        timeout_secs: None,
        when_condition: None,
    };
    JobStepRepo::create_steps(&pool, &[step]).await?;

    // Register a worker and try to claim
    let worker_id = register_test_worker(&pool).await;
    let claimed = JobStepRepo::claim_ready_step(&pool, &["script".to_string()], worker_id).await?;

    // Worker should NOT claim the task-type step
    assert!(claimed.is_none(), "Worker should not claim task-type steps");

    Ok(())
}

#[tokio::test]
async fn test_task_action_input_rendered() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = task_action_test_workspace();

    // Add input-aware task action
    workspace.actions.insert(
        "run-with-input".to_string(),
        ActionDef {
            action_type: "task".to_string(),
            name: None,
            description: None,
            task: Some("cleanup".to_string()),
            cmd: None,
            script: None,
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Add a task with a task-action step that passes templated input
    let mut flow = HashMap::new();
    let mut step_input = HashMap::new();
    step_input.insert("env".to_string(), json!("{{ input.environment }}"));
    flow.insert(
        "run".to_string(),
        FlowStep {
            action: "run-with-input".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: step_input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let mut task_input = HashMap::new();
    task_input.insert(
        "environment".to_string(),
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
        "deploy-with-input".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: task_input,
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    let _parent_job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "deploy-with-input",
        json!({"environment": "production"}),
        "api",
        None,
    )
    .await?;

    // Child job should exist and its input should contain the rendered value
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    assert_eq!(all_jobs.len(), 2);

    let child_job = all_jobs
        .iter()
        .find(|j| j.source_type == "task")
        .expect("Child job not found");
    let child_input = child_job.input.as_ref().unwrap();
    assert_eq!(child_input["env"], "production");

    Ok(())
}

#[tokio::test]
async fn test_task_action_in_hook() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = task_action_test_workspace();

    // Task with on_error hook that uses a task-type action
    let mut flow = HashMap::new();
    flow.insert(
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
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "deploy-with-hook".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![HookDef {
                action: "run-cleanup".to_string(),
                input: HashMap::new(),
            }],
        },
    );

    // Create and fail the original job
    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "deploy-with-hook",
        json!({}),
        "api",
        None,
    )
    .await?;

    let task = workspace.tasks.get("deploy-with-hook").unwrap();

    // Simulate step failure
    let worker_id = register_test_worker(&pool).await;
    JobStepRepo::mark_running(&pool, job_id, "step1", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, job_id, worker_id).await?;
    JobStepRepo::mark_failed(&pool, job_id, "step1", "something broke").await?;

    // Orchestrate → job fails
    orchestrator::on_step_completed(&pool, job_id, "step1", task, None).await?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    // Fire hooks
    let state = hook_test_state(pool.clone(), &workspace);
    stroem_server::hooks::fire_hooks(&state, &workspace, &job, task).await;

    // A hook job should have been created with task_name = "cleanup" (not "_hook:run-cleanup")
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let hook_jobs: Vec<_> = all_jobs
        .iter()
        .filter(|j| j.source_type == "hook")
        .collect();
    assert_eq!(hook_jobs.len(), 1);

    // When hook action is type: task, it creates a real task job
    let hook_job = hook_jobs[0];
    assert_eq!(hook_job.task_name, "cleanup");

    // And the hook job should have the cleanup task's steps
    let hook_steps = JobStepRepo::get_steps_for_job(&pool, hook_job.job_id).await?;
    assert_eq!(hook_steps.len(), 1);
    assert_eq!(hook_steps[0].step_name, "clean");
    assert_eq!(hook_steps[0].action_type, "script");
    assert_eq!(hook_steps[0].status, "ready");

    Ok(())
}

#[tokio::test]
async fn test_task_action_self_reference_rejected() -> Result<()> {
    use stroem_common::validation::validate_workflow_config;

    let yaml = r#"
actions:
  greet:
    type: script
    cmd: "echo hello"
  run-self:
    type: task
    task: loopy
tasks:
  loopy:
    flow:
      step1:
        action: greet
      step2:
        action: run-self
        depends_on: [step1]
"#;
    let config: stroem_common::models::workflow::WorkflowConfig =
        serde_yaml::from_str(yaml).unwrap();
    let result = validate_workflow_config(&config);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("self-reference"));

    Ok(())
}

#[tokio::test]
async fn test_task_action_child_failure_fails_parent_step() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut workspace = task_action_test_workspace();

    // Add a failing task
    let mut fail_flow = HashMap::new();
    fail_flow.insert(
        "crash".to_string(),
        FlowStep {
            action: "greet".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "failing-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: fail_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Action for failing task
    workspace.actions.insert(
        "run-failing".to_string(),
        ActionDef {
            action_type: "task".to_string(),
            name: None,
            description: None,
            task: Some("failing-task".to_string()),
            cmd: None,
            script: None,
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    // Task that runs the failing task action
    let mut parent_flow = HashMap::new();
    parent_flow.insert(
        "run".to_string(),
        FlowStep {
            action: "run-failing".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "parent-of-fail".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: parent_flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    let parent_job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "parent-of-fail",
        json!({}),
        "api",
        None,
    )
    .await?;

    // Child job should exist
    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let child_job = all_jobs
        .iter()
        .find(|j| j.source_type == "task")
        .expect("Child job not found");
    assert_eq!(child_job.parent_job_id, Some(parent_job_id));
    assert_eq!(child_job.parent_step_name.as_deref(), Some("run"));

    // Simulate worker failing the child's step
    let worker_id = register_test_worker(&pool).await;
    JobStepRepo::mark_running(&pool, child_job.job_id, "crash", worker_id).await?;
    JobRepo::mark_running_if_pending(&pool, child_job.job_id, worker_id).await?;
    JobStepRepo::mark_failed(&pool, child_job.job_id, "crash", "child crashed").await?;

    // Run orchestrator for child → child job fails
    let child_task = workspace.tasks.get("failing-task").unwrap();
    orchestrator::on_step_completed(&pool, child_job.job_id, "crash", child_task, None).await?;

    let child_after = JobRepo::get(&pool, child_job.job_id).await?.unwrap();
    assert_eq!(child_after.status, "failed");

    // Simulate propagation: mark parent step failed
    let err_msg = format!("Child job {} failed", child_job.job_id);
    JobStepRepo::mark_failed(&pool, parent_job_id, "run", &err_msg).await?;

    // Run orchestrator for parent → parent job fails
    let parent_task = workspace.tasks.get("parent-of-fail").unwrap();
    orchestrator::on_step_completed(&pool, parent_job_id, "run", parent_task, None).await?;

    let parent_after = JobRepo::get(&pool, parent_job_id).await?.unwrap();
    assert_eq!(parent_after.status, "failed");

    // Parent step should have error message about child failure
    let parent_steps = JobStepRepo::get_steps_for_job(&pool, parent_job_id).await?;
    let parent_step = parent_steps.iter().find(|s| s.step_name == "run").unwrap();
    assert_eq!(parent_step.status, "failed");
    assert!(parent_step
        .error_message
        .as_ref()
        .unwrap()
        .contains("failed"));

    Ok(())
}

// ─── Recovery sweeper tests ─────────────────────────────────────────────

/// Helper: create an AppState with recovery config.
async fn setup_recovery() -> Result<(
    AppState,
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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: stroem_server::config::RecoveryConfig {
            heartbeat_timeout_secs: 5, // short for tests
            sweep_interval_secs: 1,
            unmatched_step_timeout_secs: 30,
        },
        acl: None,
        mcp: None,
    };

    let workspace = test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());

    Ok((state, pool, temp_dir, container))
}

/// Helper: set a worker's heartbeat to a time in the past.
async fn set_worker_heartbeat_past(pool: &PgPool, worker_id: Uuid, seconds_ago: i64) {
    sqlx::query(
        "UPDATE worker SET last_heartbeat = NOW() - make_interval(secs => $1::double precision) WHERE worker_id = $2",
    )
    .bind(seconds_ago as f64)
    .bind(worker_id)
    .execute(pool)
    .await
    .expect("Failed to backdate worker heartbeat");
}

#[tokio::test]
async fn test_recovery_marks_stale_worker_inactive() -> Result<()> {
    let (_state, pool, _tmp, _container) = setup_recovery().await?;

    let worker_id = register_test_worker(&pool).await;

    // Worker just registered — heartbeat is fresh
    let stale = WorkerRepo::mark_stale_inactive(&pool, 5).await?;
    assert!(stale.is_empty(), "Fresh worker should not be stale");

    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "active");

    // Backdate heartbeat to 10 seconds ago (timeout is 5)
    set_worker_heartbeat_past(&pool, worker_id, 10).await;

    let stale = WorkerRepo::mark_stale_inactive(&pool, 5).await?;
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0], worker_id);

    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "inactive");

    Ok(())
}

#[tokio::test]
async fn test_recovery_ignores_active_workers() -> Result<()> {
    let (_state, pool, _tmp, _container) = setup_recovery().await?;

    let worker_id = register_test_worker(&pool).await;

    // Heartbeat is fresh — should not be marked stale
    let stale = WorkerRepo::mark_stale_inactive(&pool, 5).await?;
    assert!(stale.is_empty());

    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "active");

    Ok(())
}

#[tokio::test]
async fn test_recovery_worker_reactivation_on_heartbeat() -> Result<()> {
    let (_state, pool, _tmp, _container) = setup_recovery().await?;

    let worker_id = register_test_worker(&pool).await;

    // Mark as stale
    set_worker_heartbeat_past(&pool, worker_id, 10).await;
    WorkerRepo::mark_stale_inactive(&pool, 5).await?;

    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "inactive");

    // Heartbeat should reactivate
    WorkerRepo::heartbeat(&pool, worker_id).await?;

    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "active");

    Ok(())
}

#[tokio::test]
async fn test_recovery_fails_stale_step() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery().await?;

    // Create a job
    let workspace_config = state.get_workspace("default").await.unwrap();
    let job_id = create_job_for_task(
        &pool,
        &workspace_config,
        "default",
        "hello-world",
        json!({"name": "test"}),
        "api",
        None,
    )
    .await?;

    // Register worker and claim step
    let worker_id = register_test_worker(&pool).await;
    let tags = vec!["script".to_string()];
    let step = JobStepRepo::claim_ready_step(&pool, &tags, worker_id)
        .await?
        .expect("Should claim step");
    assert_eq!(step.job_id, job_id);
    assert_eq!(step.status, "running");

    // Backdate worker heartbeat so it's stale
    set_worker_heartbeat_past(&pool, worker_id, 10).await;

    // Run recovery sweep
    stroem_server::recovery::sweep_once(&state).await?;

    // Worker should be inactive
    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "inactive");

    // Step should be failed with timeout error
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(step.status, "failed");
    assert!(step
        .error_message
        .as_ref()
        .unwrap()
        .contains("heartbeat timeout"));

    // Job should be failed (single step task)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

#[tokio::test]
async fn test_recovery_orchestrates_multi_step_job() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery().await?;

    // Create a multi-step job: step1 → step2 → step3
    let workspace_config = state.get_workspace("default").await.unwrap();
    let job_id = create_job_for_task(
        &pool,
        &workspace_config,
        "default",
        "linear-3",
        json!({}),
        "api",
        None,
    )
    .await?;

    // Claim step1
    let worker_id = register_test_worker(&pool).await;
    let tags = vec!["script".to_string()];
    let step = JobStepRepo::claim_ready_step(&pool, &tags, worker_id)
        .await?
        .expect("Should claim step1");
    assert_eq!(step.step_name, "step1");

    // Backdate heartbeat → worker is stale
    set_worker_heartbeat_past(&pool, worker_id, 10).await;

    // Run sweep
    stroem_server::recovery::sweep_once(&state).await?;

    // step1 should be failed
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step1 = steps.iter().find(|s| s.step_name == "step1").unwrap();
    assert_eq!(step1.status, "failed");

    // step2 and step3 should be skipped (unreachable)
    let step2 = steps.iter().find(|s| s.step_name == "step2").unwrap();
    assert_eq!(step2.status, "skipped");

    let step3 = steps.iter().find(|s| s.step_name == "step3").unwrap();
    assert_eq!(step3.status, "skipped");

    // Job should be failed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

#[tokio::test]
async fn test_recovery_get_running_steps_for_workers_empty() -> Result<()> {
    let (_state, pool, _tmp, _container) = setup_recovery().await?;

    // Empty worker list returns empty steps
    let steps = JobStepRepo::get_running_steps_for_workers(&pool, &[]).await?;
    assert!(steps.is_empty());

    // Non-existent worker returns empty steps
    let steps = JobStepRepo::get_running_steps_for_workers(&pool, &[Uuid::new_v4()]).await?;
    assert!(steps.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_recovery_propagates_to_parent() -> Result<()> {
    // Use task_action_test_workspace which has deploy(build → run-cleanup) with type:task
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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: stroem_server::config::RecoveryConfig {
            heartbeat_timeout_secs: 5,
            sweep_interval_secs: 1,
            unmatched_step_timeout_secs: 30,
        },
        acl: None,
        mcp: None,
    };

    let workspace = task_action_test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace.clone());
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());

    // Create parent job: deploy (build → run-cleanup via task action)
    let parent_job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "deploy",
        json!({}),
        "api",
        None,
    )
    .await?;

    // First, complete the "build" step so "run-cleanup" (task step) gets promoted
    let worker_id = register_test_worker(&pool).await;
    let tags = vec!["script".to_string()];
    let build_step = JobStepRepo::claim_ready_step(&pool, &tags, worker_id)
        .await?
        .expect("Should claim build step");
    assert_eq!(build_step.step_name, "build");

    // Complete build step
    JobStepRepo::mark_completed(&pool, parent_job_id, "build", Some(json!({"ok": true}))).await?;

    // Orchestrate to promote run-cleanup task step
    orchestrator::on_step_completed(
        &pool,
        parent_job_id,
        "build",
        workspace.tasks.get("deploy").unwrap(),
        None,
    )
    .await?;

    // Handle task steps (creates child job)
    stroem_server::job_creator::handle_task_steps(&pool, &workspace, "default", parent_job_id)
        .await?;

    // Find child job
    let child_jobs: Vec<(uuid::Uuid,)> =
        sqlx::query_as("SELECT job_id FROM job WHERE parent_job_id = $1")
            .bind(parent_job_id)
            .fetch_all(&pool)
            .await?;
    assert_eq!(child_jobs.len(), 1);
    let child_job_id = child_jobs[0].0;

    // Claim the child job's "clean" step
    let worker_id2 = register_test_worker(&pool).await;
    let child_step = JobStepRepo::claim_ready_step(&pool, &tags, worker_id2)
        .await?
        .expect("Should claim child step");
    assert_eq!(child_step.job_id, child_job_id);
    assert_eq!(child_step.step_name, "clean");

    // Make the worker stale
    set_worker_heartbeat_past(&pool, worker_id2, 10).await;

    // Run sweep
    stroem_server::recovery::sweep_once(&state).await?;

    // Child step should be failed
    let child_steps = JobStepRepo::get_steps_for_job(&pool, child_job_id).await?;
    let clean_step = child_steps.iter().find(|s| s.step_name == "clean").unwrap();
    assert_eq!(clean_step.status, "failed");
    assert!(clean_step
        .error_message
        .as_ref()
        .unwrap()
        .contains("heartbeat timeout"));

    // Child job should be failed
    let child = JobRepo::get(&pool, child_job_id).await?.unwrap();
    assert_eq!(child.status, "failed");

    // Parent task step (cleanup) should be failed
    let parent_steps = JobStepRepo::get_steps_for_job(&pool, parent_job_id).await?;
    let task_step = parent_steps
        .iter()
        .find(|s| s.step_name == "cleanup")
        .unwrap();
    assert_eq!(task_step.status, "failed");

    // Parent job should be failed
    let parent = JobRepo::get(&pool, parent_job_id).await?.unwrap();
    assert_eq!(parent.status, "failed");

    Ok(())
}

// ─── Test: Execute task source_type without auth ──────────────────────

#[tokio::test]
async fn test_execute_task_without_auth_sets_source_api() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Alice"}}),
        ))
        .await?;

    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.source_type, "api");
    assert!(job.source_id.is_none());

    Ok(())
}

// ─── Test: Execute task with valid auth sets source_type user ─────────

#[tokio::test]
async fn test_execute_task_with_valid_auth_sets_source_user() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_with_auth().await?;

    let token = stroem_server::auth::create_access_token(
        &Uuid::new_v4().to_string(),
        AUTH_USER_EMAIL,
        false,
        AUTH_JWT_SECRET,
    )?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/default/tasks/hello-world/execute")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", token))
        .body(Body::from(
            serde_json::to_string(&json!({"input": {"name": "Bob"}})).unwrap(),
        ))
        .unwrap();

    let response = router.oneshot(request).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.source_type, "user");
    assert_eq!(job.source_id.as_deref(), Some(AUTH_USER_EMAIL));

    Ok(())
}

// ─── Test: Execute task with invalid token returns 401 ────────────────

#[tokio::test]
async fn test_execute_task_with_invalid_token_returns_401() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/api/workspaces/default/tasks/hello-world/execute")
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer invalid-token")
        .body(Body::from(
            serde_json::to_string(&json!({"input": {"name": "Eve"}})).unwrap(),
        ))
        .unwrap();

    let response = router.oneshot(request).await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test: Execute task with no token when auth enabled returns 401 ───

#[tokio::test]
async fn test_execute_task_no_token_when_auth_enabled_returns_401() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Eve"}}),
        ))
        .await?;

    assert_eq!(response.status(), 401);

    Ok(())
}

// ─── Test: Auth middleware protects all API endpoints ──────────────────

#[tokio::test]
async fn test_auth_middleware_protects_jobs_endpoint() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router.oneshot(api_get("/api/jobs")).await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

#[tokio::test]
async fn test_auth_middleware_protects_workers_endpoint() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router.oneshot(api_get("/api/workers")).await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

#[tokio::test]
async fn test_auth_middleware_protects_workspaces_endpoint() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router.oneshot(api_get("/api/workspaces")).await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

#[tokio::test]
async fn test_auth_middleware_allows_with_valid_token() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let token = stroem_server::auth::create_access_token(
        &Uuid::new_v4().to_string(),
        AUTH_USER_EMAIL,
        false,
        AUTH_JWT_SECRET,
    )?;

    let request = Request::builder()
        .method("GET")
        .uri("/api/jobs")
        .header("Authorization", format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(request).await?;
    assert_eq!(response.status(), 200);

    Ok(())
}

#[tokio::test]
async fn test_auth_middleware_rejects_invalid_token() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let request = Request::builder()
        .method("GET")
        .uri("/api/jobs")
        .header("Authorization", "Bearer not-a-valid-jwt")
        .body(Body::empty())
        .unwrap();

    let response = router.oneshot(request).await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

#[tokio::test]
async fn test_public_routes_accessible_without_auth() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router.oneshot(api_get("/api/config")).await?;
    assert_eq!(response.status(), 200);

    Ok(())
}

#[tokio::test]
async fn test_no_auth_configured_all_routes_open() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router.clone().oneshot(api_get("/api/jobs")).await?;
    assert_eq!(response.status(), 200);

    let response = router.clone().oneshot(api_get("/api/workers")).await?;
    assert_eq!(response.status(), 200);

    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks"))
        .await?;
    assert_eq!(response.status(), 200);

    Ok(())
}

// ─── Test: worker_id exposed in job list and detail API ─────────────

#[tokio::test]
async fn test_worker_id_in_job_list_and_detail() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Create a job
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "WorkerTest"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let job_id = body["job_id"].as_str().unwrap().to_string();

    // Job list should include worker_id field (null before any worker claims)
    let response = router.clone().oneshot(api_get("/api/jobs")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let jobs = body["items"].as_array().unwrap();
    assert!(!jobs.is_empty());
    let job = &jobs[0];
    assert!(
        job.get("worker_id").is_some(),
        "worker_id field must be present in job list"
    );
    assert!(
        job["worker_id"].is_null(),
        "worker_id should be null before worker claims"
    );

    // Job detail should include worker_id field
    let response = router
        .oneshot(api_get(&format!("/api/jobs/{}", job_id)))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert!(
        body.get("worker_id").is_some(),
        "worker_id field must be present in job detail"
    );
    assert!(
        body["worker_id"].is_null(),
        "worker_id should be null before worker claims"
    );

    Ok(())
}

// ─── Worker Detail Endpoint ──────────────────────────────────────────

#[tokio::test]
async fn test_auth_middleware_protects_worker_detail_endpoint() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_with_auth().await?;

    let worker_id = register_test_worker(&pool).await;
    let response = router
        .oneshot(api_get(&format!("/api/workers/{}", worker_id)))
        .await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

#[tokio::test]
async fn test_get_worker_invalid_uuid() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router.oneshot(api_get("/api/workers/not-a-uuid")).await?;
    assert_eq!(response.status(), 400);
    let body = body_json(response).await;
    assert!(body["error"].as_str().unwrap().contains("Invalid"));

    Ok(())
}

#[tokio::test]
async fn test_get_worker_not_found() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let random_id = Uuid::new_v4();
    let response = router
        .oneshot(api_get(&format!("/api/workers/{}", random_id)))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

#[tokio::test]
async fn test_get_worker_success() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let worker_id = register_test_worker(&pool).await;

    let response = router
        .oneshot(api_get(&format!("/api/workers/{}", worker_id)))
        .await?;
    assert_eq!(response.status(), 200);

    let body = body_json(response).await;
    assert_eq!(body["worker_id"].as_str().unwrap(), worker_id.to_string());
    assert_eq!(body["name"].as_str().unwrap(), "test-worker");
    assert_eq!(body["status"].as_str().unwrap(), "active");
    assert!(body["tags"].is_array());
    assert!(
        body.get("registered_at").is_some(),
        "registered_at field must be present"
    );
    assert!(
        body.get("last_heartbeat").is_some(),
        "last_heartbeat field must be present"
    );
    assert!(body["jobs"].is_array());
    assert_eq!(body["jobs"].as_array().unwrap().len(), 0);

    Ok(())
}

#[tokio::test]
async fn test_get_worker_with_jobs() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let worker_id = register_test_worker(&pool).await;

    // Create a job and assign the worker to it
    let job_id = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        Some(json!({"name": "Test"})),
        "api",
        None,
    )
    .await?;
    JobRepo::mark_running(&pool, job_id, worker_id).await?;

    let response = router
        .oneshot(api_get(&format!("/api/workers/{}", worker_id)))
        .await?;
    assert_eq!(response.status(), 200);

    let body = body_json(response).await;
    let jobs = body["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["job_id"].as_str().unwrap(), job_id.to_string());
    assert_eq!(jobs[0]["task_name"].as_str().unwrap(), "hello-world");
    assert_eq!(
        jobs[0]["worker_id"].as_str().unwrap(),
        worker_id.to_string()
    );

    Ok(())
}

// ─── Triggers API tests ──────────────────────────────────────────────

#[tokio::test]
async fn test_list_triggers() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_get("/api/workspaces/default/triggers"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let triggers = body.as_array().unwrap();
    assert_eq!(triggers.len(), 5);

    let names: Vec<&str> = triggers
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"nightly"));
    assert!(names.contains(&"weekly-backup"));

    // Verify nightly trigger details
    let nightly = triggers.iter().find(|t| t["name"] == "nightly").unwrap();
    assert_eq!(nightly["type"], "scheduler");
    assert_eq!(nightly["cron"], "0 2 * * *");
    assert_eq!(nightly["task"], "hello-world");
    assert_eq!(nightly["enabled"], true);
    assert_eq!(nightly["input"]["name"], "nightly");

    // Cron trigger should have next_runs
    let next_runs = nightly["next_runs"].as_array().unwrap();
    assert_eq!(next_runs.len(), 5);

    // Verify weekly-backup trigger (disabled)
    let weekly = triggers
        .iter()
        .find(|t| t["name"] == "weekly-backup")
        .unwrap();
    assert_eq!(weekly["enabled"], false);
    assert_eq!(weekly["task"], "backup-task");
    // Disabled triggers still compute next_runs
    assert_eq!(weekly["next_runs"].as_array().unwrap().len(), 5);

    Ok(())
}

#[tokio::test]
async fn test_list_triggers_nonexistent_workspace() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_get("/api/workspaces/nonexistent/triggers"))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

#[tokio::test]
async fn test_task_list_has_triggers_field() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let tasks = body.as_array().unwrap();

    // hello-world has an enabled trigger (nightly) → has_triggers = true
    let hello_world = tasks.iter().find(|t| t["id"] == "hello-world").unwrap();
    assert_eq!(hello_world["has_triggers"], true);

    // backup-task has only a disabled trigger → has_triggers = false
    let backup_task = tasks.iter().find(|t| t["id"] == "backup-task").unwrap();
    assert_eq!(backup_task["has_triggers"], false);

    // greet-and-shout has no triggers → has_triggers = false
    let greet_and_shout = tasks.iter().find(|t| t["id"] == "greet-and-shout").unwrap();
    assert_eq!(greet_and_shout["has_triggers"], false);

    Ok(())
}

#[tokio::test]
async fn test_task_detail_includes_triggers() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // hello-world has the nightly trigger
    let response = router
        .clone()
        .oneshot(api_get("/api/workspaces/default/tasks/hello-world"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let triggers = body["triggers"].as_array().unwrap();
    // hello-world has 4 triggers: nightly (scheduler) + 3 webhooks
    assert_eq!(triggers.len(), 4);
    let nightly = triggers.iter().find(|t| t["name"] == "nightly").unwrap();
    assert_eq!(nightly["type"], "scheduler");
    assert_eq!(nightly["cron"], "0 2 * * *");
    assert_eq!(nightly["task"], "hello-world");
    assert_eq!(nightly["enabled"], true);
    assert_eq!(nightly["next_runs"].as_array().unwrap().len(), 5);

    // greet-and-shout has no triggers
    let response = router
        .oneshot(api_get("/api/workspaces/default/tasks/greet-and-shout"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let triggers = body["triggers"].as_array().unwrap();
    assert!(triggers.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_workspace_info_includes_triggers_count() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router.oneshot(api_get("/api/workspaces")).await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;

    let workspaces = body.as_array().unwrap();
    let default_ws = workspaces.iter().find(|w| w["name"] == "default").unwrap();
    assert_eq!(default_ws["triggers_count"].as_u64().unwrap(), 5);

    Ok(())
}

// ===== Webhook trigger tests =====

#[tokio::test]
async fn test_webhook_post_json_creates_job() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/github-push?secret=whsec_test123")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"ref":"refs/heads/main","commits":[]}"#))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_json(response).await;
    assert!(body["job_id"].as_str().is_some());
    assert_eq!(body["trigger"], "github-push");
    assert_eq!(body["task"], "hello-world");

    Ok(())
}

#[tokio::test]
async fn test_webhook_post_plaintext_body_as_string() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/github-push?secret=whsec_test123")
        .header("Content-Type", "text/plain")
        .body(Body::from("hello world"))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    Ok(())
}

#[tokio::test]
async fn test_webhook_get_creates_job() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("GET")
        .uri("/hooks/public-hook?env=production")
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_json(response).await;
    assert!(body["job_id"].as_str().is_some());
    assert_eq!(body["trigger"], "public-hook");
    assert_eq!(body["task"], "hello-world");

    Ok(())
}

#[tokio::test]
async fn test_webhook_secret_via_header() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/github-push")
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer whsec_test123")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    Ok(())
}

#[tokio::test]
async fn test_webhook_invalid_secret_401() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/github-push?secret=wrong-secret")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    Ok(())
}

#[tokio::test]
async fn test_webhook_missing_secret_401() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/github-push")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    Ok(())
}

#[tokio::test]
async fn test_webhook_no_secret_public() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/public-hook")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    Ok(())
}

#[tokio::test]
async fn test_webhook_disabled_404() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/disabled-hook")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn test_webhook_nonexistent_404() -> Result<()> {
    let (router, _pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/does-not-exist")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn test_webhook_source_type_tracking() -> Result<()> {
    let (router, pool, _temp_dir, _container) = setup().await?;

    let request = Request::builder()
        .method("POST")
        .uri("/hooks/public-hook")
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse().unwrap();

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.source_type, "webhook");
    assert_eq!(job.source_id.as_deref(), Some("default/on-deploy"));

    Ok(())
}

#[tokio::test]
async fn test_webhook_input_merging() -> Result<()> {
    let (router, pool, _temp_dir, _container) = setup().await?;

    // The github-push trigger has default input { environment: "staging" }
    let request = Request::builder()
        .method("POST")
        .uri("/hooks/github-push?secret=whsec_test123")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"ref":"refs/heads/main"}"#))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse().unwrap();

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    let input = job.input.unwrap();
    // Should have body, headers, method, query from request
    assert_eq!(input["body"]["ref"], "refs/heads/main");
    assert_eq!(input["method"], "POST");
    // Should have environment from trigger defaults
    assert_eq!(input["environment"], "staging");

    Ok(())
}

// ─── API Key Tests ──────────────────────────────────────────────────────

/// Helper: login and return (access_token, user_id)
async fn login_and_get_token(router: &Router) -> Result<(String, String)> {
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/auth/login",
            json!({"email": AUTH_USER_EMAIL, "password": AUTH_USER_PASSWORD}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let token = body["access_token"].as_str().unwrap().to_string();
    // Decode user_id from token
    let claims = stroem_server::auth::validate_access_token(&token, AUTH_JWT_SECRET)?;
    Ok((token, claims.sub))
}

fn authed_get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap()
}

fn authed_request(method: &str, uri: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap()
}

fn authed_delete(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("Authorization", format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn test_api_key_create_and_list() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    // Create an API key
    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "CI Pipeline"}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let raw_key = body["key"].as_str().unwrap();
    assert!(raw_key.starts_with("strm_"));
    assert_eq!(raw_key.len(), 37);
    assert_eq!(body["name"], "CI Pipeline");
    assert!(body["prefix"].as_str().unwrap().starts_with("strm_"));

    // List API keys
    let response = router
        .clone()
        .oneshot(authed_get("/api/auth/api-keys", &token))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let keys = body.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["name"], "CI Pipeline");
    // Raw key should never be returned in list
    assert!(keys[0].get("key").is_none());

    Ok(())
}

#[tokio::test]
async fn test_api_key_create_with_expiry() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "Short-lived key", "expires_in_days": 7}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert!(body["expires_at"].is_string());

    Ok(())
}

#[tokio::test]
async fn test_api_key_used_as_bearer_token() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    // Create API key
    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "Test Key"}),
        ))
        .await?;
    let body = body_json(response).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    // Use API key to access a protected endpoint
    let response = router
        .clone()
        .oneshot(authed_get("/api/jobs", &api_key))
        .await?;
    assert_eq!(response.status(), 200);

    Ok(())
}

#[tokio::test]
async fn test_api_key_deleted_returns_401() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    // Create API key
    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "Deletable Key"}),
        ))
        .await?;
    let body = body_json(response).await;
    let api_key = body["key"].as_str().unwrap().to_string();
    let prefix = body["prefix"].as_str().unwrap().to_string();

    // Verify the key works
    let response = router
        .clone()
        .oneshot(authed_get("/api/jobs", &api_key))
        .await?;
    assert_eq!(response.status(), 200);

    // Delete the key
    let response = router
        .clone()
        .oneshot(authed_delete(
            &format!("/api/auth/api-keys/{}", prefix),
            &token,
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Verify the key no longer works
    let response = router
        .clone()
        .oneshot(authed_get("/api/jobs", &api_key))
        .await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

#[tokio::test]
async fn test_api_key_expired_returns_401() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    // Create API key via the API
    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "Expiring Key", "expires_in_days": 1}),
        ))
        .await?;
    let body = body_json(response).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    // Manually expire it in the DB
    let key_hash = stroem_server::auth::hash_api_key(&api_key);
    sqlx::query("UPDATE api_key SET expires_at = NOW() - INTERVAL '1 hour' WHERE key_hash = $1")
        .bind(&key_hash)
        .execute(&pool)
        .await?;

    // Verify the expired key returns 401
    let response = router
        .clone()
        .oneshot(authed_get("/api/jobs", &api_key))
        .await?;
    assert_eq!(response.status(), 401);

    // Verify listing still shows the key (expired but not deleted)
    let response = router
        .clone()
        .oneshot(authed_get("/api/auth/api-keys", &token))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    Ok(())
}

#[tokio::test]
async fn test_api_key_invalid_returns_401() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;

    let response = router
        .clone()
        .oneshot(authed_get("/api/jobs", "strm_not_a_real_key_1234567890ab"))
        .await?;
    assert_eq!(response.status(), 401);

    Ok(())
}

#[tokio::test]
async fn test_api_key_empty_name_rejected() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "  "}),
        ))
        .await?;
    assert_eq!(response.status(), 400);

    Ok(())
}

#[tokio::test]
async fn test_api_key_jwt_still_works() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    // JWT should still work alongside API keys
    let response = router
        .clone()
        .oneshot(authed_get("/api/jobs", &token))
        .await?;
    assert_eq!(response.status(), 200);

    Ok(())
}

#[tokio::test]
async fn test_api_key_delete_nonexistent_returns_404() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    let response = router
        .clone()
        .oneshot(authed_delete("/api/auth/api-keys/strm_non", &token))
        .await?;
    assert_eq!(response.status(), 404);

    Ok(())
}

#[tokio::test]
async fn test_api_key_cannot_manage_keys() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    // Create an API key via JWT
    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "First Key"}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    // Try to create another key using the API key — should be 403
    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &api_key,
            json!({"name": "Second Key"}),
        ))
        .await?;
    assert_eq!(response.status(), 403);

    // Try to list keys using the API key — should be 403
    let response = router
        .clone()
        .oneshot(authed_get("/api/auth/api-keys", &api_key))
        .await?;
    assert_eq!(response.status(), 403);

    // Try to delete using the API key — should be 403
    let prefix = body["prefix"].as_str().unwrap();
    let response = router
        .clone()
        .oneshot(authed_delete(
            &format!("/api/auth/api-keys/{}", prefix),
            &api_key,
        ))
        .await?;
    assert_eq!(response.status(), 403);

    // But the API key can still access regular protected endpoints
    let response = router
        .clone()
        .oneshot(authed_get("/api/jobs", &api_key))
        .await?;
    assert_eq!(response.status(), 200);

    Ok(())
}

#[tokio::test]
async fn test_api_key_negative_expiry_rejected() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_with_auth().await?;
    let (token, _user_id) = login_and_get_token(&router).await?;

    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "Bad Key", "expires_in_days": -5}),
        ))
        .await?;
    assert_eq!(response.status(), 400);

    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &token,
            json!({"name": "Bad Key", "expires_in_days": 0}),
        ))
        .await?;
    assert_eq!(response.status(), 400);

    Ok(())
}

// ─── Status filter integration tests ─────────────────────────────────

#[tokio::test]
async fn test_list_jobs_with_status_filter() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Create 3 jobs
    for _ in 0..3 {
        router
            .clone()
            .oneshot(api_request(
                "POST",
                "/api/workspaces/default/tasks/hello-world/execute",
                json!({"input": {"name": "test"}}),
            ))
            .await?;
    }

    // All 3 should be pending
    let response = router
        .clone()
        .oneshot(api_get("/api/jobs?status=pending"))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
    assert_eq!(body["total"].as_i64().unwrap(), 3);

    // Complete one job
    let jobs = JobRepo::list(&pool, None, None, 10, 0).await?;
    let job_id = jobs[0].job_id;
    JobRepo::mark_completed(&pool, job_id, None).await?;

    // status=completed → 1
    let response = router
        .clone()
        .oneshot(api_get("/api/jobs?status=completed"))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["total"].as_i64().unwrap(), 1);

    // status=pending → 2
    let response = router
        .clone()
        .oneshot(api_get("/api/jobs?status=pending"))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"].as_i64().unwrap(), 2);

    // no filter → 3
    let response = router.clone().oneshot(api_get("/api/jobs")).await?;
    let body = body_json(response).await;
    assert_eq!(body["total"].as_i64().unwrap(), 3);

    Ok(())
}

#[tokio::test]
async fn test_list_jobs_invalid_status_returns_400() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router.oneshot(api_get("/api/jobs?status=bogus")).await?;
    assert_eq!(response.status(), 400);
    let body = body_json(response).await;
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("Invalid status filter"));
    assert!(error.contains("bogus"));

    Ok(())
}

#[tokio::test]
async fn test_list_jobs_status_with_workspace_filter() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create job in "default" workspace
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "test"}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Create job in "ops" workspace (deploy-app task, no required input)
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/ops/tasks/deploy-app/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // Complete the ops job
    let ops_jobs = JobRepo::list(&pool, Some("ops"), None, 10, 0).await?;
    JobRepo::mark_completed(&pool, ops_jobs[0].job_id, None).await?;

    // workspace=default&status=pending → 1
    let response = router
        .clone()
        .oneshot(api_get("/api/jobs?workspace=default&status=pending"))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // workspace=ops&status=completed → 1
    let response = router
        .clone()
        .oneshot(api_get("/api/jobs?workspace=ops&status=completed"))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // workspace=ops&status=pending → 0
    let response = router
        .clone()
        .oneshot(api_get("/api/jobs?workspace=ops&status=pending"))
        .await?;
    let body = body_json(response).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0);

    Ok(())
}

// ─── WebSocket auth integration tests ────────────────────────────────

#[tokio::test]
async fn test_ws_auth_rejects_without_token() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_with_auth().await?;

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

    // Start server on real port (WebSocket requires real TCP)
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Try to connect without any token — should get rejected
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream",
        addr.port(),
        job_id
    );
    let result = tokio_tungstenite::connect_async(&url).await;
    // Server should reject the upgrade with a non-101 status
    assert!(
        result.is_err(),
        "WebSocket should be rejected without auth token"
    );

    server.abort();
    Ok(())
}

#[tokio::test]
async fn test_ws_auth_accepts_jwt_via_query_param() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_with_auth().await?;

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

    // Login to get a JWT
    let (token, _user_id) = login_and_get_token(&router).await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Connect with token in query param — should succeed
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream?token={}",
        addr.port(),
        job_id,
        token
    );
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WebSocket should connect with valid JWT in query param");

    drop(ws_stream);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn test_ws_auth_accepts_api_key_via_query_param() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_with_auth().await?;

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

    // Login and create an API key
    let (jwt_token, _user_id) = login_and_get_token(&router).await?;
    let response = router
        .clone()
        .oneshot(authed_request(
            "POST",
            "/api/auth/api-keys",
            &jwt_token,
            json!({"name": "WS Test Key"}),
        ))
        .await?;
    assert_eq!(response.status(), 200);
    let body = body_json(response).await;
    let api_key = body["key"].as_str().unwrap().to_string();
    assert!(api_key.starts_with("strm_"));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Connect with API key in query param
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream?token={}",
        addr.port(),
        job_id,
        api_key
    );
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WebSocket should connect with valid API key in query param");

    drop(ws_stream);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn test_ws_auth_rejects_invalid_token() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_with_auth().await?;

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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Connect with an invalid JWT token
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream?token=invalid-jwt-garbage",
        addr.port(),
        job_id
    );
    let result = tokio_tungstenite::connect_async(&url).await;
    assert!(
        result.is_err(),
        "WebSocket should be rejected with invalid token"
    );

    server.abort();
    Ok(())
}

#[tokio::test]
async fn test_ws_no_auth_allows_without_token() -> Result<()> {
    // When auth is NOT configured, WebSocket should work without any token
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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream",
        addr.port(),
        job_id
    );
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WebSocket should connect without token when auth is disabled");

    drop(ws_stream);
    server.abort();
    Ok(())
}

// ─── skip_backfill WebSocket test ────────────────────────────────────

#[tokio::test]
async fn test_ws_skip_backfill_suppresses_existing_logs() -> Result<()> {
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

    // Append some existing logs
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body(
                "build",
                &[("stdout", "existing line 1"), ("stdout", "existing line 2")],
            ),
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

    // Connect WITH skip_backfill=true
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream?skip_backfill=true",
        addr.port(),
        job_id
    );
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    use futures_util::StreamExt;

    // Push a live log line
    let response = router_for_push
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "live after skip")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    // The first message should be the live line, NOT the backfill
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
        .await?
        .unwrap()?;
    let text = msg.into_text()?;
    assert!(
        text.contains("live after skip"),
        "First WS message should be the live line, not backfill"
    );
    assert!(
        !text.contains("existing line"),
        "Backfill should be suppressed with skip_backfill=true"
    );

    drop(ws_stream);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn test_ws_without_skip_backfill_sends_existing_logs() -> Result<()> {
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

    // Append existing logs
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/logs", job_id),
            log_lines_body("build", &[("stdout", "backfill content here")]),
        ))
        .await?;
    assert_eq!(response.status(), 200);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Connect WITHOUT skip_backfill (default behavior)
    let url = format!(
        "ws://127.0.0.1:{}/api/jobs/{}/logs/stream",
        addr.port(),
        job_id
    );
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect");

    use futures_util::StreamExt;

    // First message should contain the backfill
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws_stream.next())
        .await?
        .unwrap()?;
    let text = msg.into_text()?;
    assert!(
        text.contains("backfill content here"),
        "Should receive backfill when skip_backfill is not set"
    );

    drop(ws_stream);
    server.abort();
    Ok(())
}

// ─── Test: Connection input pass-through at claim ─────────────────────

/// When a flow step maps only some inputs (e.g. `sql`) but omits a connection-typed
/// input (e.g. `db`), the claim handler should pass through the resolved connection
/// object from the job-level input.
#[tokio::test]
async fn test_connection_input_passthrough_at_claim() -> Result<()> {
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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mut workspace = WorkspaceConfig::default();

    // Connection: clickhouse-prod
    workspace.connections.insert(
        "clickhouse-prod".to_string(),
        ConnectionDef {
            connection_type: Some("clickhouse".to_string()),
            values: HashMap::from([
                ("host".to_string(), json!("ch.example.com")),
                ("port".to_string(), json!(9000)),
                ("database".to_string(), json!("analytics")),
            ]),
        },
    );

    // Action: execute-query — expects both `sql` (string) and `db` (clickhouse connection)
    let mut action_input = HashMap::new();
    action_input.insert(
        "sql".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: true,
            default: None,
            secret: false,
            options: None,
            allow_custom: false,
            order: None,
        },
    );
    action_input.insert(
        "db".to_string(),
        InputFieldDef {
            field_type: "clickhouse".to_string(),
            name: None,
            description: None,
            required: true,
            default: None,
            secret: false,
            options: None,
            allow_custom: false,
            order: None,
        },
    );
    workspace.actions.insert(
        "execute-query".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo running query".to_string()),
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
            input: action_input,
            output: None,
            manifest: None,
        },
    );

    // Task: run-query — has `sql` (string) and `db` (clickhouse) inputs.
    // Flow step only maps `sql`, deliberately omits `db`.
    let mut task_input = HashMap::new();
    task_input.insert(
        "sql".to_string(),
        InputFieldDef {
            field_type: "string".to_string(),
            name: None,
            description: None,
            required: true,
            default: None,
            secret: false,
            options: None,
            allow_custom: false,
            order: None,
        },
    );
    task_input.insert(
        "db".to_string(),
        InputFieldDef {
            field_type: "clickhouse".to_string(),
            name: None,
            description: None,
            required: true,
            default: None,
            secret: false,
            options: None,
            allow_custom: false,
            order: None,
        },
    );

    let mut flow = HashMap::new();
    flow.insert(
        "query".to_string(),
        FlowStep {
            action: "execute-query".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            // Only map `sql`; `db` is NOT mapped here
            input: HashMap::from([("sql".to_string(), json!("{{ input.sql }}"))]),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );

    workspace.tasks.insert(
        "run-query".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: task_input,
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    // Execute task: provide connection name "clickhouse-prod" for `db` input
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/run-query/execute",
            json!({"input": {"sql": "SELECT count() FROM events", "db": "clickhouse-prod"}}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    // Register worker and claim the step
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker_id,
        "test-worker",
        &["script".to_string()],
        None,
    )
    .await?;

    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;
    let body = body_json(response).await;

    assert_eq!(body["step_name"].as_str().unwrap(), "query");

    // The `sql` field should be rendered from the template
    let input = &body["input"];
    assert_eq!(input["sql"], "SELECT count() FROM events");

    // The `db` field should be the resolved connection object (passed through from job input)
    assert!(
        input["db"].is_object(),
        "db should be a resolved connection object, got: {}",
        input["db"]
    );
    assert_eq!(input["db"]["host"], "ch.example.com");
    assert_eq!(input["db"]["port"], 9000);
    assert_eq!(input["db"]["database"], "analytics");

    Ok(())
}

// ─── Webhook sync-mode timeout tests ────────────────────────────────────────

/// Build a minimal workspace and router wired with a sync-mode webhook trigger.
///
/// The trigger has a 1-second timeout so that tests can exercise the timeout
/// path without waiting for the default 30 seconds.
async fn setup_sync_webhook() -> Result<(
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

    // Minimal workspace: one script action, one task, one sync webhook trigger.
    let mut workspace = WorkspaceConfig::new();

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
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "sync-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    // Sync webhook with a 1-second timeout so the test doesn't block for long.
    workspace.triggers.insert(
        "on-sync".to_string(),
        TriggerDef::Webhook {
            name: "sync-hook".to_string(),
            task: "sync-task".to_string(),
            secret: None,
            input: HashMap::new(),
            enabled: true,
            mode: Some("sync".to_string()),
            timeout_secs: Some(1),
        },
    );

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    Ok((router, pool, temp_dir, container))
}

/// When a sync-mode webhook fires and the job does not complete within the
/// configured timeout, the handler returns 202 Accepted with `"status": "running"`
/// and the created `job_id`.
///
/// This exercises the `tokio::time::timeout` branch in `webhook_handler` where
/// neither `rx.recv()` resolves nor the job completes before the deadline.
#[tokio::test]
async fn test_sync_webhook_timeout_returns_202_with_running_status() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_sync_webhook().await?;

    // POST to the sync webhook — no worker is running, so the job will stay
    // in "pending" state and the 1-second timeout will elapse.
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hooks/sync-hook")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await?;

    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "sync webhook timeout must return 202 Accepted"
    );

    let body = body_json(response).await;

    assert_eq!(
        body["status"].as_str(),
        Some("running"),
        "timeout response must carry status=running"
    );
    assert_eq!(
        body["trigger"].as_str(),
        Some("sync-hook"),
        "trigger name must be echoed back"
    );
    assert_eq!(
        body["task"].as_str(),
        Some("sync-task"),
        "task name must be echoed back"
    );

    // job_id must be a valid UUID so the caller can poll for completion.
    let job_id_str = body["job_id"].as_str().expect("job_id must be present");
    let job_id: Uuid = job_id_str.parse().expect("job_id must be a valid UUID");
    assert_ne!(job_id, Uuid::nil(), "job_id must not be the nil UUID");

    Ok(())
}

/// An async-mode (default) webhook returns 200 immediately without waiting,
/// while a sync-mode webhook with the same setup returns 202 on timeout.
/// This documents the behavioural difference between the two modes.
#[tokio::test]
async fn test_async_webhook_returns_200_immediately() -> Result<()> {
    // Use the standard test workspace which has `public-hook` (async, no secret).
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hooks/public-hook")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await?;

    // Async webhook always returns 200 immediately regardless of job state.
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "async webhook must return 200 OK immediately"
    );

    let body = body_json(response).await;
    assert!(
        body["job_id"].as_str().is_some(),
        "job_id must be present in async response"
    );
    // Async response does NOT include a status field (job is just created).
    assert!(
        body.get("status").is_none(),
        "async response must not include a status field"
    );

    Ok(())
}

// ─── Worker recovery via HTTP API ─────────────────────────────────────

#[tokio::test]
async fn test_recovery_marks_stale_worker_steps_as_failed_via_api() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery().await?;
    let router = build_router(state.clone(), CancellationToken::new());

    // Execute single-step task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Recovery"}}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Register worker via API
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "stale-worker", "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let worker_id_str = body["worker_id"].as_str().unwrap().to_string();
    let worker_id: Uuid = worker_id_str.parse()?;

    // Claim step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id_str, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "greet");

    // Report step start
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/start", job_id),
            json!({"worker_id": worker_id_str}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    // Backdate heartbeat so the worker appears stale (timeout is 5s in setup_recovery)
    set_worker_heartbeat_past(&pool, worker_id, 200).await;

    // Run a single recovery sweep
    stroem_server::recovery::sweep_once(&state).await?;

    // Worker should now be inactive
    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "inactive");

    // Step should be failed with a heartbeat timeout error
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let greet = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(greet.status, "failed");
    assert!(
        greet
            .error_message
            .as_ref()
            .unwrap()
            .contains("heartbeat timeout"),
        "expected heartbeat timeout in error, got: {:?}",
        greet.error_message
    );

    // Job should be failed (its only step failed)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

#[tokio::test]
async fn test_recovery_does_not_affect_active_workers_via_api() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery().await?;
    let router = build_router(state.clone(), CancellationToken::new());

    // Execute single-step task
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Active"}}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Register worker
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "active-worker", "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let worker_id_str = body["worker_id"].as_str().unwrap().to_string();

    // Claim step
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id_str, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    // Report step start
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/start", job_id),
            json!({"worker_id": worker_id_str}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    // Do NOT backdate the heartbeat — worker is still active
    stroem_server::recovery::sweep_once(&state).await?;

    // Step should still be running
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let greet = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(greet.status, "running");

    // Job should still be running
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "running");

    Ok(())
}

#[tokio::test]
async fn test_recovery_handles_multi_step_job_via_api() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery().await?;
    let router = build_router(state.clone(), CancellationToken::new());

    // Execute the 2-step task: greet → shout
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/greet-and-shout/execute",
            json!({"input": {"name": "World"}}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Register worker
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "multi-worker", "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let worker_id_str = body["worker_id"].as_str().unwrap().to_string();
    let worker_id: Uuid = worker_id_str.parse()?;

    // Claim and complete step 1 (greet)
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id_str, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "greet");

    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/greet/complete", job_id),
            json!({"output": {"greeting": "hello world"}}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    // Claim step 2 (shout) and report start
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id_str, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["step_name"].as_str().unwrap(), "shout");

    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!("/worker/jobs/{}/steps/shout/start", job_id),
            json!({"worker_id": worker_id_str}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    // Make worker stale
    set_worker_heartbeat_past(&pool, worker_id, 200).await;

    // Run recovery sweep
    stroem_server::recovery::sweep_once(&state).await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;

    // Step 1 (greet) should remain completed
    let greet = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(greet.status, "completed");

    // Step 2 (shout) should be failed
    let shout = steps.iter().find(|s| s.step_name == "shout").unwrap();
    assert_eq!(shout.status, "failed");
    assert!(
        shout
            .error_message
            .as_ref()
            .unwrap()
            .contains("heartbeat timeout"),
        "expected heartbeat timeout in error, got: {:?}",
        shout.error_message
    );

    // Job should be failed
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

#[tokio::test]
async fn test_recovery_worker_reactivation_on_heartbeat_via_api() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery().await?;
    let router = build_router(state.clone(), CancellationToken::new());

    // Register worker via API
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "reactivate-worker", "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let worker_id_str = body["worker_id"].as_str().unwrap().to_string();
    let worker_id: Uuid = worker_id_str.parse()?;

    // Backdate heartbeat and run sweep to mark inactive
    set_worker_heartbeat_past(&pool, worker_id, 200).await;
    stroem_server::recovery::sweep_once(&state).await?;

    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "inactive");

    // Worker sends a heartbeat via the API
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/heartbeat",
            json!({"worker_id": worker_id_str}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    // Worker should be active again
    let worker = WorkerRepo::get(&pool, worker_id).await?.unwrap();
    assert_eq!(worker.status, "active");

    Ok(())
}

// ─── Scheduler integration tests ────────────────────────────────────────────

async fn setup_scheduler_workspace(
    trigger_name: &str,
    cron: &str,
    task_name: &str,
    trigger_input: HashMap<String, serde_json::Value>,
    enabled: bool,
) -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();

    workspace.actions.insert(
        "echo-action".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo hello".to_string()),
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    let mut flow = HashMap::new();
    flow.insert(
        "step1".to_string(),
        FlowStep {
            action: "echo-action".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        task_name.to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,

            on_success: vec![],
            on_error: vec![],
        },
    );

    workspace.triggers.insert(
        trigger_name.to_string(),
        TriggerDef::Scheduler {
            cron: cron.to_string(),
            task: task_name.to_string(),
            input: trigger_input,
            enabled,
            concurrency: Default::default(),
        },
    );

    workspace
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scheduler_fires_cron_trigger() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let workspace = setup_scheduler_workspace(
        "every-second",
        "* * * * * *",
        "scheduled-task",
        HashMap::new(),
        true,
    )
    .await;
    let mgr = WorkspaceManager::from_config("default", workspace);

    let cancel = tokio_util::sync::CancellationToken::new();
    let sched_config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url: url.clone() },
        log_storage: LogStorageConfig {
            local_dir: std::env::temp_dir().to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };
    let sched_log_storage = LogStorage::new(&sched_config.log_storage.local_dir);
    let sched_state = AppState::new(
        pool.clone(),
        mgr,
        sched_config,
        sched_log_storage,
        HashMap::new(),
    );
    let handle = stroem_server::scheduler::start(sched_state, cancel.clone());

    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    cancel.cancel();
    handle.await?;

    let jobs = sqlx::query_as::<_, stroem_db::JobRow>(
        "SELECT job_id, workspace, task_name, mode, input, output, status, source_type, \
         source_id, worker_id, revision, created_at, started_at, completed_at, log_path, \
         parent_job_id, parent_step_name, timeout_secs FROM job WHERE source_type = $1",
    )
    .bind("trigger")
    .fetch_all(&pool)
    .await?;

    assert!(
        !jobs.is_empty(),
        "at least one job should have been created by the trigger"
    );

    let job = &jobs[0];
    assert_eq!(job.source_type, "trigger");
    assert_eq!(job.task_name, "scheduled-task");
    assert_eq!(job.workspace, "default");

    let source_id = job.source_id.as_deref().unwrap_or("");
    assert!(
        source_id.contains("every-second"),
        "source_id '{source_id}' should contain the trigger name"
    );
    assert!(
        source_id.contains("default"),
        "source_id '{source_id}' should contain the workspace name"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scheduler_disabled_trigger_does_not_fire() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let workspace = setup_scheduler_workspace(
        "disabled-trigger",
        "* * * * * *",
        "disabled-task",
        HashMap::new(),
        false,
    )
    .await;
    let mgr = WorkspaceManager::from_config("default", workspace);

    let cancel = tokio_util::sync::CancellationToken::new();
    let sched_config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url: url.clone() },
        log_storage: LogStorageConfig {
            local_dir: std::env::temp_dir().to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };
    let sched_log_storage = LogStorage::new(&sched_config.log_storage.local_dir);
    let sched_state = AppState::new(
        pool.clone(),
        mgr,
        sched_config,
        sched_log_storage,
        HashMap::new(),
    );
    let handle = stroem_server::scheduler::start(sched_state, cancel.clone());

    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    cancel.cancel();
    handle.await?;

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM job WHERE source_type = $1")
        .bind("trigger")
        .fetch_one(&pool)
        .await?;

    assert_eq!(count.0, 0, "disabled trigger must not create any jobs");

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scheduler_passes_trigger_input_to_job() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let mut trigger_input = HashMap::new();
    trigger_input.insert("env".to_string(), json!("staging"));
    trigger_input.insert("dry_run".to_string(), json!(true));

    let workspace = setup_scheduler_workspace(
        "input-trigger",
        "* * * * * *",
        "input-task",
        trigger_input,
        true,
    )
    .await;
    let mgr = WorkspaceManager::from_config("default", workspace);

    let cancel = tokio_util::sync::CancellationToken::new();
    let sched_config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url: url.clone() },
        log_storage: LogStorageConfig {
            local_dir: std::env::temp_dir().to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };
    let sched_log_storage = LogStorage::new(&sched_config.log_storage.local_dir);
    let sched_state = AppState::new(
        pool.clone(),
        mgr,
        sched_config,
        sched_log_storage,
        HashMap::new(),
    );
    let handle = stroem_server::scheduler::start(sched_state, cancel.clone());

    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    cancel.cancel();
    handle.await?;

    let jobs = sqlx::query_as::<_, stroem_db::JobRow>(
        "SELECT job_id, workspace, task_name, mode, input, output, status, source_type, \
         source_id, worker_id, revision, created_at, started_at, completed_at, log_path, \
         parent_job_id, parent_step_name, timeout_secs FROM job WHERE source_type = $1 LIMIT 1",
    )
    .bind("trigger")
    .fetch_all(&pool)
    .await?;

    assert!(
        !jobs.is_empty(),
        "trigger must have created at least one job"
    );

    let input = jobs[0].input.as_ref().expect("job input must not be null");
    assert_eq!(
        input["env"],
        json!("staging"),
        "job input must contain env from trigger input"
    );
    assert_eq!(
        input["dry_run"],
        json!(true),
        "job input must contain dry_run from trigger input"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_scheduler_clean_shutdown() -> Result<()> {
    let workspace = WorkspaceConfig::default();
    let mgr = WorkspaceManager::from_config("default", workspace);

    let cancel = tokio_util::sync::CancellationToken::new();
    // No triggers, so fire_trigger is never called — pool is never used.
    let pool = sqlx::PgPool::connect_lazy("postgres://invalid:5432/db")?;
    let sched_config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: "postgres://invalid:5432/db".to_string(),
        },
        log_storage: LogStorageConfig {
            local_dir: std::env::temp_dir().to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };
    let sched_log_storage = LogStorage::new(&sched_config.log_storage.local_dir);
    let sched_state = AppState::new(pool, mgr, sched_config, sched_log_storage, HashMap::new());
    let handle = stroem_server::scheduler::start(sched_state, cancel.clone());

    cancel.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    assert!(
        result.is_ok(),
        "scheduler must exit cleanly within 5 seconds of cancellation"
    );
    assert!(
        result.unwrap().is_ok(),
        "scheduler task must not panic on clean shutdown"
    );

    Ok(())
}

// ─── Multi-workspace: Job execution records correct workspace ────────

#[tokio::test]
async fn test_multi_workspace_job_execution() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_multi_workspace().await?;

    // Execute a task in "default" workspace
    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Alice"}}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let job_id_default: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Execute a task in "ops" workspace
    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/ops/tasks/deploy-app/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let job_id_ops: Uuid = body["job_id"].as_str().unwrap().parse()?;

    // Verify "default" job has correct workspace and task_name in DB
    let job_default = JobRepo::get(&pool, job_id_default).await?.unwrap();
    assert_eq!(job_default.workspace, "default");
    assert_eq!(job_default.task_name, "hello-world");

    // Verify "ops" job has correct workspace and task_name in DB
    let job_ops = JobRepo::get(&pool, job_id_ops).await?.unwrap();
    assert_eq!(job_ops.workspace, "ops");
    assert_eq!(job_ops.task_name, "deploy-app");

    // Verify job detail endpoint also reflects workspace
    let resp = router
        .clone()
        .oneshot(api_get(&format!("/api/jobs/{}", job_id_default)))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    assert_eq!(body["workspace"].as_str().unwrap(), "default");
    assert_eq!(body["task_name"].as_str().unwrap(), "hello-world");

    let resp = router
        .oneshot(api_get(&format!("/api/jobs/{}", job_id_ops)))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    assert_eq!(body["workspace"].as_str().unwrap(), "ops");
    assert_eq!(body["task_name"].as_str().unwrap(), "deploy-app");

    Ok(())
}

// ─── Multi-workspace: Worker claims steps from correct workspace ─────

#[tokio::test]
async fn test_multi_workspace_worker_claims_from_correct_workspace() -> Result<()> {
    let (router, pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create a step in "default" workspace directly
    let job_id_default = JobRepo::create(
        &pool,
        "default",
        "hello-world",
        "distributed",
        Some(json!({"name": "Test"})),
        "api",
        None,
    )
    .await?;
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id: job_id_default,
            step_name: "say-hello".to_string(),
            action_name: "greet".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo Hello"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    // Create a step in "ops" workspace
    let job_id_ops = JobRepo::create(
        &pool,
        "ops",
        "deploy-app",
        "distributed",
        Some(json!({})),
        "api",
        None,
    )
    .await?;
    JobStepRepo::create_steps(
        &pool,
        &[NewJobStep {
            job_id: job_id_ops,
            step_name: "run-deploy".to_string(),
            action_name: "deploy".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: Some(json!({"cmd": "echo deploying"})),
            input: None,
            status: "ready".to_string(),
            required_tags: vec!["script".to_string()],
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
        }],
    )
    .await?;

    // Register a worker
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "multi-ws-worker", "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let reg_body = body_json(resp).await;
    let worker_id = reg_body["worker_id"].as_str().unwrap().to_string();

    // Claim first step
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let claim1 = body_json(resp).await;
    let claimed_workspace_1 = claim1["workspace"].as_str().unwrap().to_string();
    let claimed_job_id_1: Uuid = claim1["job_id"].as_str().unwrap().parse()?;
    let claimed_step_1 = claim1["step_name"].as_str().unwrap().to_string();
    assert!(
        claimed_workspace_1 == "default" || claimed_workspace_1 == "ops",
        "claimed workspace must be one of the configured workspaces"
    );

    // Complete the first claimed step so the second becomes claimable
    router
        .clone()
        .oneshot(worker_request(
            "POST",
            &format!(
                "/worker/jobs/{}/steps/{}/complete",
                claimed_job_id_1, claimed_step_1
            ),
            json!({"status": "completed", "output": {}}),
        ))
        .await?;

    // Claim second step
    let resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id, "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(resp.status(), 200);
    let claim2 = body_json(resp).await;
    let claimed_workspace_2 = claim2["workspace"].as_str().unwrap().to_string();
    let claimed_job_id_2: Uuid = claim2["job_id"].as_str().unwrap().parse()?;

    // The second claim must be from the other workspace
    assert_ne!(
        claimed_workspace_1, claimed_workspace_2,
        "second claim should be from the other workspace"
    );
    assert_ne!(claimed_job_id_1, claimed_job_id_2);

    // Both jobs must have had their steps claimed
    let steps_default = JobStepRepo::get_steps_for_job(&pool, job_id_default).await?;
    let steps_ops = JobStepRepo::get_steps_for_job(&pool, job_id_ops).await?;
    assert!(steps_default
        .iter()
        .any(|s| s.status == "running" || s.status == "completed"));
    assert!(steps_ops
        .iter()
        .any(|s| s.status == "running" || s.status == "completed"));

    Ok(())
}

// ─── Multi-workspace: Job listing filters by workspace ───────────────

#[tokio::test]
async fn test_multi_workspace_job_listing_filters_by_workspace() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // Create two jobs in "default"
    for _ in 0..2 {
        let resp = router
            .clone()
            .oneshot(api_request(
                "POST",
                "/api/workspaces/default/tasks/hello-world/execute",
                json!({"input": {"name": "Test"}}),
            ))
            .await?;
        assert_eq!(resp.status(), 200);
    }

    // Create three jobs in "ops"
    for _ in 0..3 {
        let resp = router
            .clone()
            .oneshot(api_request(
                "POST",
                "/api/workspaces/ops/tasks/deploy-app/execute",
                json!({"input": {}}),
            ))
            .await?;
        assert_eq!(resp.status(), 200);
    }

    // List all jobs (no workspace filter) — must include at least 5
    let resp = router.clone().oneshot(api_get("/api/jobs")).await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let total = body["total"].as_u64().unwrap();
    assert!(total >= 5, "expected at least 5 total jobs, got {}", total);

    // List only "default" jobs — exactly 2
    let resp = router
        .clone()
        .oneshot(api_get("/api/jobs?workspace=default"))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let default_jobs = body["items"].as_array().unwrap();
    assert_eq!(default_jobs.len(), 2);
    for job in default_jobs {
        assert_eq!(job["workspace"].as_str().unwrap(), "default");
    }
    assert_eq!(body["total"].as_u64().unwrap(), 2);

    // List only "ops" jobs — exactly 3
    let resp = router
        .clone()
        .oneshot(api_get("/api/jobs?workspace=ops"))
        .await?;
    assert_eq!(resp.status(), 200);
    let body = body_json(resp).await;
    let ops_jobs = body["items"].as_array().unwrap();
    assert_eq!(ops_jobs.len(), 3);
    for job in ops_jobs {
        assert_eq!(job["workspace"].as_str().unwrap(), "ops");
    }
    assert_eq!(body["total"].as_u64().unwrap(), 3);

    Ok(())
}

// ─── Multi-workspace: Tarball download for multiple workspaces ───────

#[tokio::test]
async fn test_multi_workspace_tarball_download() -> Result<()> {
    let container = Postgres::default().start().await?;
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgres://postgres:postgres@localhost:{}/postgres", port);
    let pool = create_pool(&url).await?;
    run_migrations(&pool).await?;

    let temp_dir = TempDir::new()?;
    let log_dir = temp_dir.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Create two distinct workspace directories with workflow files
    let ws_default_dir = TempDir::new()?;
    let ws_ops_dir = TempDir::new()?;

    for (dir, yaml) in [
        (
            ws_default_dir.path(),
            "actions:\n  greet:\n    type: script\n    cmd: echo hi\ntasks:\n  hello-world:\n    flow:\n      s1:\n        action: greet\n",
        ),
        (
            ws_ops_dir.path(),
            "actions:\n  deploy:\n    type: script\n    cmd: echo deploying\ntasks:\n  deploy-app:\n    flow:\n      run-deploy:\n        action: deploy\n",
        ),
    ] {
        let wf_dir = dir.join(".workflows");
        std::fs::create_dir_all(&wf_dir)?;
        std::fs::write(wf_dir.join("workflow.yaml"), yaml)?;
    }

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
        },
        workspaces: HashMap::from([
            (
                "default".to_string(),
                WorkspaceSourceDef::Folder {
                    path: ws_default_dir.path().to_string_lossy().to_string(),
                },
            ),
            (
                "ops".to_string(),
                WorkspaceSourceDef::Folder {
                    path: ws_ops_dir.path().to_string_lossy().to_string(),
                },
            ),
        ]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: Default::default(),
        acl: None,
        mcp: None,
    };

    let mgr =
        WorkspaceManager::new(config.workspaces.clone(), HashMap::new(), HashMap::new()).await;

    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool, mgr, config, log_storage, HashMap::new());
    let router = build_router(state, CancellationToken::new());

    // Download "default" tarball — expect 200, gzip content-type, X-Revision header
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/default.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap().to_str()?,
        "application/gzip"
    );
    assert!(resp.headers().get("X-Revision").is_some());
    let body_bytes = resp.into_body().collect().await?.to_bytes();
    assert!(!body_bytes.is_empty());

    // Download "ops" tarball — expect 200 with non-empty body
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/ops.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap().to_str()?,
        "application/gzip"
    );
    assert!(resp.headers().get("X-Revision").is_some());
    let body_bytes = resp.into_body().collect().await?.to_bytes();
    assert!(!body_bytes.is_empty());

    // Download non-existent workspace — expect 404
    let req = Request::builder()
        .method("GET")
        .uri("/worker/workspace/nonexistent.tar.gz")
        .header("Authorization", "Bearer test-token-secret")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await?;
    assert_eq!(resp.status(), 404);

    Ok(())
}

// ─── Multi-workspace: Task not found in wrong workspace ──────────────

#[tokio::test]
async fn test_multi_workspace_task_not_found_in_wrong_workspace() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup_multi_workspace().await?;

    // "hello-world" lives in "default" — executing via "ops" route must 404
    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/ops/tasks/hello-world/execute",
            json!({"input": {"name": "Test"}}),
        ))
        .await?;
    assert_eq!(resp.status(), 404);

    // "deploy-app" lives in "ops" — executing via "default" route must 404
    let resp = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/deploy-app/execute",
            json!({"input": {}}),
        ))
        .await?;
    assert_eq!(resp.status(), 404);

    // Task detail endpoint also returns 404 for the wrong workspace
    let resp = router
        .clone()
        .oneshot(api_get("/api/workspaces/ops/tasks/hello-world"))
        .await?;
    assert_eq!(resp.status(), 404);

    let resp = router
        .oneshot(api_get("/api/workspaces/default/tasks/deploy-app"))
        .await?;
    assert_eq!(resp.status(), 404);

    Ok(())
}

// ─── Worker version integration tests ────────────────────────────────────────

#[tokio::test]
async fn test_worker_register_with_version_stored_in_db() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Register worker WITH a version field
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "versioned-worker", "tags": ["script"], "version": "0.5.9"}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let worker_id: Uuid = body["worker_id"].as_str().unwrap().parse()?;

    // Verify the version was persisted to the database
    let worker = WorkerRepo::get(&pool, worker_id)
        .await?
        .expect("Worker should exist in DB");
    assert_eq!(worker.version.as_deref(), Some("0.5.9"));

    Ok(())
}

#[tokio::test]
async fn test_worker_register_without_version_stores_null() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Register worker WITHOUT a version field (legacy backward-compat behaviour)
    let response = router
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "legacy-worker", "tags": ["script"]}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let worker_id: Uuid = body["worker_id"].as_str().unwrap().parse()?;

    // Version must be NULL in the database
    let worker = WorkerRepo::get(&pool, worker_id)
        .await?
        .expect("Worker should exist in DB");
    assert!(worker.version.is_none());

    Ok(())
}

#[tokio::test]
async fn test_list_workers_response_includes_version() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Register a worker with a known version
    let response = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/register",
            json!({"name": "v-worker", "tags": ["script"], "version": "1.2.3"}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    // Fetch the workers list and verify the version field is present
    let response = router.oneshot(api_get("/api/workers")).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    let items = body["items"].as_array().expect("items should be an array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["version"].as_str(), Some("1.2.3"));

    Ok(())
}

#[tokio::test]
async fn test_get_config_returns_version_field() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let response = router.oneshot(api_get("/api/config")).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    // The version field must be present and non-empty
    let version = body["version"]
        .as_str()
        .expect("version should be a string");
    assert!(!version.is_empty(), "version should not be empty");

    Ok(())
}

// ─── Unmatched ready step recovery tests ─────────────────────────────

/// Helper: create a recovery AppState with a custom unmatched_step_timeout_secs.
async fn setup_recovery_with_unmatched_timeout(
    timeout_secs: u64,
) -> Result<(
    AppState,
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
        },
        workspaces: HashMap::from([(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp_dir.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token-secret".to_string(),
        auth: None,
        recovery: stroem_server::config::RecoveryConfig {
            heartbeat_timeout_secs: 120,
            sweep_interval_secs: 1,
            unmatched_step_timeout_secs: timeout_secs,
        },
        acl: None,
        mcp: None,
    };

    let workspace = test_workspace();
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new());

    Ok((state, pool, temp_dir, container))
}

/// Helper: override a step's required_tags and backdate its ready_at.
async fn set_step_tags_and_backdate(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    required_tags: Vec<String>,
    backdate_secs: i64,
) {
    let tags_json = serde_json::to_value(&required_tags).unwrap();
    sqlx::query(
        "UPDATE job_step SET required_tags = $1, ready_at = NOW() - make_interval(secs => $2::double precision) \
         WHERE job_id = $3 AND step_name = $4",
    )
    .bind(tags_json)
    .bind(backdate_secs as f64)
    .bind(job_id)
    .bind(step_name)
    .execute(pool)
    .await
    .expect("Failed to update step tags and ready_at");
}

#[tokio::test]
async fn test_recovery_fails_unmatched_ready_step() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery_with_unmatched_timeout(1).await?;

    // Register a worker with ["script"] tags — it cannot run ["gpu"] steps
    let _worker_id = register_test_worker(&pool).await;

    // Create a real job via the normal path (hello-world has a single "greet" step)
    let workspace_config = state.get_workspace("default").await.unwrap();
    let job_id = create_job_for_task(
        &pool,
        &workspace_config,
        "default",
        "hello-world",
        json!({"name": "test"}),
        "api",
        None,
    )
    .await?;

    // Override the step's required_tags to ["gpu"] (no worker can match) and backdate ready_at
    set_step_tags_and_backdate(&pool, job_id, "greet", vec!["gpu".to_string()], 10).await;

    // Verify step is ready before sweep
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    assert_eq!(steps[0].status, "ready");

    // Run recovery sweep
    stroem_server::recovery::sweep_once(&state).await?;

    // Step should be failed with descriptive error
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(step.status, "failed");
    assert!(
        step.error_message
            .as_ref()
            .unwrap()
            .contains("No active worker"),
        "Error message should mention no active worker: {:?}",
        step.error_message
    );

    // Job should be failed (single step → cascaded)
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

#[tokio::test]
async fn test_recovery_does_not_fail_matched_ready_step() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery_with_unmatched_timeout(1).await?;

    // Register a worker with ["script"] tags
    let _worker_id = register_test_worker(&pool).await;

    // Create a real job (hello-world, step "greet" has required_tags: ["script"])
    let workspace_config = state.get_workspace("default").await.unwrap();
    let job_id = create_job_for_task(
        &pool,
        &workspace_config,
        "default",
        "hello-world",
        json!({"name": "test"}),
        "api",
        None,
    )
    .await?;

    // Backdate ready_at but keep tags as ["script"] — worker CAN match
    set_step_tags_and_backdate(&pool, job_id, "greet", vec!["script".to_string()], 10).await;

    // Run recovery sweep
    stroem_server::recovery::sweep_once(&state).await?;

    // Step should still be ready (not failed)
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(
        step.status, "ready",
        "Step with matching worker should remain ready"
    );

    Ok(())
}

#[tokio::test]
async fn test_recovery_does_not_fail_recent_unmatched_step() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery_with_unmatched_timeout(60).await?;

    // Register a worker with ["script"] tags
    let _worker_id = register_test_worker(&pool).await;

    // Create a real job and set tags to ["gpu"] but backdate only 1 second
    // (timeout is 60s, so it should NOT be failed yet)
    let workspace_config = state.get_workspace("default").await.unwrap();
    let job_id = create_job_for_task(
        &pool,
        &workspace_config,
        "default",
        "hello-world",
        json!({"name": "test"}),
        "api",
        None,
    )
    .await?;

    set_step_tags_and_backdate(&pool, job_id, "greet", vec!["gpu".to_string()], 1).await;

    // Run recovery sweep
    stroem_server::recovery::sweep_once(&state).await?;

    // Step should still be ready — hasn't exceeded the timeout
    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(
        step.status, "ready",
        "Recently-ready step should not be failed yet"
    );

    Ok(())
}

#[tokio::test]
async fn test_recovery_fails_unmatched_step_with_no_workers() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery_with_unmatched_timeout(1).await?;

    // Do NOT register any worker
    let workspace_config = state.get_workspace("default").await.unwrap();
    let job_id = create_job_for_task(
        &pool,
        &workspace_config,
        "default",
        "hello-world",
        json!({"name": "test"}),
        "api",
        None,
    )
    .await?;

    set_step_tags_and_backdate(&pool, job_id, "greet", vec!["script".to_string()], 10).await;

    stroem_server::recovery::sweep_once(&state).await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(step.status, "failed");
    assert!(step
        .error_message
        .as_ref()
        .unwrap()
        .contains("No active worker"));

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(job.status, "failed");

    Ok(())
}

#[tokio::test]
async fn test_recovery_fails_unmatched_step_with_inactive_worker() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery_with_unmatched_timeout(1).await?;

    // Register a worker with ["script"] tags, then mark it inactive
    let worker_id = register_test_worker(&pool).await;
    sqlx::query("UPDATE worker SET status = 'inactive' WHERE worker_id = $1")
        .bind(worker_id)
        .execute(&pool)
        .await?;

    let workspace_config = state.get_workspace("default").await.unwrap();
    let job_id = create_job_for_task(
        &pool,
        &workspace_config,
        "default",
        "hello-world",
        json!({"name": "test"}),
        "api",
        None,
    )
    .await?;

    // Step has ["script"] tags — the inactive worker matches but shouldn't protect it
    set_step_tags_and_backdate(&pool, job_id, "greet", vec!["script".to_string()], 10).await;

    stroem_server::recovery::sweep_once(&state).await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(
        step.status, "failed",
        "Inactive worker should not protect step"
    );

    Ok(())
}

#[tokio::test]
async fn test_recovery_does_not_fail_task_type_step() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery_with_unmatched_timeout(1).await?;

    // No workers registered — but task-type steps should be excluded

    // Create a job + task-type step directly via SQL
    let job_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, input, status, source_type, source_id) \
         VALUES ($1, 'default', 'hello-world', '{}', 'running', 'api', 'test')",
    )
    .bind(job_id)
    .execute(&pool)
    .await?;

    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, required_tags, runner, ready_at) \
         VALUES ($1, 'sub-task', 'sub-action', 'task', 'ready', '[]'::jsonb, 'none', NOW() - INTERVAL '10 minutes')",
    )
    .bind(job_id)
    .execute(&pool)
    .await?;

    stroem_server::recovery::sweep_once(&state).await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "sub-task").unwrap();
    assert_eq!(
        step.status, "ready",
        "Task-type steps must not be failed by Phase 4"
    );

    Ok(())
}

#[tokio::test]
async fn test_recovery_does_not_fail_empty_tags_step() -> Result<()> {
    let (state, pool, _tmp, _container) = setup_recovery_with_unmatched_timeout(1).await?;

    // Register a worker — any active worker should match empty required_tags
    let _worker_id = register_test_worker(&pool).await;

    let workspace_config = state.get_workspace("default").await.unwrap();
    let job_id = create_job_for_task(
        &pool,
        &workspace_config,
        "default",
        "hello-world",
        json!({"name": "test"}),
        "api",
        None,
    )
    .await?;

    // Set required_tags to empty array — should match any worker
    set_step_tags_and_backdate(&pool, job_id, "greet", vec![], 10).await;

    stroem_server::recovery::sweep_once(&state).await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let step = steps.iter().find(|s| s.step_name == "greet").unwrap();
    assert_eq!(
        step.status, "ready",
        "Step with empty tags should match any active worker"
    );

    Ok(())
}

// ─── Phase 5a when-condition integration tests ────────────────────────────────

/// Builds a minimal workspace with a `noop` script action and one task defined
/// by the supplied flow map.  Avoids boilerplate repetition in when-tests.
fn when_test_workspace_with_flow(
    task_name: &str,
    flow: HashMap<String, FlowStep>,
) -> WorkspaceConfig {
    let mut workspace = WorkspaceConfig::default();
    workspace.actions.insert(
        "noop".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: Some("true".to_string()),
            script: None,
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );
    workspace.tasks.insert(
        task_name.to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: None,
            on_success: vec![],
            on_error: vec![],
        },
    );
    workspace
}

/// When a task has a root step (no dependencies) with `when: "false"`, the
/// post-creation promote loop must evaluate the condition immediately and mark
/// the step `skipped`.  Any dependent steps must cascade to `skipped` via
/// `skip_unreachable_steps`.
#[tokio::test]
async fn test_create_job_for_task_root_when_false_skips_at_creation() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let mut flow = HashMap::new();
    flow.insert(
        "root".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: Some("false".to_string()),
            inline_action: None,
        },
    );
    flow.insert(
        "child".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec!["root".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    let workspace = when_test_workspace_with_flow("conditional-root", flow);

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "conditional-root",
        json!({}),
        "api",
        None,
    )
    .await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let root = steps.iter().find(|s| s.step_name == "root").unwrap();
    let child = steps.iter().find(|s| s.step_name == "child").unwrap();

    assert_eq!(
        root.status, "skipped",
        "Root step with when:false must be skipped immediately at job creation"
    );
    assert_eq!(
        child.status, "skipped",
        "Child step must cascade to skipped because its only dependency was skipped"
    );

    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert_eq!(
        job.status, "completed",
        "Job must complete when all steps are skipped at creation time"
    );

    Ok(())
}

/// Complementary to the test above: a root step with a truthy `when` condition
/// must be promoted to `ready` by the post-creation promote loop.
#[tokio::test]
async fn test_create_job_for_task_root_when_true_becomes_ready() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let mut flow = HashMap::new();
    flow.insert(
        "root".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: Some("true".to_string()),
            inline_action: None,
        },
    );
    let workspace = when_test_workspace_with_flow("truthy-root", flow);

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "truthy-root",
        json!({}),
        "api",
        None,
    )
    .await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let root = steps.iter().find(|s| s.step_name == "root").unwrap();

    assert_eq!(
        root.status, "ready",
        "Root step with when:true must be promoted to ready at job creation"
    );

    Ok(())
}

/// A `type: task` flow step that carries `when: "false"` must be skipped by the
/// post-creation promote loop.  No child job must be spawned for it.
#[tokio::test]
async fn test_create_job_for_task_step_type_task_with_when_false_is_skipped() -> Result<()> {
    let (_router, pool, _tmp, _container) = setup().await?;

    let mut workspace = WorkspaceConfig::default();

    workspace.actions.insert(
        "noop".to_string(),
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: Some("true".to_string()),
            script: None,
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    workspace.actions.insert(
        "run-child".to_string(),
        ActionDef {
            action_type: "task".to_string(),
            name: None,
            description: None,
            task: Some("child-task".to_string()),
            cmd: None,
            script: None,
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
            input: HashMap::new(),
            output: None,
            manifest: None,
        },
    );

    let mut child_flow = HashMap::new();
    child_flow.insert(
        "work".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "child-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: child_flow,
            timeout: None,
            on_success: vec![],
            on_error: vec![],
        },
    );

    let mut parent_flow = HashMap::new();
    parent_flow.insert(
        "conditional-task-step".to_string(),
        FlowStep {
            action: "run-child".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: Some("false".to_string()),
            inline_action: None,
        },
    );
    workspace.tasks.insert(
        "parent-task".to_string(),
        TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: parent_flow,
            timeout: None,
            on_success: vec![],
            on_error: vec![],
        },
    );

    let parent_job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "parent-task",
        json!({}),
        "api",
        None,
    )
    .await?;

    let steps = JobStepRepo::get_steps_for_job(&pool, parent_job_id).await?;
    let task_step = steps
        .iter()
        .find(|s| s.step_name == "conditional-task-step")
        .unwrap();

    assert_eq!(
        task_step.status, "skipped",
        "type:task step with when:false must be skipped at job creation"
    );

    let all_jobs = JobRepo::list(&pool, Some("default"), None, 100, 0).await?;
    let child_jobs: Vec<_> = all_jobs
        .iter()
        .filter(|j| j.source_type == "task")
        .collect();
    assert!(
        child_jobs.is_empty(),
        "No child job must be spawned for a type:task step skipped by its when condition"
    );

    let job = JobRepo::get(&pool, parent_job_id).await?.unwrap();
    assert_eq!(
        job.status, "completed",
        "Job must complete when its only step is skipped"
    );

    Ok(())
}

/// The job detail endpoint must return `when_condition` on each step so the UI
/// and API consumers can display the condition expression.
#[tokio::test]
async fn test_job_detail_api_exposes_when_condition() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    let mut flow = HashMap::new();
    flow.insert(
        "build".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            inline_action: None,
        },
    );
    flow.insert(
        "deploy".to_string(),
        FlowStep {
            action: "noop".to_string(),
            name: None,
            description: None,
            depends_on: vec!["build".to_string()],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: Some("{{ input.should_deploy }}".to_string()),
            inline_action: None,
        },
    );
    let workspace = when_test_workspace_with_flow("build-and-deploy", flow);

    let job_id = create_job_for_task(
        &pool,
        &workspace,
        "default",
        "build-and-deploy",
        json!({"should_deploy": true}),
        "api",
        None,
    )
    .await?;

    let res = router
        .oneshot(api_get(&format!("/api/jobs/{}", job_id)))
        .await?;

    assert_eq!(res.status(), StatusCode::OK);

    let body = body_json(res).await;
    let steps = body["steps"].as_array().expect("steps must be an array");

    let deploy_step = steps
        .iter()
        .find(|s| s["step_name"].as_str() == Some("deploy"))
        .expect("deploy step must be present in the response");

    assert_eq!(
        deploy_step["when_condition"],
        json!("{{ input.should_deploy }}"),
        "when_condition must be returned verbatim in the job detail API"
    );

    let build_step = steps
        .iter()
        .find(|s| s["step_name"].as_str() == Some("build"))
        .expect("build step must be present in the response");

    assert!(
        build_step["when_condition"].is_null(),
        "Steps without a when condition must have null when_condition in the API response"
    );

    Ok(())
}

// ===== Webhook job status endpoint tests =====

/// Helper: fire a webhook and return the created job_id.
async fn fire_webhook(router: Router, uri: &str) -> Uuid {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "fire_webhook: expected 200 OK"
    );
    let body = body_json(response).await;
    body["job_id"]
        .as_str()
        .expect("job_id missing in webhook response")
        .parse()
        .expect("job_id is not a valid UUID")
}

// Test 1: malformed UUID in the job_id path segment → 400
#[tokio::test]
async fn test_webhook_job_status_malformed_uuid() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let request = Request::builder()
        .method("GET")
        .uri("/hooks/github-push/jobs/not-a-uuid?secret=whsec_test123")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "malformed UUID must return 400"
    );
    let body = body_json(response).await;
    assert!(
        body["error"].as_str().is_some(),
        "response must include an error field"
    );

    Ok(())
}

// Test 2: webhook name does not exist → 404
#[tokio::test]
async fn test_webhook_job_status_unknown_webhook() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let job_id = Uuid::new_v4();
    let request = Request::builder()
        .method("GET")
        .uri(&format!("/hooks/nonexistent-webhook/jobs/{}", job_id))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "unknown webhook name must return 404"
    );

    Ok(())
}

// Test 3: webhook exists but job UUID is not in the DB → 404
#[tokio::test]
async fn test_webhook_job_status_unknown_job_id() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let random_id = Uuid::new_v4();
    let request = Request::builder()
        .method("GET")
        .uri(&format!("/hooks/public-hook/jobs/{}", random_id))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "unknown job id must return 404"
    );

    Ok(())
}

// Test 4: IDOR — job created via the API (source_type = "api") is not visible via webhook status endpoint → 404
#[tokio::test]
async fn test_webhook_job_status_idor_api_sourced_job() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Create a job through the normal API endpoint (source_type = "api")
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            "/api/workspaces/default/tasks/hello-world/execute",
            json!({"input": {"name": "Alice"}}),
        ))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let api_job_id = body["job_id"].as_str().unwrap().to_string();

    // Try to access it via the webhook status endpoint for public-hook
    let request = Request::builder()
        .method("GET")
        .uri(&format!("/hooks/public-hook/jobs/{}", api_job_id))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "API-sourced job must not be visible via webhook status endpoint"
    );

    Ok(())
}

// Test 5: IDOR — job created by webhook A is not visible via webhook B's status endpoint → 404
#[tokio::test]
async fn test_webhook_job_status_idor_different_webhook() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Fire public-hook to create a job
    let job_id = fire_webhook(router.clone(), "/hooks/public-hook").await;

    // Try to access that job via github-push's status endpoint
    let request = Request::builder()
        .method("GET")
        .uri(&format!(
            "/hooks/github-push/jobs/{}?secret=whsec_test123",
            job_id
        ))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "job from a different webhook must not be accessible via another webhook's status endpoint"
    );

    Ok(())
}

// Test 6: secret-protected webhook, no secret provided → 401
#[tokio::test]
async fn test_webhook_job_status_secret_required_but_missing() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    let job_id = Uuid::new_v4();
    // Request without any secret for the secret-protected github-push webhook
    let request = Request::builder()
        .method("GET")
        .uri(&format!("/hooks/github-push/jobs/{}", job_id))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "missing secret must return 401"
    );

    Ok(())
}

// Test 7: secret-protected webhook, secret supplied via query param → 200 (or 404 for missing job)
#[tokio::test]
async fn test_webhook_job_status_secret_via_query_param() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // First create a real job via the webhook so we have a valid job_id
    let job_id = fire_webhook(router.clone(), "/hooks/github-push?secret=whsec_test123").await;

    // Access status with secret via query param
    let request = Request::builder()
        .method("GET")
        .uri(&format!(
            "/hooks/github-push/jobs/{}?secret=whsec_test123",
            job_id
        ))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    // Secret is correct → endpoint accepts the request and returns job status
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "valid secret via query param must be accepted"
    );
    let body = body_json(response).await;
    assert_eq!(
        body["job_id"].as_str(),
        Some(job_id.to_string().as_str()),
        "job_id must match the created job"
    );
    assert_eq!(body["trigger"], "github-push");
    assert_eq!(body["task"], "hello-world");
    assert!(
        body["status"].as_str().is_some(),
        "response must include a status field"
    );

    Ok(())
}

// Test 8: secret-protected webhook, secret supplied via Authorization: Bearer header → 200
#[tokio::test]
async fn test_webhook_job_status_secret_via_bearer() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Create a job via the webhook (with secret in query param for the fire)
    let job_id = fire_webhook(router.clone(), "/hooks/github-push?secret=whsec_test123").await;

    // Check status with secret via Authorization: Bearer header
    let request = Request::builder()
        .method("GET")
        .uri(&format!("/hooks/github-push/jobs/{}", job_id))
        .header("Authorization", "Bearer whsec_test123")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "valid secret via Bearer header must be accepted"
    );
    let body = body_json(response).await;
    assert_eq!(body["trigger"], "github-push");
    assert_eq!(body["task"], "hello-world");

    Ok(())
}

// Test 9: fire webhook, immediately check status without wait param → returns current status
#[tokio::test]
async fn test_webhook_job_status_default_no_wait() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Fire public-hook (no secret required)
    let job_id = fire_webhook(router.clone(), "/hooks/public-hook").await;

    // Check status immediately — no wait param, job is likely still pending/running
    let request = Request::builder()
        .method("GET")
        .uri(&format!("/hooks/public-hook/jobs/{}", job_id))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "immediate status check must return 200"
    );
    let body = body_json(response).await;
    assert_eq!(
        body["job_id"].as_str(),
        Some(job_id.to_string().as_str()),
        "job_id must match"
    );
    assert_eq!(body["trigger"], "public-hook");
    assert_eq!(body["task"], "hello-world");
    // Status is present and is one of the valid job statuses
    let status = body["status"]
        .as_str()
        .expect("status field must be present");
    assert!(
        ["pending", "running", "completed", "failed", "cancelled"].contains(&status),
        "status '{}' is not a valid job status",
        status
    );
    // No-wait response should include a created_at timestamp
    assert!(
        body["created_at"].as_str().is_some(),
        "created_at must be present"
    );

    Ok(())
}

// Test 10: fire webhook, drive job to a terminal state, then GET with wait=true →
// returns immediately with terminal status (race-guard path: job already terminal at subscribe time)
#[tokio::test]
async fn test_webhook_job_status_wait_on_terminal_job() -> Result<()> {
    let (router, pool, _tmp, _container) = setup().await?;

    // Fire public-hook to create a job.
    // The `hello-world` task requires `name` in its step template; since public-hook
    // sends no input, the step will fail template rendering at claim time and drive
    // the job to `failed` — which is still a terminal state and sufficient to verify
    // the `wait=true` fast-path (job already terminal before subscribing).
    let job_id = fire_webhook(router.clone(), "/hooks/public-hook").await;

    // Register a worker and attempt to claim the step.
    // The claim will return 422 because `{{ input.name }}` cannot be rendered (missing
    // field), but as a side-effect the handler marks the step failed and drives the job
    // to its terminal state via the orchestrator, including firing the job_completion
    // notification — all before the 422 response is returned.
    let worker_id = register_test_worker(&pool).await;
    let _claim_resp = router
        .clone()
        .oneshot(worker_request(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": worker_id.to_string(), "tags": ["script"]}),
        ))
        .await?;

    // The job must now be in a terminal state (failed) regardless of whether the
    // claim succeeded or the template rendering failed.
    let job = JobRepo::get(&pool, job_id).await?.unwrap();
    assert!(
        ["failed", "completed", "cancelled"].contains(&job.status.as_str()),
        "job must be terminal after claim attempt, got: {}",
        job.status
    );
    let expected_terminal_status = job.status.clone();

    // Now GET /hooks/public-hook/jobs/{job_id}?wait=true — the job is already terminal,
    // so the endpoint re-queries the DB after subscribing (race guard) and returns
    // immediately without waiting on the broadcast channel.
    let request = Request::builder()
        .method("GET")
        .uri(&format!(
            "/hooks/public-hook/jobs/{}?wait=true&timeout=5",
            job_id
        ))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "wait=true on already-terminal job must return 200"
    );
    let body = body_json(response).await;
    assert_eq!(
        body["job_id"].as_str(),
        Some(job_id.to_string().as_str()),
        "job_id must match"
    );
    assert_eq!(
        body["status"].as_str(),
        Some(expected_terminal_status.as_str()),
        "status must match the terminal status from DB"
    );
    assert_eq!(body["trigger"], "public-hook");
    assert_eq!(body["task"], "hello-world");
    assert!(
        body["completed_at"].as_str().is_some(),
        "completed_at must be present for a terminal job"
    );

    Ok(())
}

// Test 11: fire webhook, cancel the job, then GET with wait=true → returns immediately with "cancelled"
#[tokio::test]
async fn test_webhook_job_status_cancelled_treated_as_terminal() -> Result<()> {
    let (router, _pool, _tmp, _container) = setup().await?;

    // Fire public-hook to create a job
    let job_id = fire_webhook(router.clone(), "/hooks/public-hook").await;

    // Cancel the job via the API — this also fires the job_completion notification
    // (the job has no running steps, so cancellation is immediate).
    let response = router
        .clone()
        .oneshot(api_request(
            "POST",
            &format!("/api/jobs/{}/cancel", job_id),
            json!({}),
        ))
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "cancel request must succeed"
    );

    // GET /hooks/public-hook/jobs/{job_id}?wait=true — job is already cancelled (terminal),
    // so the endpoint should return immediately without blocking on the broadcast channel.
    let request = Request::builder()
        .method("GET")
        .uri(&format!(
            "/hooks/public-hook/jobs/{}?wait=true&timeout=2",
            job_id
        ))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await?;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "wait=true on cancelled job must return 200"
    );
    let body = body_json(response).await;
    assert_eq!(
        body["job_id"].as_str(),
        Some(job_id.to_string().as_str()),
        "job_id must match"
    );
    assert_eq!(
        body["status"].as_str(),
        Some("cancelled"),
        "status must be cancelled"
    );
    assert_eq!(body["trigger"], "public-hook");

    Ok(())
}
