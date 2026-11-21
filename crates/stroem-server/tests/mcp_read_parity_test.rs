//! MCP read tools that mirror the REST API: each tool answers with the same
//! content, ACL and errors as its REST handler (they share one function).
//!
//! The fixture: one workspace `default` with a secret, tasks in three
//! folders and an ACL for `viewer@parity.test` (Run on `public/*`, View on
//! `view/*`, nothing else). `admin@parity.test` is an admin.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{ApiKeyRepo, UserRepo};
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    AclAction, AclConfig, AclRule, AuthConfig, DbConfig, LogStorageConfig, McpConfig,
    RetentionConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

const VIEWER: &str = "viewer@parity.test";
const ADMIN: &str = "admin@parity.test";
const SECRET: &str = "pa55-parity-s3cret";
/// A secret whose value is also a step name: identifiers stay intact.
const STEP_SECRET: &str = "sync-step-7f3a";
const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

const YAML: &str = r#"
secrets:
  api_token: "pa55-parity-s3cret"
  step_token: "sync-step-7f3a"
actions:
  noop: { type: script, script: "true" }
tasks:
  report:
    folder: public
    input: { name: { type: string, default: x } }
    flow: { build: { action: noop } }
  restricted:
    folder: view
    flow: { build: { action: noop } }
  hidden:
    folder: hidden
    flow: { build: { action: noop } }
  files:
    folder: public
    input: { data_file: { type: file } }
    flow: { build: { action: noop } }
  view-files:
    folder: view
    input: { data_file: { type: file } }
    flow: { build: { action: noop } }
  hidden-files:
    folder: hidden
    input: { data_file: { type: file } }
    flow: { build: { action: noop } }
triggers:
  nightly-report: { type: scheduler, cron: "0 0 * * *", task: report }
  nightly-restricted: { type: scheduler, cron: "0 1 * * *", task: restricted }
  nightly-hidden: { type: scheduler, cron: "0 2 * * *", task: hidden }
"#;

struct App {
    router: Router,
    pool: PgPool,
    viewer: String,
    admin: String,
    input_blob: Arc<dyn BlobArchive>,
    _tmp: TempDir,
}

fn acl() -> AclConfig {
    AclConfig {
        default: AclAction::Deny,
        rules: vec![
            AclRule {
                workspace: "default".to_string(),
                tasks: vec!["public/*".to_string()],
                action: AclAction::Run,
                groups: vec![],
                users: vec![VIEWER.to_string()],
            },
            AclRule {
                workspace: "default".to_string(),
                tasks: vec!["view/*".to_string()],
                action: AclAction::View,
                groups: vec![],
                users: vec![VIEWER.to_string()],
            },
        ],
    }
}

async fn app() -> Result<App> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool.clone();
    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url: test_db.url },
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
                path: tmp.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "mcp-parity-test-token-long-enough-32c".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: "mcp-parity-jwt-secret-key-long-enough".to_string(),
            refresh_secret: "mcp-parity-refresh-secret-long-enough".to_string(),
            base_url: None,
            providers: HashMap::new(),
            initial_user: None,
            rate_limit: Default::default(),
        }),
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: Some(acl()),
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

    let workspace: WorkspaceConfig = serde_yaml::from_str(YAML)?;
    let mgr = WorkspaceManager::from_config("default", workspace);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let input_blob: Arc<dyn BlobArchive> =
        Arc::new(LocalBlobArchive::new(tmp.path().join("input-archive")));
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_input_blob(Some(input_blob.clone()));
    let router = build_router(state, CancellationToken::new());

    let viewer = api_key(&pool, VIEWER, false).await?;
    let admin = api_key(&pool, ADMIN, true).await?;
    Ok(App {
        router,
        pool,
        viewer,
        admin,
        input_blob,
        _tmp: tmp,
    })
}

async fn api_key(pool: &PgPool, email: &str, is_admin: bool) -> Result<String> {
    let user_id = Uuid::new_v4();
    UserRepo::create(pool, user_id, email, None, None).await?;
    UserRepo::set_admin(pool, user_id, is_admin).await?;
    let (raw, hash) = stroem_server::auth::generate_api_key();
    ApiKeyRepo::create(pool, &hash, user_id, "parity", &raw[..12], None).await?;
    Ok(raw)
}

// ─── Requests ────────────────────────────────────────────────────────────────

fn mcp_request(token: &str, sid: Option<&str>, body: &Value) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {token}"));
    if let Some(s) = sid {
        b = b.header("Mcp-Session-Id", s);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// Initialise a session and call one tool; the JSON-RPC response body.
async fn mcp(app: &App, token: &str, tool: &str, args: Value) -> Value {
    let init = json!({"jsonrpc": "2.0", "method": "initialize", "id": 0, "params": {
        "protocolVersion": "2025-03-26", "capabilities": {},
        "clientInfo": {"name": "t", "version": "1"}}});
    let resp = app
        .router
        .clone()
        .oneshot(mcp_request(token, None, &init))
        .await
        .unwrap();
    let sid = resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let call = json!({"jsonrpc": "2.0", "method": "tools/call", "id": 1,
        "params": {"name": tool, "arguments": args}});
    let resp = app
        .router
        .clone()
        .oneshot(mcp_request(token, sid.as_deref(), &call))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// The tool's JSON payload; panics on an error answer.
fn payload(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no tool text: {resp}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

/// The error message of an error answer (a JSON-RPC error).
fn error_message(resp: &Value) -> String {
    resp["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an error: {resp}"))
        .to_string()
}

fn is_invalid_params(resp: &Value) -> bool {
    resp["error"]["code"] == json!(-32602)
}

/// rmcp answers arguments that do not deserialize (a missing or mistyped
/// field) with a tool result flagged `isError`.
fn is_rejected_call(resp: &Value) -> bool {
    resp["result"]["isError"] == json!(true)
        && resp["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("failed to deserialize parameters"))
}

async fn rest(app: &App, token: &str, method: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, body)
}

// ─── Seeding ─────────────────────────────────────────────────────────────────

#[derive(Default)]
struct SeedJob<'a> {
    task: &'a str,
    status: &'a str,
    source_type: &'a str,
    input: Option<Value>,
    raw_input: Option<Value>,
    output: Option<Value>,
    source_job_id: Option<Uuid>,
    parent_job_id: Option<Uuid>,
    parent_step_name: Option<&'a str>,
    age_secs: f64,
}

async fn seed_job(pool: &PgPool, s: SeedJob<'_>) -> Uuid {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, input, raw_input, \
                          output, source_job_id, parent_job_id, parent_step_name, created_at, \
                          completed_at) \
         VALUES ($1, 'default', $2, $3, $4, $5, $6, $7, $8, $9, $10, \
                 NOW() - make_interval(secs => $11), NOW())",
    )
    .bind(job_id)
    .bind(if s.task.is_empty() { "report" } else { s.task })
    .bind(if s.status.is_empty() {
        "completed"
    } else {
        s.status
    })
    .bind(if s.source_type.is_empty() {
        "api"
    } else {
        s.source_type
    })
    .bind(s.input)
    .bind(s.raw_input)
    .bind(s.output)
    .bind(s.source_job_id)
    .bind(s.parent_job_id)
    .bind(s.parent_step_name)
    .bind(s.age_secs)
    .execute(pool)
    .await
    .expect("seed job");
    job_id
}

