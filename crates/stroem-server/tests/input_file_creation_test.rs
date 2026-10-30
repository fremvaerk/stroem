//! Accepting `file` input values at job creation (spec
//! 2026-10-29-file-inputs-design.md § 4.4 – § 4.6, § 8): A1 own upload for the
//! creations a caller starts directly, A2 lineage for Re-run / Restart / task
//! retry / `type: task` child, the required check in every creation mode, and
//! the value-free typed 400.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use futures_util::stream;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::file_field::new_file_id;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{ApiKeyRepo, JobInputFileRepo, JobRepo, JobStepRepo, UserRepo};
use stroem_server::auth::generate_api_key;
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    AuthConfig, DbConfig, LogStorageConfig, McpConfig, RetentionConfig, ServerConfig,
    WorkspaceSourceDef,
};
use stroem_server::input_files::upload::{store_upload, UploadTarget};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

use crate::common::capture::capture;
use crate::common::pinned::job_log_text;

const CANNOT_USE: &str = "Input field 'data_file': the file cannot be used";
const REQUIRED: &str = "Input field 'data_file': a file is required";
const WORKER_TOKEN: &str = "test-worker-token";

/// `__FILE__` is replaced with an uploaded file id (or a placeholder) by
/// [`workspace`], so a trigger or a hook can name a real upload.
const YAML: &str = r#"
actions:
  noop: { type: script, script: "true" }
  call-required: { type: task, task: files-required }
  call-files: { type: task, task: files }
tasks:
  files:
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
  files-other:
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
  files-required:
    input: { data_file: { type: file, required: true } }
    flow: { run: { action: noop } }
  plain:
    input: { name: { type: string } }
    flow: { run: { action: noop } }
  retrying:
    input: { data_file: { type: file } }
    retry: { max_attempts: 3, delay: 1s }
    flow: { run: { action: noop } }
  parent-required-child:
    flow: { call: { action: call-required } }
  hooked-required:
    flow: { run: { action: noop } }
    on_success:
      - action: call-required
  hooked-files:
    flow: { run: { action: noop } }
    on_success:
      - action: call-files
        input: { data_file: "__FILE__" }
  consumer:
    flow: { run: { action: noop } }
triggers:
  feed-required: { type: event_source, task: consumer, target_task: files-required }
  feed-files: { type: event_source, task: consumer, target_task: files }
  nightly-required: { type: scheduler, cron: "0 2 * * *", task: files-required }
  nightly-files:
    type: scheduler
    cron: "0 2 * * *"
    task: files
    input: { data_file: "__FILE__" }
  wh-files:
    type: webhook
    name: wh-files
    task: files
    input: { data_file: "__FILE__" }
"#;

fn workspace(file_id: &str) -> WorkspaceConfig {
    serde_yaml::from_str(&YAML.replace("__FILE__", file_id)).unwrap()
}

#[derive(Default)]
struct Opts {
    auth: bool,
}

struct App {
    router: Router,
    state: AppState,
    pool: PgPool,
    _tmp: TempDir,
}

async fn build_app(opts: Opts) -> Result<App> {
    let pool = stroem_test_support::test_pool().await;
    let tmp = TempDir::new()?;
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let archive_dir = tmp.path().join("input-archive");
    std::fs::create_dir_all(&archive_dir)?;

    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig {
            url: "postgres://unused".to_string(),
        },
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
        worker_token: WORKER_TOKEN.to_string(),
        auth: opts.auth.then(|| AuthConfig {
            jwt_secret: "test-jwt-secret-key".to_string(),
            refresh_secret: "test-refresh-secret-key".to_string(),
            base_url: None,
            providers: HashMap::new(),
            initial_user: None,
            rate_limit: Default::default(),
        }),
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

    // "other" has the same tasks but no triggers, so a webhook name is unique.
    let mut other = workspace("none");
    other.triggers.clear();
    let mgr = WorkspaceManager::from_configs(vec![
        ("default".to_string(), workspace("none"), None),
        ("other".to_string(), other, None),
    ]);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(archive_dir));
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_input_blob(Some(archive));
    let router = build_router(state.clone(), CancellationToken::new());
    Ok(App {
        router,
        state,
        pool,
        _tmp: tmp,
    })
}