#[derive(Default)]
struct SeedStep<'a> {
    step: &'a str,
    action_type: &'a str,
    status: &'a str,
    input: Option<Value>,
    output: Option<Value>,
    error: Option<&'a str>,
    retry_history: Option<Value>,
    worker_id: Option<Uuid>,
}

async fn seed_step(pool: &PgPool, job_id: Uuid, s: SeedStep<'_>) {
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, input, \
                               output, error_message, retry_history, worker_id, started_at, \
                               completed_at) \
         VALUES ($1, $2, $2, $3, $4, $5, $6, $7, COALESCE($8, '[]'::jsonb), $9, NOW(), NOW())",
    )
    .bind(job_id)
    .bind(s.step)
    .bind(if s.action_type.is_empty() {
        "script"
    } else {
        s.action_type
    })
    .bind(if s.status.is_empty() {
        "completed"
    } else {
        s.status
    })
    .bind(s.input)
    .bind(s.output)
    .bind(s.error)
    .bind(s.retry_history)
    .bind(s.worker_id)
    .execute(pool)
    .await
    .expect("seed step");
}

/// A completed `report` job with content everywhere: job input/output, a
/// step with input/output/retry history, a `type: task` step with a child
/// job, and a Re-run of it.
async fn seed_rich_job(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let job_id = seed_job(
        pool,
        SeedJob {
            input: Some(json!({"name": "world"})),
            raw_input: Some(json!({"name": "world"})),
            output: Some(json!({"greeting": "hello world", "rows": 3})),
            age_secs: 60.0,
            ..Default::default()
        },
    )
    .await;
    seed_step(
        pool,
        job_id,
        SeedStep {
            step: "build",
            input: Some(json!({"name": "world"})),
            output: Some(json!({"greeting": "hello world"})),
            retry_history: Some(json!([{"attempt": 0, "error": "exit 1: flaky"}])),
            ..Default::default()
        },
    )
    .await;
    seed_step(
        pool,
        job_id,
        SeedStep {
            step: "call",
            action_type: "task",
            ..Default::default()
        },
    )
    .await;
    let child = seed_job(
        pool,
        SeedJob {
            task: "report",
            source_type: "task",
            parent_job_id: Some(job_id),
            parent_step_name: Some("call"),
            age_secs: 30.0,
            ..Default::default()
        },
    )
    .await;
    let rerun = seed_job(
        pool,
        SeedJob {
            source_type: "rerun",
            source_job_id: Some(job_id),
            age_secs: 10.0,
            ..Default::default()
        },
    )
    .await;
    (job_id, child, rerun)
}

// ─── get_job_status = REST job detail ───────────────────────────────────────

#[tokio::test]
async fn get_job_status_answers_the_rest_job_detail() -> Result<()> {
    let app = app().await?;
    let (job_id, child, rerun) = seed_rich_job(&app.pool).await;

    let (status, body) = rest(&app, &app.admin, "GET", &format!("/api/jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let resp = mcp(
        &app,
        &app.admin,
        "get_job_status",
        json!({"job_id": job_id.to_string()}),
    )
    .await;
    let detail = payload(&resp);
    assert_eq!(detail, body, "MCP must answer the REST job detail");

    assert_eq!(
        detail["output"],
        json!({"greeting": "hello world", "rows": 3})
    );
    assert_eq!(detail["input"], json!({"name": "world"}));
    assert_eq!(detail["raw_input"], json!({"name": "world"}));
    let steps = detail["steps"].as_array().unwrap();
    let build = steps.iter().find(|s| s["step_name"] == "build").unwrap();
    assert_eq!(build["output"], json!({"greeting": "hello world"}));
    assert_eq!(build["input"], json!({"name": "world"}));
    assert_eq!(build["retry_history"][0]["error"], json!("exit 1: flaky"));
    let call = steps.iter().find(|s| s["step_name"] == "call").unwrap();
    assert_eq!(call["child_jobs"][0]["id"], json!(child));
    assert_eq!(detail["derived_jobs"][0]["id"], json!(rerun));
    assert_eq!(detail["derived_jobs"][0]["source_type"], json!("rerun"));
    assert!(detail["input_files"].as_array().unwrap().is_empty());
    assert!(detail.get("truncated_values").is_none(), "{detail}");
    Ok(())
}

#[tokio::test]
async fn get_job_status_lists_input_files_and_links() -> Result<()> {
    let app = app().await?;
    let source = seed_job(&app.pool, SeedJob::default()).await;
    let job_id = seed_job(
        &app.pool,
        SeedJob {
            task: "files",
            source_type: "rerun",
            source_job_id: Some(source),
            ..Default::default()
        },
    )
    .await;
    let file_id = attach_file(&app, job_id, "files", "data_file", "report.csv", b"a,b\n").await?;

    let detail = payload(
        &mcp(
            &app,
            &app.viewer,
            "get_job_status",
            json!({"job_id": job_id.to_string()}),
        )
        .await,
    );
    assert_eq!(detail["source_job_id"], json!(source));
    assert_eq!(detail["source_type"], json!("rerun"));
    assert_eq!(
        detail["input_files"],
        json!([{"field": "data_file", "file_id": file_id, "name": "report.csv",
                "size_bytes": 4, "content_type": "text/csv"}])
    );
    Ok(())
}

#[tokio::test]
async fn get_job_status_masks_secrets_and_keeps_identifiers() -> Result<()> {
    let app = app().await?;
    let job_id = seed_job(
        &app.pool,
        SeedJob {
            output: Some(json!({"token": SECRET})),
            ..Default::default()
        },
    )
    .await;
    seed_step(
        &app.pool,
        job_id,
        SeedStep {
            step: STEP_SECRET,
            output: Some(json!({"leak": format!("token={SECRET}"), "step": STEP_SECRET})),
            error: Some("exit 1: pa55-parity-s3cret rejected"),
            ..Default::default()
        },
    )
    .await;

    let resp = mcp(
        &app,
        &app.viewer,
        "get_job_status",
        json!({"job_id": job_id.to_string()}),
    )
    .await;
    assert!(!resp.to_string().contains(SECRET), "secret leaked: {resp}");
    let detail = payload(&resp);
    assert_eq!(detail["output"]["token"], json!(MASK));
    let step = &detail["steps"][0];
    // An identifier equal to a secret value stays intact; content is masked.
    assert_eq!(step["step_name"], json!(STEP_SECRET));
    assert_eq!(step["action_name"], json!(STEP_SECRET));
    assert_eq!(step["output"]["leak"], json!(format!("token={MASK}")));
    assert_eq!(step["output"]["step"], json!(MASK));
    assert_eq!(
        step["error_message"],
        json!(format!("exit 1: {MASK} rejected"))
    );
    assert_eq!(detail["job_id"], json!(job_id));
    Ok(())
}

#[tokio::test]
async fn get_job_status_acl_view_is_enough_and_deny_is_not_found() -> Result<()> {
    let app = app().await?;
    let viewable = seed_job(
        &app.pool,
        SeedJob {
            task: "restricted",
            output: Some(json!({"ok": true})),
            ..Default::default()
        },
    )
    .await;
    let hidden = seed_job(
        &app.pool,
        SeedJob {
            task: "hidden",
            ..Default::default()
        },
    )
    .await;

    let detail = payload(
        &mcp(
            &app,
            &app.viewer,
            "get_job_status",
            json!({"job_id": viewable.to_string()}),
        )
        .await,
    );
    assert_eq!(detail["output"], json!({"ok": true}));

    let resp = mcp(
        &app,
        &app.viewer,
        "get_job_status",
        json!({"job_id": hidden.to_string()}),
    )
    .await;
    assert_eq!(error_message(&resp), "Job not found");
    // The admin sees it.
    let detail = payload(
        &mcp(
            &app,
            &app.admin,
            "get_job_status",
            json!({"job_id": hidden.to_string()}),
        )
        .await,
    );
    assert_eq!(detail["task_name"], json!("hidden"));
    Ok(())
}

#[tokio::test]
async fn get_job_status_bad_parameters() -> Result<()> {
    let app = app().await?;
    let resp = mcp(
        &app,
        &app.admin,
        "get_job_status",
        json!({"job_id": "not-a-uuid"}),
    )
    .await;
    assert!(is_invalid_params(&resp), "{resp}");
    let resp = mcp(
        &app,
        &app.admin,
        "get_job_status",
        json!({"job_id": Uuid::new_v4().to_string()}),
    )
    .await;
    assert_eq!(error_message(&resp), "Job not found");
    Ok(())
}

#[tokio::test]
async fn get_job_status_hides_a_later_run_the_caller_may_not_see() -> Result<()> {
    let app = app().await?;
    let source = seed_job(&app.pool, SeedJob::default()).await;
    let visible = seed_job(
        &app.pool,
        SeedJob {
            source_type: "rerun",
            source_job_id: Some(source),
            age_secs: 2.0,
            ..Default::default()
        },
    )
    .await;
    // Authorised by its own row: a run of a task in a denied folder.
    let denied = seed_job(
        &app.pool,
        SeedJob {
            task: "hidden",
            source_type: "restart",
            source_job_id: Some(source),
            age_secs: 1.0,
            ..Default::default()
        },
    )
    .await;

    let ids = |detail: &Value| -> Vec<Value> {
        detail["derived_jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].clone())
            .collect()
    };
    let args = json!({"job_id": source.to_string()});
    let viewer = payload(&mcp(&app, &app.viewer, "get_job_status", args.clone()).await);
    assert_eq!(ids(&viewer), vec![json!(visible)], "{viewer}");
    let admin = payload(&mcp(&app, &app.admin, "get_job_status", args).await);
    assert_eq!(ids(&admin), vec![json!(denied), json!(visible)], "{admin}");
    Ok(())
}

#[tokio::test]
async fn get_job_status_replaces_oversized_values_with_a_note() -> Result<()> {
    let app = app().await?;
    let big = "x".repeat(300 * 1024);
    let job_id = seed_job(
        &app.pool,
        SeedJob {
            output: Some(json!({"small": "kept"})),
            ..Default::default()
        },
    )
    .await;
    seed_step(
        &app.pool,
        job_id,
        SeedStep {
            step: "build",
            input: Some(json!({"name": "world"})),
            output: Some(json!({"blob": big})),
            ..Default::default()
        },
    )
    .await;

    let resp = mcp(
        &app,
        &app.admin,
        "get_job_status",
        json!({"job_id": job_id.to_string()}),
    )
    .await;
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.len() <= 256 * 1024, "answer is {} bytes", text.len());
    assert!(!text.contains("\n  "), "the answer is compact JSON");
    let detail = payload(&resp);
    let step = &detail["steps"][0];
    let note = step["output"]
        .as_str()
        .expect("a note in place of the value");
    assert!(note.contains("GET /api/jobs/"), "{note}");
    assert_eq!(step["input"], json!({"name": "world"}));
    assert_eq!(detail["output"], json!({"small": "kept"}));
    let truncated = detail["truncated_values"].as_array().unwrap();
    assert_eq!(truncated.len(), 1, "{detail}");
    assert_eq!(truncated[0]["path"], json!("/steps/0/output"));
    assert!(truncated[0]["size_bytes"].as_u64().unwrap() > 300 * 1024);

    // REST is unchanged: the whole value.
    let (_, body) = rest(&app, &app.admin, "GET", &format!("/api/jobs/{job_id}")).await;
    assert_eq!(
        body["steps"][0]["output"]["blob"].as_str().unwrap().len(),
        big.len()
    );
    Ok(())
}

/// Insert an input file (row + blob) and attach it to `job_id`'s `field`.
async fn attach_file(
    app: &App,
    job_id: Uuid,
    task: &str,
    field: &str,
    name: &str,
    content: &[u8],
) -> Result<String> {
    attach_file_with(
        app,
        job_id,
        FileSeed {
            task,
            field,
            name,
            content_type: "text/csv",
            content: Some(content),
            row_size: content.len() as i64,
        },
    )
    .await
}

struct FileSeed<'a> {
    task: &'a str,
    field: &'a str,
    name: &'a str,
    content_type: &'a str,
    /// `None`: no blob is stored.
    content: Option<&'a [u8]>,
    /// The size the row records.
    row_size: i64,
}