/// Store an upload for `ws`/`task` owned by `user`; returns its file id.
async fn upload(
    state: &AppState,
    ws: &str,
    task: &str,
    user: Option<Uuid>,
    bytes: &[u8],
) -> String {
    let body: futures_core::stream::BoxStream<'static, anyhow::Result<bytes::Bytes>> =
        Box::pin(stream::iter(vec![Ok(bytes::Bytes::copy_from_slice(bytes))]));
    store_upload(
        state,
        UploadTarget {
            workspace: ws,
            task_name: task,
            original_name: "data.csv",
            content_type: "text/csv",
            user_id: user,
            origin: "api",
        },
        body,
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:?}"))
    .unwrap()
    .file_id
}

async fn user(pool: &PgPool) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    let email = format!("{user_id}@test.com");
    UserRepo::create(pool, user_id, &email, None, None)
        .await
        .unwrap();
    let (raw, hash) = generate_api_key();
    ApiKeyRepo::create(pool, &hash, user_id, "test", &raw[..12], None)
        .await
        .unwrap();
    (user_id, raw)
}

async fn post(app: &App, uri: &str, key: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    let resp = app
        .router
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn execute(app: &App, task: &str, key: Option<&str>, body: Value) -> (StatusCode, Value) {
    post(
        app,
        &format!("/api/workspaces/default/tasks/{task}/execute"),
        key,
        body,
    )
    .await
}

fn job_id_of(body: &Value) -> Uuid {
    body["job_id"].as_str().unwrap().parse().unwrap()
}

async fn attached(pool: &PgPool, job_id: Uuid) -> Option<String> {
    JobInputFileRepo::get_for_field(pool, job_id, "data_file")
        .await
        .unwrap()
        .map(|r| r.file_id)
}

async fn jobs_of_task(pool: &PgPool, task: &str) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT job_id FROM job WHERE task_name = $1 ORDER BY created_at")
        .bind(task)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn expire(pool: &PgPool, file_id: &str) {
    sqlx::query("UPDATE input_file SET created_at = NOW() - interval '2 days' WHERE file_id = $1")
        .bind(file_id)
        .execute(pool)
        .await
        .unwrap();
}

/// Settle `job_id` as completed (its single root step `run` completed).
async fn complete(app: &App, job_id: Uuid) {
    JobStepRepo::mark_completed(&app.pool, job_id, "run", None)
        .await
        .unwrap();
    app.state.settlement().advance(job_id).await.unwrap();
    let job = JobRepo::get(&app.pool, job_id).await.unwrap().unwrap();
    assert_eq!(job.status, "completed", "job {job_id} did not complete");
}

async fn mcp_execute(app: &App, task: &str, input: Value) -> Value {
    let mcp = |sid: Option<&str>, body: Value| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(s) = sid {
            b = b.header("Mcp-Session-Id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };
    let init = app
        .router
        .clone()
        .oneshot(mcp(
            None,
            json!({
                "jsonrpc": "2.0", "method": "initialize", "id": 0,
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "1.0"}
                }
            }),
        ))
        .await
        .unwrap();
    let sid = init
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let resp = app
        .router
        .clone()
        .oneshot(mcp(
            sid.as_deref(),
            json!({
                "jsonrpc": "2.0", "method": "tools/call", "id": 2,
                "params": {
                    "name": "execute_task",
                    "arguments": {"workspace": "default", "task_name": task, "input": input}
                }
            }),
        ))
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn emit(app: &App, task: &str, trigger: &str, input: Value) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri("/worker/event-source/emit")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {WORKER_TOKEN}"))
        .body(Body::from(
            json!({
                "workspace": "default",
                "task": task,
                "input": input,
                "source_id": format!("default/{trigger}"),
            })
            .to_string(),
        ))
        .unwrap();
    app.router.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn execute_accepts_own_upload_and_attaches() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let id = upload(&app.state, "default", "files", None, b"a,b\n").await;

    let (status, body) = execute(&app, "files", None, json!({"input": {"data_file": id}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let job_id = job_id_of(&body);

    assert_eq!(
        attached(&app.pool, job_id).await.as_deref(),
        Some(id.as_str())
    );
    let job = JobRepo::get(&app.pool, job_id).await?.unwrap();
    assert_eq!(job.input.unwrap()["data_file"], json!(id));
    assert_eq!(job.raw_input.unwrap()["data_file"], json!(id));
    Ok(())
}

#[tokio::test]
async fn execute_same_upload_twice_within_ttl() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let id = upload(&app.state, "default", "files", None, b"x").await;
    for _ in 0..2 {
        let (status, body) =
            execute(&app, "files", None, json!({"input": {"data_file": id}})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            attached(&app.pool, job_id_of(&body)).await.as_deref(),
            Some(id.as_str())
        );
    }
    Ok(())
}

#[tokio::test]
async fn execute_rejects_unknown_other_user_other_task_other_workspace_expired() -> Result<()> {
    let app = build_app(Opts { auth: true }).await?;
    let (alice, alice_key) = user(&app.pool).await;
    let (bob, _) = user(&app.pool).await;

    let unknown = new_file_id();
    let other_user = upload(&app.state, "default", "files", Some(bob), b"x").await;
    let other_task = upload(&app.state, "default", "files-other", Some(alice), b"x").await;
    let other_ws = upload(&app.state, "other", "files", Some(alice), b"x").await;
    let expired = upload(&app.state, "default", "files", Some(alice), b"x").await;
    expire(&app.pool, &expired).await;

    for (case, id) in [
        ("unknown", &unknown),
        ("other user", &other_user),
        ("other task", &other_task),
        ("other workspace", &other_ws),
        ("expired", &expired),
    ] {
        let (status, body) = execute(
            &app,
            "files",
            Some(&alice_key),
            json!({"input": {"data_file": id}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {body}");
        assert_eq!(body["error"], json!(CANNOT_USE), "{case}");
    }
    assert!(jobs_of_task(&app.pool, "files").await.is_empty());

    // The same caller's own fresh upload is accepted.
    let own = upload(&app.state, "default", "files", Some(alice), b"x").await;
    let (status, body) = execute(
        &app,
        "files",
        Some(&alice_key),
        json!({"input": {"data_file": own}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

#[tokio::test]
async fn execute_rejects_deleting_file() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let id = upload(&app.state, "default", "files", None, b"x").await;
    sqlx::query("UPDATE input_file SET deleting_at = NOW() WHERE file_id = $1")
        .bind(&id)
        .execute(&app.pool)
        .await?;
    let (status, body) = execute(&app, "files", None, json!({"input": {"data_file": id}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!(CANNOT_USE));
    Ok(())
}

#[tokio::test]
async fn execute_rejects_non_string_value() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    for value in [
        json!(42),
        json!({"name": "x"}),
        json!(["a"]),
        json!("not-a-file-id"),
    ] {
        let (status, body) =
            execute(&app, "files", None, json!({"input": {"data_file": value}})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}: {body}");
        assert_eq!(body["error"], json!(CANNOT_USE), "{value}");
    }
    // Absent, null and "" on an optional field: no file, job created.
    for input in [
        json!({}),
        json!({"data_file": null}),
        json!({"data_file": ""}),
    ] {
        let (status, body) = execute(&app, "files", None, json!({"input": input})).await;
        assert_eq!(status, StatusCode::OK, "{input}: {body}");
        assert_eq!(attached(&app.pool, job_id_of(&body)).await, None);
    }
    // A task without file fields is unchanged.
    let (status, body) = execute(&app, "plain", None, json!({"input": {"name": "x"}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

#[tokio::test]
async fn required_file_missing_fails_in_every_mode() -> Result<()> {
    let app = build_app(Opts::default()).await?;

    // Execute: 400 whose outermost text says "required".
    for input in [
        json!({}),
        json!({"data_file": null}),
        json!({"data_file": ""}),
    ] {
        let (status, body) = execute(&app, "files-required", None, json!({"input": input})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{input}: {body}");
        assert_eq!(body["error"], json!(REQUIRED));
        assert!(body["error"].as_str().unwrap().contains("required"));
    }

    // MCP execute: invalid params.
    let resp = mcp_execute(&app, "files-required", json!({})).await;
    assert_eq!(resp["error"]["code"], json!(-32602), "{resp}");
    assert_eq!(resp["error"]["message"], json!(REQUIRED), "{resp}");
    assert!(jobs_of_task(&app.pool, "files-required").await.is_empty());

    // Re-run without a value.
    let id = upload(&app.state, "default", "files-required", None, b"x").await;
    let (status, body) = execute(
        &app,
        "files-required",
        None,
        json!({"input": {"data_file": id}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let source = job_id_of(&body);
    let (status, body) = execute(
        &app,
        "files-required",
        None,
        json!({"input": {}, "source_job_id": source}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!(REQUIRED));

    // Restart of a source whose field was absent (the field became required
    // after the source ran).
    let (status, body) = execute(&app, "files", None, json!({"input": {}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let absent_source = job_id_of(&body);
    complete(&app, absent_source).await;
    let mut cfg = workspace("none");
    cfg.tasks
        .get_mut("files")
        .unwrap()
        .input
        .get_mut("data_file")
        .unwrap()
        .required = true;
    app.state
        .workspaces
        .replace_config_for_test("default", cfg)
        .await;
    let (status, body) = post(
        &app,
        &format!("/api/jobs/{absent_source}/restart"),
        None,
        json!({"from_step": "run"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!(REQUIRED));
    app.state
        .workspaces
        .replace_config_for_test("default", workspace("none"))
        .await;

    // `type: task` child: the parent step fails with the typed text.
    let (status, body) = execute(&app, "parent-required-child", None, json!({"input": {}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let parent = job_id_of(&body);
    let step = JobStepRepo::get_steps_for_job(&app.pool, parent)
        .await?
        .into_iter()
        .find(|s| s.step_name == "call")
        .unwrap();
    assert_eq!(step.status, "failed");
    // Refused by the child's creation (the parent passed no value at all).
    assert_eq!(
        step.error_message.as_deref(),
        Some(format!("Failed to create child job for task 'files-required': {REQUIRED}").as_str()),
    );

    // Event-source emit: dropped and logged.
    let before = jobs_of_task(&app.pool, "files-required").await.len();
    let (logs, guard) = capture();
    let status = emit(&app, "files-required", "feed-required", json!({})).await;
    drop(guard);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        jobs_of_task(&app.pool, "files-required").await.len(),
        before
    );
    assert!(logs.text().contains(REQUIRED), "{}", logs.text());

    // `type: task` hook: logged to the source job, no hook job.
    let (status, body) = execute(&app, "hooked-required", None, json!({"input": {}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hooked = job_id_of(&body);
    complete(&app, hooked).await;
    let log = job_log_text(&app.pool, &app.state, hooked).await;
    assert!(log.contains(REQUIRED), "{log}");
    assert_eq!(
        jobs_of_task(&app.pool, "files-required").await.len(),
        before
    );
    Ok(())
}

#[tokio::test]
async fn masked_sentinel_in_file_field_is_not_replayed() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let id = upload(&app.state, "default", "files", None, b"x").await;
    let (status, body) = execute(&app, "files", None, json!({"input": {"data_file": id}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let source = job_id_of(&body);

    let (status, body) = execute(
        &app,
        "files",
        None,
        json!({"input": {"data_file": "••••••"}, "source_job_id": source}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!(CANNOT_USE));
    Ok(())
}

#[tokio::test]
async fn rerun_reuses_source_file_a2() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let id = upload(&app.state, "default", "files", None, b"x").await;
    let (status, body) = execute(&app, "files", None, json!({"input": {"data_file": id}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let source = job_id_of(&body);
    // Past the TTL A1 no longer matches: only the lineage edge can accept it.
    expire(&app.pool, &id).await;

    // The id sent explicitly (the UI prefill), and the field replayed.
    for request in [
        json!({"input": {"data_file": id}, "source_job_id": source}),
        json!({"input": {}, "source_job_id": source, "replay_fields": ["data_file"]}),
    ] {
        let (status, body) = execute(&app, "files", None, request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{request}: {body}");
        let rerun = job_id_of(&body);
        assert_eq!(
            attached(&app.pool, rerun).await.as_deref(),
            Some(id.as_str())
        );
        let job = JobRepo::get(&app.pool, rerun).await?.unwrap();
        assert_eq!(job.source_job_id, Some(source));
    }

    // A file attached to an unrelated job is not lineage.
    let other = upload(&app.state, "default", "files", None, b"y").await;
    let (status, body) = execute(&app, "files", None, json!({"input": {"data_file": other}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    expire(&app.pool, &other).await;
    let (status, body) = execute(
        &app,
        "files",
        None,
        json!({"input": {"data_file": other}, "source_job_id": source}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!(CANNOT_USE));
    Ok(())
}

#[tokio::test]
async fn restart_replays_source_file_a2() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let id = upload(&app.state, "default", "files", None, b"x").await;
    let (status, body) = execute(&app, "files", None, json!({"input": {"data_file": id}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let source = job_id_of(&body);
    complete(&app, source).await;
    expire(&app.pool, &id).await;

    let (status, body) = post(
        &app,
        &format!("/api/jobs/{source}/restart"),
        None,
        json!({"from_step": "run"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let restart = job_id_of(&body);
    assert_eq!(
        attached(&app.pool, restart).await.as_deref(),
        Some(id.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn retry_chain_after_ttl_uses_failed_job() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let id = upload(&app.state, "default", "retrying", None, b"x").await;
    let (status, body) = execute(&app, "retrying", None, json!({"input": {"data_file": id}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let job1 = job_id_of(&body);

    JobStepRepo::mark_failed(&app.pool, job1, "run", "boom").await?;
    app.state.settlement().advance(job1).await?;
    let job2 = JobRepo::get(&app.pool, job1)
        .await?
        .unwrap()
        .retry_job_id
        .expect("retry 2 created");
    assert_eq!(
        attached(&app.pool, job2).await.as_deref(),
        Some(id.as_str())
    );

    // Expire the upload and drop the ROOT's attachment: retry 3 can then only
    // be accepted through the job that just failed (job 2), not the root.
    expire(&app.pool, &id).await;
    sqlx::query("DELETE FROM job_input_file WHERE job_id = $1")
        .bind(job1)
        .execute(&app.pool)
        .await?;

    JobStepRepo::mark_failed(&app.pool, job2, "run", "boom").await?;
    app.state.settlement().advance(job2).await?;
    let job3 = JobRepo::get(&app.pool, job2)
        .await?
        .unwrap()
        .retry_job_id
        .unwrap_or_else(|| {
            panic!("retry 3 not created");
        });
    assert_eq!(
        attached(&app.pool, job3).await.as_deref(),
        Some(id.as_str())
    );
    let row3 = JobRepo::get(&app.pool, job3).await?.unwrap();
    assert_eq!(row3.retry_of_job_id, Some(job1));
    Ok(())
}

#[tokio::test]
async fn trigger_and_hook_creation_reject_valid_upload() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    // A valid NULL-owner upload (auth off): A1 would accept it, but a trigger
    // or a hook never resolves a value through A1.
    let id = upload(&app.state, "default", "files", None, b"x").await;
    app.state
        .workspaces
        .replace_config_for_test("default", workspace(&id))
        .await;

    // Webhook: the typed 400.
    let (status, body) = post(&app, "/hooks/wh-files", None, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!(CANNOT_USE));

    // Scheduler: logged, no job; with a required field: Required.
    let (logs, guard) = capture();
    stroem_server::scheduler::fire_trigger_once(
        &app.state,
        &app.state.workspaces,
        &app.state.workspaces,
        "default/nightly-files",
    )
    .await;
    stroem_server::scheduler::fire_trigger_once(
        &app.state,
        &app.state.workspaces,
        &app.state.workspaces,
        "default/nightly-required",
    )
    .await;
    drop(guard);
    let text = logs.text();
    assert!(text.contains(CANNOT_USE), "{text}");
    assert!(text.contains(REQUIRED), "{text}");
    assert!(jobs_of_task(&app.pool, "files").await.is_empty());
    assert!(jobs_of_task(&app.pool, "files-required").await.is_empty());

    // `type: task` hook: logged to the source job, no hook job.
    let (status, body) = execute(&app, "hooked-files", None, json!({"input": {}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hooked = job_id_of(&body);
    complete(&app, hooked).await;
    let log = job_log_text(&app.pool, &app.state, hooked).await;
    assert!(log.contains(CANNOT_USE), "{log}");
    assert!(jobs_of_task(&app.pool, "files").await.is_empty());
    Ok(())
}

#[tokio::test]
async fn event_source_emit_with_file_value_is_dropped() -> Result<()> {
    let app = build_app(Opts::default()).await?;
    let id = upload(&app.state, "default", "files", None, b"x").await;
    let (logs, guard) = capture();
    let status = emit(&app, "files", "feed-files", json!({"data_file": id})).await;
    drop(guard);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(jobs_of_task(&app.pool, "files").await.is_empty());
    assert!(logs.text().contains(CANNOT_USE), "{}", logs.text());
    Ok(())
}