async fn attach_file_with(app: &App, job_id: Uuid, f: FileSeed<'_>) -> Result<String> {
    let FileSeed {
        task,
        field,
        name,
        content_type,
        content,
        row_size,
    } = f;
    let file_id = format!("file_{}", Uuid::new_v4().simple());
    let key = format!("inputs/default/{file_id}");
    if let Some(content) = content {
        app.input_blob
            .put(&key, content_type, bytes::Bytes::copy_from_slice(content))
            .await?;
    }
    stroem_db::InputFileRepo::insert(
        &app.pool,
        &stroem_db::NewInputFile {
            file_id: &file_id,
            workspace: "default",
            task_name: task,
            uploaded_by_user_id: None,
            origin: "api",
            original_name: name,
            safe_name: name,
            content_type,
            size_bytes: row_size,
            sha256: "00",
            storage_key: &key,
        },
    )
    .await?;
    sqlx::query("INSERT INTO job_input_file (job_id, field, file_id) VALUES ($1, $2, $3)")
        .bind(job_id)
        .bind(field)
        .bind(&file_id)
        .execute(&app.pool)
        .await?;
    Ok(file_id)
}

/// A pinned job whose commit can never load: the answer masks every content
/// string (the REST rule), identifiers intact. The transient case (an error,
/// `redaction set unavailable, retry`) is in `git_refs_read_paths_test.rs`.
#[tokio::test]
async fn get_job_status_of_a_job_with_an_unloadable_pin_masks_all_content() -> Result<()> {
    use crate::common::pinned::{mcp_call, mcp_is_error, mcp_tool_json, pinned_workspace_fixture};
    let fx = pinned_workspace_fixture(crate::common::pinned::PinnedFixtureOpts {
        mcp: true,
        ..Default::default()
    })
    .await?;
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, revision, git_ref, \
                          output, created_at, completed_at) \
         VALUES ($1, 'etl', 'nightly', 'failed', 'api', $2, 'release/2.3', $3, NOW(), NOW())",
    )
    .bind(job_id)
    .bind("00000000000000000000000000000000000000ff")
    .bind(json!({"result": "plain text"}))
    .execute(&fx.pool)
    .await?;

    let resp = mcp_call(
        &fx.router,
        None,
        "get_job_status",
        json!({"job_id": job_id.to_string()}),
    )
    .await;
    assert!(!mcp_is_error(&resp), "{resp}");
    let detail = mcp_tool_json(&resp);
    assert_eq!(detail["output"]["result"], json!(MASK), "{detail}");
    assert_eq!(detail["job_id"], json!(job_id));
    assert_eq!(detail["ref"], json!("release/2.3"));
    Ok(())
}

// ─── list_triggers / list_attention_triggers ─────────────────────────────────

const DAY: f64 = 86_400.0;

/// A job fired by trigger `default/{trigger}`; `pin` = `(git_ref, folder)`.
async fn seed_trigger_run(
    pool: &PgPool,
    trigger: &str,
    task: &str,
    status: &str,
    ago_secs: f64,
    pin: Option<(&str, &str)>,
) -> Uuid {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type, source_id, \
                          git_ref, task_folder, created_at) \
         VALUES ($1, 'default', $2, 'distributed', $3, 'trigger', $4, $5, $6, \
                 NOW() - make_interval(secs => $7))",
    )
    .bind(job_id)
    .bind(task)
    .bind(status)
    .bind(format!("default/{trigger}"))
    .bind(pin.map(|p| p.0))
    .bind(pin.map(|p| p.1))
    .bind(ago_secs)
    .execute(pool)
    .await
    .expect("seed trigger run");
    job_id
}

fn names(list: &Value) -> Vec<String> {
    let mut v: Vec<String> = list
        .as_array()
        .unwrap_or_else(|| panic!("not a list: {list}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn by_name<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no trigger {name}: {list}"))
}

#[tokio::test]
async fn list_triggers_answers_the_rest_trigger_list() -> Result<()> {
    let app = app().await?;
    let run = seed_trigger_run(
        &app.pool,
        "nightly-report",
        "report",
        "failed",
        3.0 * DAY,
        None,
    )
    .await;

    let (status, body) = rest(&app, &app.admin, "GET", "/api/workspaces/default/triggers").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list = payload(
        &mcp(
            &app,
            &app.admin,
            "list_triggers",
            json!({"workspace": "default"}),
        )
        .await,
    );
    assert_eq!(list, body);
    assert_eq!(
        names(&list),
        ["nightly-hidden", "nightly-report", "nightly-restricted"]
    );
    let t = by_name(&list, "nightly-report");
    assert_eq!(t["last_run"]["job_id"], json!(run.to_string()));
    assert_eq!(t["last_run"]["status"], json!("failed"));
    assert!(t["missed"]["count"].as_u64().unwrap() >= 1, "{t}");
    Ok(())
}

#[tokio::test]
async fn list_triggers_acl_hides_denied_triggers_and_runs_but_not_the_missed_mark() -> Result<()> {
    let app = app().await?;
    // The last run is a pinned job whose commit put the task in `hidden`.
    seed_trigger_run(
        &app.pool,
        "nightly-report",
        "report",
        "completed",
        3.0 * DAY,
        Some(("v2", "hidden")),
    )
    .await;

    let list = payload(
        &mcp(
            &app,
            &app.viewer,
            "list_triggers",
            json!({"workspace": "default"}),
        )
        .await,
    );
    // View on `view/*` is enough to see a trigger; `hidden/*` is denied.
    assert_eq!(names(&list), ["nightly-report", "nightly-restricted"]);
    let t = by_name(&list, "nightly-report");
    assert!(t["last_run"].is_null(), "{t}");
    assert_eq!(t["last_run_hidden"], json!(true));
    assert!(t["missed"]["count"].as_u64().unwrap() >= 1, "{t}");

    let list = payload(
        &mcp(
            &app,
            &app.admin,
            "list_triggers",
            json!({"workspace": "default"}),
        )
        .await,
    );
    assert_eq!(
        by_name(&list, "nightly-report")["last_run_hidden"],
        json!(false)
    );
    Ok(())
}

#[tokio::test]
async fn list_triggers_unknown_workspace_is_the_rest_error() -> Result<()> {
    let app = app().await?;
    let resp = mcp(
        &app,
        &app.admin,
        "list_triggers",
        json!({"workspace": "nope"}),
    )
    .await;
    assert_eq!(error_message(&resp), "Workspace 'nope' not found");
    let resp = mcp(&app, &app.admin, "list_triggers", json!({})).await;
    assert!(
        is_rejected_call(&resp),
        "a missing workspace is a bad call: {resp}"
    );
    Ok(())
}

#[tokio::test]
async fn list_attention_triggers_answers_the_rest_dashboard_card() -> Result<()> {
    let app = app().await?;
    seed_trigger_run(
        &app.pool,
        "nightly-report",
        "report",
        "completed",
        3.0 * DAY,
        None,
    )
    .await;
    seed_trigger_run(
        &app.pool,
        "nightly-restricted",
        "restricted",
        "failed",
        60.0,
        None,
    )
    .await;
    seed_trigger_run(&app.pool, "nightly-hidden", "hidden", "failed", 60.0, None).await;

    let (status, body) = rest(&app, &app.admin, "GET", "/api/triggers/attention").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let admin = payload(&mcp(&app, &app.admin, "list_attention_triggers", json!({})).await);
    assert_eq!(admin, body);
    assert_eq!(
        names(&admin),
        ["nightly-hidden", "nightly-report", "nightly-restricted"]
    );
    assert_eq!(
        by_name(&admin, "nightly-restricted")["last_run_failed"],
        json!(true)
    );
    assert_eq!(
        by_name(&admin, "nightly-report")["workspace"],
        json!("default")
    );

    let viewer = payload(&mcp(&app, &app.viewer, "list_attention_triggers", json!({})).await);
    assert_eq!(names(&viewer), ["nightly-report", "nightly-restricted"]);
    Ok(())
}

// ─── list_jobs: search, offset, total ────────────────────────────────────────

/// Five jobs, oldest first: report ×2, restricted ×2, hidden ×1 (newest).
async fn seed_job_list(pool: &PgPool) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for (i, task) in ["report", "report", "restricted", "restricted", "hidden"]
        .into_iter()
        .enumerate()
    {
        ids.push(
            seed_job(
                pool,
                SeedJob {
                    task,
                    age_secs: 100.0 - i as f64,
                    ..Default::default()
                },
            )
            .await,
        );
    }
    ids
}

fn job_ids(list: &Value) -> Vec<Value> {
    list["jobs"]
        .as_array()
        .unwrap_or_else(|| panic!("no jobs: {list}"))
        .iter()
        .map(|j| j["job_id"].clone())
        .collect()
}

#[tokio::test]
async fn list_jobs_answers_the_rest_page_with_offset_and_total() -> Result<()> {
    let app = app().await?;
    let ids = seed_job_list(&app.pool).await;

    let (status, body) = rest(&app, &app.admin, "GET", "/api/jobs?limit=2&offset=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list = payload(
        &mcp(
            &app,
            &app.admin,
            "list_jobs",
            json!({"limit": 2, "offset": 1}),
        )
        .await,
    );
    assert_eq!(list["jobs"], body["items"]);
    assert_eq!(list["total"], body["total"]);
    assert_eq!(list["total"], json!(5));
    assert_eq!(list["count"], json!(2));
    assert_eq!(job_ids(&list), vec![json!(ids[3]), json!(ids[2])]);
    Ok(())
}

#[tokio::test]
async fn list_jobs_search_matches_like_rest() -> Result<()> {
    let app = app().await?;
    let ids = seed_job_list(&app.pool).await;

    let (_, body) = rest(&app, &app.admin, "GET", "/api/jobs?search=restrict").await;
    let list = payload(&mcp(&app, &app.admin, "list_jobs", json!({"search": "restrict"})).await);
    assert_eq!(list["jobs"], body["items"]);
    assert_eq!(list["total"], json!(2));
    assert_eq!(job_ids(&list), vec![json!(ids[3]), json!(ids[2])]);

    // With a workspace filter and a status filter too.
    let list = payload(
        &mcp(
            &app,
            &app.admin,
            "list_jobs",
            json!({"workspace": "default", "status": "completed", "search": " restrict "}),
        )
        .await,
    );
    assert_eq!(
        list["total"],
        json!(2),
        "the search term is trimmed: {list}"
    );
    Ok(())
}

#[tokio::test]
async fn list_jobs_applies_the_acl_before_limit_and_in_total() -> Result<()> {
    let app = app().await?;
    let ids = seed_job_list(&app.pool).await;
    // The newest job is denied: a page of one is the newest VISIBLE job.
    let list = payload(&mcp(&app, &app.viewer, "list_jobs", json!({"limit": 1})).await);
    assert_eq!(job_ids(&list), vec![json!(ids[3])]);
    assert_eq!(list["total"], json!(4));
    let list = payload(&mcp(&app, &app.viewer, "list_jobs", json!({"search": "hidden"})).await);
    assert_eq!(list["total"], json!(0));
    assert_eq!(list["count"], json!(0));
    Ok(())
}

#[tokio::test]
async fn list_jobs_bad_parameters_are_the_rest_errors() -> Result<()> {
    let app = app().await?;
    let resp = mcp(
        &app,
        &app.admin,
        "list_jobs",
        json!({"workspace": "default", "task_name": "report", "search": "x"}),
    )
    .await;
    assert_eq!(
        error_message(&resp),
        "Cannot use both task_name and search filters"
    );
    let resp = mcp(
        &app,
        &app.admin,
        "list_jobs",
        json!({"search": "s".repeat(201)}),
    )
    .await;
    assert_eq!(
        error_message(&resp),
        "Search term must be 200 characters or fewer"
    );
    let resp = mcp(&app, &app.admin, "list_jobs", json!({"offset": -1})).await;
    assert!(is_invalid_params(&resp), "{resp}");
    let resp = mcp(&app, &app.admin, "list_jobs", json!({"status": "nope"})).await;
    assert!(is_invalid_params(&resp), "{resp}");
    assert!(
        error_message(&resp).starts_with("Invalid status filter 'nope'"),
        "{resp}"
    );
    Ok(())
}

// ─── refresh_workspace ───────────────────────────────────────────────────────

#[tokio::test]
async fn refresh_workspace_reloads_and_shares_the_rest_cooldown() -> Result<()> {
    let app = app().await?;
    let resp = mcp(
        &app,
        &app.admin,
        "refresh_workspace",
        json!({"workspace": "default"}),
    )
    .await;
    let body = payload(&resp);
    assert_eq!(body["workspace"], json!("default"));
    assert_eq!(body["refreshed"], json!(true));
    assert!(body.get("revision").is_some(), "{body}");

    // The REST route sees the same cooldown, and so does a second MCP call.
    let (status, _) = rest(&app, &app.admin, "POST", "/api/workspaces/default/refresh").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let resp = mcp(
        &app,
        &app.admin,
        "refresh_workspace",
        json!({"workspace": "default"}),
    )
    .await;
    let msg = error_message(&resp);
    assert!(
        msg.starts_with("Workspace refresh rate-limited; retry after ") && msg.ends_with('s'),
        "{msg}"
    );
    Ok(())
}

#[tokio::test]
async fn refresh_workspace_needs_a_visible_task() -> Result<()> {
    let app = app().await?;
    // View on one task is enough.
    let resp = mcp(
        &app,
        &app.viewer,
        "refresh_workspace",
        json!({"workspace": "default"}),
    )
    .await;
    assert_eq!(payload(&resp)["refreshed"], json!(true));

    // No visible task: the same answer as an unknown workspace.
    let nobody = api_key(&app.pool, "nobody@parity.test", false).await?;
    let resp = mcp(
        &app,
        &nobody,
        "refresh_workspace",
        json!({"workspace": "default"}),
    )
    .await;
    assert_eq!(error_message(&resp), "Workspace 'default' not found");
    let resp = mcp(
        &app,
        &app.admin,
        "refresh_workspace",
        json!({"workspace": "nope"}),
    )
    .await;
    assert_eq!(error_message(&resp), "Workspace 'nope' not found");
    let resp = mcp(&app, &app.admin, "refresh_workspace", json!({})).await;
    assert!(is_rejected_call(&resp), "{resp}");
    Ok(())
}

// ─── get_task_stats / get_stats ──────────────────────────────────────────────

/// A completed run of `task` that took `secs`, with one step `build`.
async fn seed_timed_run(pool: &PgPool, task: &str, secs: f64) -> Uuid {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, created_at, \
                          started_at, completed_at) \
         VALUES ($1, 'default', $2, 'completed', 'api', NOW() - make_interval(secs => $3 + 1), \
                 NOW() - make_interval(secs => $3), NOW())",
    )
    .bind(job_id)
    .bind(task)
    .bind(secs)
    .execute(pool)
    .await
    .expect("seed timed run");
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, started_at, \
                               completed_at) \
         VALUES ($1, 'build', 'noop', 'script', 'completed', NOW() - make_interval(secs => $2), \
                 NOW())",
    )
    .bind(job_id)
    .bind(secs)
    .execute(pool)
    .await
    .expect("seed timed step");
    job_id
}

#[tokio::test]
async fn get_task_stats_answers_the_rest_stats() -> Result<()> {
    let app = app().await?;
    for secs in [10.0, 20.0, 30.0] {
        seed_timed_run(&app.pool, "report", secs).await;
    }
    let (status, body) = rest(
        &app,
        &app.admin,
        "GET",
        "/api/workspaces/default/tasks/report/stats?limit=2",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stats = payload(
        &mcp(
            &app,
            &app.admin,
            "get_task_stats",
            json!({"workspace": "default", "task_name": "report", "limit": 2}),
        )
        .await,
    );
    assert_eq!(stats, body);
    assert_eq!(stats["window"], json!(2));
    assert_eq!(stats["task"]["sample_size"], json!(2));
    assert_eq!(stats["steps"][0]["step_name"], json!("build"));

    let stats = payload(
        &mcp(
            &app,
            &app.admin,
            "get_task_stats",
            json!({"workspace": "default", "task_name": "report"}),
        )
        .await,
    );
    assert_eq!(stats["window"], json!(50), "the REST default window");
    assert_eq!(stats["task"]["sample_size"], json!(3));
    Ok(())
}

#[tokio::test]
async fn get_task_stats_view_is_enough_and_deny_is_not_found() -> Result<()> {
    let app = app().await?;
    seed_timed_run(&app.pool, "restricted", 5.0).await;
    let call = |task: &'static str| json!({"workspace": "default", "task_name": task});
    let stats = payload(&mcp(&app, &app.viewer, "get_task_stats", call("restricted")).await);
    assert_eq!(stats["task"]["sample_size"], json!(1));
    let resp = mcp(&app, &app.viewer, "get_task_stats", call("hidden")).await;
    assert_eq!(error_message(&resp), "Task not found");
    let resp = mcp(&app, &app.admin, "get_task_stats", call("nope")).await;
    assert_eq!(error_message(&resp), "Task not found");
    let resp = mcp(
        &app,
        &app.admin,
        "get_task_stats",
        json!({"workspace": "nope", "task_name": "report"}),
    )
    .await;
    assert_eq!(error_message(&resp), "Workspace 'nope' not found");
    Ok(())
}

#[tokio::test]
async fn get_stats_answers_the_rest_dashboard_counts_within_the_acl() -> Result<()> {
    let app = app().await?;
    for (task, status) in [
        ("report", "completed"),
        ("report", "failed"),
        ("restricted", "running"),
        ("hidden", "failed"),
        ("hidden", "pending"),
    ] {
        seed_job(
            &app.pool,
            SeedJob {
                task,
                status,
                ..Default::default()
            },
        )
        .await;
    }
    let (status, body) = rest(&app, &app.admin, "GET", "/api/stats").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let admin = payload(&mcp(&app, &app.admin, "get_stats", json!({})).await);
    assert_eq!(admin, body);
    assert_eq!(
        admin,
        json!({"pending": 1, "running": 1, "completed": 1, "failed": 2,
               "cancelled": 0, "skipped": 0})
    );
    let (_, body) = rest(&app, &app.viewer, "GET", "/api/stats").await;
    let viewer = payload(&mcp(&app, &app.viewer, "get_stats", json!({})).await);
    assert_eq!(viewer, body);
    assert_eq!(
        viewer,
        json!({"pending": 0, "running": 1, "completed": 1, "failed": 1,
               "cancelled": 0, "skipped": 0})
    );
    Ok(())
}

// ─── list_workers / get_worker ───────────────────────────────────────────────

async fn register_worker(pool: &PgPool, name: &str) -> Uuid {
    let worker_id = Uuid::new_v4();
    stroem_db::WorkerRepo::register(
        pool,
        worker_id,
        name,
        &["script".to_string()],
        &["gpu".to_string()],
        false,
        Some("0.18.5"),
    )
    .await
    .expect("register worker");
    worker_id
}

#[tokio::test]
async fn list_workers_answers_the_rest_page() -> Result<()> {
    let app = app().await?;
    for name in ["w-a", "w-b", "w-c"] {
        register_worker(&app.pool, name).await;
    }
    let (status, body) = rest(&app, &app.viewer, "GET", "/api/workers?limit=2&offset=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page = payload(
        &mcp(
            &app,
            &app.viewer,
            "list_workers",
            json!({"limit": 2, "offset": 1}),
        )
        .await,
    );
    assert_eq!(page, body);
    assert_eq!(page["total"], json!(3));
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["items"][0]["tags"], json!(["gpu"]));

    let resp = mcp(&app, &app.viewer, "list_workers", json!({"offset": -1})).await;
    assert!(is_invalid_params(&resp), "{resp}");
    Ok(())
}

#[tokio::test]
async fn get_worker_filters_steps_by_acl_and_masks_errors() -> Result<()> {
    let app = app().await?;
    let worker = register_worker(&app.pool, "w-detail").await;
    for (task, step) in [
        ("report", "pub"),
        ("restricted", "view"),
        ("hidden", "secret"),
    ] {
        let job = seed_job(
            &app.pool,
            SeedJob {
                task,
                status: "failed",
                ..Default::default()
            },
        )
        .await;
        seed_step(
            &app.pool,
            job,
            SeedStep {
                step,
                status: "failed",
                error: Some("exit 1: pa55-parity-s3cret"),
                worker_id: Some(worker),
                ..Default::default()
            },
        )
        .await;
    }
    let uri = format!("/api/workers/{worker}");
    let args = json!({"worker_id": worker.to_string()});

    let (_, body) = rest(&app, &app.viewer, "GET", &uri).await;
    let viewer = mcp(&app, &app.viewer, "get_worker", args.clone()).await;
    assert!(
        !viewer.to_string().contains(SECRET),
        "secret leaked: {viewer}"
    );
    let viewer = payload(&viewer);
    assert_eq!(viewer, body);
    let steps: Vec<&str> = viewer["steps"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["step_name"].as_str().unwrap())
        .collect();
    assert_eq!(steps.len(), 2, "{viewer}");
    assert!(!steps.contains(&"secret"), "{viewer}");
    assert_eq!(viewer["steps"]["total"], json!(2));
    assert_eq!(
        viewer["steps"]["items"][0]["error_message"],
        json!(format!("exit 1: {MASK}"))
    );

    let (_, body) = rest(&app, &app.admin, "GET", &uri).await;
    let admin = payload(&mcp(&app, &app.admin, "get_worker", args).await);
    assert_eq!(admin, body);
    assert_eq!(admin["steps"]["total"], json!(3));
    assert_eq!(admin["name"], json!("w-detail"));
    Ok(())
}

#[tokio::test]
async fn get_worker_bad_parameters() -> Result<()> {
    let app = app().await?;
    let resp = mcp(&app, &app.admin, "get_worker", json!({"worker_id": "nope"})).await;
    assert!(is_invalid_params(&resp), "{resp}");
    let resp = mcp(
        &app,
        &app.admin,
        "get_worker",
        json!({"worker_id": Uuid::new_v4().to_string()}),
    )
    .await;
    assert_eq!(error_message(&resp), "Worker not found");
    Ok(())
}

// ─── get_input_file ──────────────────────────────────────────────────────────

/// A `files`-family job with one attached file; returns `(job_id, file_id)`.
async fn job_with_file(app: &App, task: &str, f: FileSeed<'_>) -> Result<Uuid> {
    let job_id = seed_job(
        &app.pool,
        SeedJob {
            task,
            ..Default::default()
        },
    )
    .await;
    attach_file_with(app, job_id, FileSeed { task, ..f }).await?;
    Ok(job_id)
}

fn csv(content: &[u8]) -> FileSeed<'_> {
    FileSeed {
        task: "",
        field: "data_file",
        name: "report.csv",
        content_type: "text/csv",
        content: Some(content),
        row_size: content.len() as i64,
    }
}

#[tokio::test]
async fn get_input_file_returns_a_text_file_inline() -> Result<()> {
    let app = app().await?;
    let job_id = job_with_file(&app, "files", csv(b"id,total\n1,10\n")).await?;
    let resp = mcp(
        &app,
        &app.viewer,
        "get_input_file",
        json!({"job_id": job_id.to_string(), "field": "data_file"}),
    )
    .await;
    assert_eq!(
        resp["result"]["content"][0]["text"],
        json!("id,total\n1,10\n"),
        "{resp}"
    );
    // The REST download has the same bytes.
    let (status, body) = rest(
        &app,
        &app.viewer,
        "GET",
        &format!("/api/jobs/{job_id}/input-files/data_file"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!("id,total\n1,10\n"));
    Ok(())
}

#[tokio::test]
async fn get_input_file_returns_an_uploaded_binary_as_a_blob() -> Result<()> {
    use base64::Engine;
    let app = app().await?;
    // An MCP inline upload is stored as application/octet-stream.
    let job_id = job_with_file(
        &app,
        "files",
        FileSeed {
            content_type: "application/octet-stream",
            ..csv(b"\x00\x01binary")
        },
    )
    .await?;
    let resp = mcp(
        &app,
        &app.viewer,
        "get_input_file",
        json!({"job_id": job_id.to_string(), "field": "data_file"}),
    )
    .await;
    let res = &resp["result"]["content"][0]["resource"];
    assert_eq!(res["mimeType"], json!("application/octet-stream"), "{resp}");
    assert_eq!(
        res["uri"],
        json!(format!("stroem://input-files/{job_id}/data_file"))
    );
    let blob = base64::engine::general_purpose::STANDARD.decode(res["blob"].as_str().unwrap())?;
    assert_eq!(blob, b"\x00\x01binary");
    Ok(())
}

#[tokio::test]
async fn get_input_file_acl_view_is_enough_and_deny_is_not_found() -> Result<()> {
    let app = app().await?;
    let viewable = job_with_file(&app, "view-files", csv(b"a")).await?;
    let hidden = job_with_file(&app, "hidden-files", csv(b"b")).await?;
    let args = |id: Uuid| json!({"job_id": id.to_string(), "field": "data_file"});
    let resp = mcp(&app, &app.viewer, "get_input_file", args(viewable)).await;
    assert_eq!(resp["result"]["content"][0]["text"], json!("a"), "{resp}");
    let resp = mcp(&app, &app.viewer, "get_input_file", args(hidden)).await;
    assert_eq!(error_message(&resp), "Job not found");
    let resp = mcp(&app, &app.admin, "get_input_file", args(hidden)).await;
    assert_eq!(resp["result"]["content"][0]["text"], json!("b"), "{resp}");
    Ok(())
}

#[tokio::test]
async fn get_input_file_refuses_a_large_file_without_reading_it() -> Result<()> {
    let app = app().await?;
    // The row says 2 MiB and there is NO blob: a read would fail with an
    // internal error, so the refusal proves the size is checked first.
    let job_id = job_with_file(
        &app,
        "files",
        FileSeed {
            content: None,
            row_size: 2 * 1024 * 1024,
            ..csv(b"")
        },
    )
    .await?;
    let resp = mcp(
        &app,
        &app.viewer,
        "get_input_file",
        json!({"job_id": job_id.to_string(), "field": "data_file"}),
    )
    .await;
    assert!(is_invalid_params(&resp), "{resp}");
    let msg = error_message(&resp);
    assert!(
        msg.contains(&format!("/api/jobs/{job_id}/input-files/data_file")),
        "{msg}"
    );
    assert!(msg.contains("2097152 bytes"), "{msg}");
    assert!(
        !msg.contains("report.csv"),
        "the file name is content: {msg}"
    );

    // A row that under-states the size: the stream bound still refuses.
    let big = vec![b'x'; 2 * 1024 * 1024];
    let job_id = job_with_file(
        &app,
        "files",
        FileSeed {
            row_size: 10,
            ..csv(&big)
        },
    )
    .await?;
    let resp = mcp(
        &app,
        &app.viewer,
        "get_input_file",
        json!({"job_id": job_id.to_string(), "field": "data_file"}),
    )
    .await;
    assert!(is_invalid_params(&resp), "{resp}");
    Ok(())
}

#[tokio::test]
async fn get_input_file_bad_parameters() -> Result<()> {
    let app = app().await?;
    let job_id = job_with_file(&app, "files", csv(b"a")).await?;
    let resp = mcp(
        &app,
        &app.admin,
        "get_input_file",
        json!({"job_id": job_id.to_string(), "field": "other"}),
    )
    .await;
    assert_eq!(error_message(&resp), "Input file not found");
    let resp = mcp(
        &app,
        &app.admin,
        "get_input_file",
        json!({"job_id": Uuid::new_v4().to_string(), "field": "data_file"}),
    )
    .await;
    assert_eq!(error_message(&resp), "Job not found");
    let resp = mcp(
        &app,
        &app.admin,
        "get_input_file",
        json!({"job_id": "x", "field": "data_file"}),
    )
    .await;
    assert!(is_invalid_params(&resp), "{resp}");
    Ok(())
}

#[tokio::test]
async fn get_job_logs_description_states_the_tail_limit() -> Result<()> {
    let app = app().await?;
    let list = json!({"jsonrpc": "2.0", "method": "tools/list", "id": 1});
    let init = json!({"jsonrpc": "2.0", "method": "initialize", "id": 0, "params": {
        "protocolVersion": "2025-03-26", "capabilities": {},
        "clientInfo": {"name": "t", "version": "1"}}});
    let resp = app
        .router
        .clone()
        .oneshot(mcp_request(&app.admin, None, &init))
        .await
        .unwrap();
    let sid = resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let resp = app
        .router
        .clone()
        .oneshot(mcp_request(&app.admin, sid.as_deref(), &list))
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes)?;
    let tools = body["result"]["tools"].as_array().unwrap();
    let describe = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .and_then(|t| t["description"].as_str())
            .unwrap_or_else(|| panic!("no tool {name}"))
            .to_string()
    };
    let logs = describe("get_job_logs");
    assert!(logs.contains("4 MiB"), "{logs}");
    assert!(logs.contains("--full"), "{logs}");
    assert!(describe("get_job_status").contains("256 KiB"));
    assert!(describe("get_input_file").contains("1 MiB"));
    Ok(())
}

// ─── Fail closed when the ACL context cannot load ────────────────────────────

/// The viewer's groups cannot be read (the table is gone from this test's
/// database): every tool that filters by ACL refuses with the fixed internal
/// error, never an unfiltered answer; worker detail drops every step, as REST.
#[tokio::test]
async fn every_acl_filtered_tool_fails_closed_without_the_acl_context() -> Result<()> {
    let app = app().await?;
    let job_id = seed_job(&app.pool, SeedJob::default()).await;
    attach_file(&app, job_id, "report", "data_file", "a.csv", b"a").await?;
    seed_trigger_run(&app.pool, "nightly-report", "report", "failed", 60.0, None).await;
    seed_timed_run(&app.pool, "report", 5.0).await;
    let worker = register_worker(&app.pool, "w-closed").await;
    seed_step(
        &app.pool,
        job_id,
        SeedStep {
            step: "build",
            worker_id: Some(worker),
            ..Default::default()
        },
    )
    .await;
    sqlx::query("ALTER TABLE user_group RENAME TO user_group_gone")
        .execute(&app.pool)
        .await?;

    let job = job_id.to_string();
    for (tool, args) in [
        ("get_job_status", json!({"job_id": job})),
        ("list_triggers", json!({"workspace": "default"})),
        ("list_attention_triggers", json!({})),
        ("list_jobs", json!({})),
        ("refresh_workspace", json!({"workspace": "default"})),
        (
            "get_task_stats",
            json!({"workspace": "default", "task_name": "report"}),
        ),
        ("get_stats", json!({})),
        (
            "get_input_file",
            json!({"job_id": job, "field": "data_file"}),
        ),
    ] {
        let resp = mcp(&app, &app.viewer, tool, args).await;
        assert_eq!(
            error_message(&resp),
            "Internal server error",
            "{tool}: {resp}"
        );
    }
    let detail = payload(
        &mcp(
            &app,
            &app.viewer,
            "get_worker",
            json!({"worker_id": worker.to_string()}),
        )
        .await,
    );
    assert_eq!(detail["steps"]["total"], json!(0), "{detail}");
    // The admin does not read groups: unaffected.
    let detail = payload(&mcp(&app, &app.admin, "get_job_status", json!({"job_id": job})).await);
    assert_eq!(detail["job_id"], json!(job_id));
    Ok(())
}
