//! Claim-time input files (spec 2026-10-29-file-inputs-design.md § 5.1 –
//! § 5.4, § 6.1, § 6.5): the render root per runner, `input_files` in the
//! claim response, the old-worker failure, the A3 rule in the step-input
//! render, `merge_missing_action_fields` by target type, and the worker
//! download endpoint.

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use bytes::Bytes;
use futures_util::stream;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{JobInputFileRepo, JobStepRepo};
use stroem_server::blob_storage::{Blob, BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    DbConfig, LogStorageConfig, RetentionConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::input_files::upload::{store_upload, UploadResponse, UploadTarget};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

const WORKER_TOKEN: &str = "test-worker-token";
const INPUTS_DIR: &str = "/w/in";
const OLD_WORKER: &str =
    "this step has file inputs, which the worker does not support; upgrade the worker";

const YAML: &str = r#"
actions:
  noop: { type: script, script: "true" }
  cat-file:
    type: script
    script: 'cat "{{ input.data_file }}"'
    input: { data_file: { type: file } }
  cat-file-docker:
    type: script
    runner: docker
    script: 'cat "{{ input.data_file }}"'
    input: { data_file: { type: file } }
  cat-file-pod:
    type: script
    runner: pod
    script: 'cat "{{ input.data_file }}"'
    input: { data_file: { type: file } }
  docker-cat:
    type: docker
    image: alpine:3
    cmd: 'cat "{{ input.data_file }}"'
    input: { data_file: { type: file } }
  pod-cat:
    type: pod
    image: alpine:3
    cmd: 'cat "{{ input.data_file }}"'
    input: { data_file: { type: file } }
  uses-path:
    type: script
    script: 'cat "{{ input.path }}"'
    input: { path: { type: string } }
  uses-file:
    type: script
    script: 'cat "{{ input.data }}"'
    input: { data: { type: file } }
  merge-string:
    type: script
    script: 'cat "{{ input.data_file }}"'
    input: { data_file: { type: string } }
  two-files:
    type: script
    script: 'echo "{{ input.a }}" "{{ input.s1a }}"'
    input: { a: { type: file }, s1a: { type: string } }
  uses-file-each:
    type: script
    script: 'cat "{{ input.data }}" "{{ each.item }}"'
    input: { data: { type: file } }
  think:
    type: agent
    provider: anthropic
    model: claude-sonnet-5
    prompt: 'read {{ input.data_file }}'
tasks:
  local:
    input: { data_file: { type: file } }
    flow: { run: { action: cat-file } }
  runner-docker:
    input: { data_file: { type: file } }
    flow: { run: { action: cat-file-docker } }
  runner-pod:
    input: { data_file: { type: file } }
    flow: { run: { action: cat-file-pod } }
  type-docker:
    input: { data_file: { type: file } }
    flow: { run: { action: docker-cat } }
  type-pod:
    input: { data_file: { type: file } }
    flow: { run: { action: pod-cat } }
  to-string:
    input: { data_file: { type: file } }
    flow: { run: { action: uses-path, input: { path: "{{ input.data_file }}" } } }
  to-file:
    input: { data_file: { type: file } }
    flow: { run: { action: uses-file, input: { data: "{{ input.data_file }}" } } }
  merge-to-string:
    input: { data_file: { type: file } }
    flow: { run: { action: merge-string } }
  merge-retry:
    input: { data_file: { type: file } }
    flow:
      run:
        action: merge-string
        retry: { max_attempts: 2, delay: 1s }
  collide:
    input: { a: { type: file }, b: { type: file } }
    flow:
      run:
        action: two-files
        input: { a: "{{ input.b }}", s1a: "{{ input.a }}" }
  two-fields:
    input: { a: { type: file }, b: { type: file } }
    flow: { run: { action: noop } }
  unused:
    input: { data_file: { type: file } }
    flow: { run: { action: noop } }
  plain:
    input: { name: { type: string } }
    flow: { run: { action: noop } }
  ask:
    input: { data_file: { type: file } }
    flow: { run: { action: think } }
  loop-files:
    input: { data_file: { type: file } }
    flow:
      run:
        action: uses-file-each
        for_each: "{{ [input.data_file] | json_encode() }}"
        input: { data: "{{ each.item }}" }
  foreign:
    input: { data_file: { type: file }, other: { type: string } }
    flow: { run: { action: uses-file, input: { data: "{{ input.other }}" } } }
"#;

struct App {
    router: Router,
    state: AppState,
    pool: PgPool,
    archive: Arc<dyn BlobArchive>,
    _tmp: TempDir,
}

fn server_config(tmp: &TempDir) -> ServerConfig {
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    ServerConfig {
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
        auth: None,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: None,
        mcp: None,
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
    }
}

/// A server state over `pool` with `archive` as the input-file store.
fn state_with(pool: &PgPool, tmp: &TempDir, archive: Option<Arc<dyn BlobArchive>>) -> AppState {
    let config = server_config(tmp);
    let ws: WorkspaceConfig = serde_yaml::from_str(YAML).unwrap();
    let mgr = WorkspaceManager::from_configs(vec![("default".to_string(), ws, None)]);
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
        .with_input_blob(archive)
}

async fn build_app() -> Result<App> {
    let pool = stroem_test_support::test_pool().await;
    let tmp = TempDir::new()?;
    let archive_dir = tmp.path().join("input-archive");
    std::fs::create_dir_all(&archive_dir)?;
    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(archive_dir));
    let state = state_with(&pool, &tmp, Some(archive.clone()));
    let router = build_router(state.clone(), CancellationToken::new());
    Ok(App {
        router,
        state,
        pool,
        archive,
        _tmp: tmp,
    })
}

/// Store an upload named `name` for `task`; auth is off, so no owner.
async fn upload(app: &App, task: &str, name: &str, bytes: &[u8]) -> UploadResponse {
    let body: futures_core::stream::BoxStream<'static, anyhow::Result<Bytes>> =
        Box::pin(stream::iter(vec![Ok(Bytes::copy_from_slice(bytes))]));
    store_upload(
        &app.state,
        UploadTarget {
            workspace: "default",
            task_name: task,
            original_name: name,
            content_type: "text/csv",
            user_id: None,
            origin: "api",
        },
        body,
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:?}"))
    .unwrap()
}

async fn send(router: &Router, req: Request<Body>) -> (StatusCode, http::HeaderMap, Bytes) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes)
}

fn json_of(bytes: &Bytes) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

async fn execute(app: &App, task: &str, input: Value) -> Uuid {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/workspaces/default/tasks/{task}/execute"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "input": input }).to_string()))
        .unwrap();
    let (status, _, bytes) = send(&app.router, req).await;
    let body = json_of(&bytes);
    assert_eq!(status, StatusCode::OK, "execute {task}: {body}");
    body["job_id"].as_str().unwrap().parse().unwrap()
}

async fn worker_post(app: &App, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {WORKER_TOKEN}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, _, bytes) = send(&app.router, req).await;
    (status, json_of(&bytes))
}

/// Register a worker with `caps` and claim once, sending `inputs_dir` when
/// given. A failed claim answers 422 `{"error": …}`.
async fn claim(app: &App, caps: &[&str], inputs_dir: Option<&str>) -> (StatusCode, Value) {
    let (status, body) = worker_post(
        app,
        "/worker/register",
        json!({"name": "files-worker", "capabilities": caps}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register: {body}");
    let worker_id = body["worker_id"].as_str().unwrap().to_string();
    let mut req = json!({"worker_id": worker_id, "capabilities": caps});
    if let Some(dir) = inputs_dir {
        req["inputs_dir"] = json!(dir);
    }
    worker_post(app, "/worker/jobs/claim", req).await
}

/// Claim one `script` step with the test worker root; must succeed.
async fn claim_local(app: &App) -> Value {
    let (status, body) = claim(app, &["script"], Some(INPUTS_DIR)).await;
    assert_eq!(status, StatusCode::OK, "claim: {body}");
    assert!(body["job_id"].is_string(), "no work claimed: {body}");
    body
}

async fn step_row(pool: &PgPool, job_id: Uuid, step: &str) -> stroem_db::JobStepRow {
    JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step_name == step)
        .unwrap()
}

async fn worker_get(
    router: &Router,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, http::HeaderMap, Bytes) {
    let mut b = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    send(router, b.body(Body::empty()).unwrap()).await
}

fn download_uri(job_id: Uuid, file_id: &str) -> String {
    format!("/worker/jobs/{job_id}/input-files/{file_id}")
}

fn path_under(root: &str, file_id: &str, name: &str) -> String {
    format!("{root}/{file_id}/{name}")
}

// ─── claim: render root and input_files ─────────────────────────────────

#[tokio::test]
async fn local_step_renders_worker_root() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "local", "report.csv", b"a,b\n1,2\n").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;

    let body = claim_local(&app).await;
    assert_eq!(body["job_id"], json!(job_id.to_string()));
    let script = body["action_spec"]["script"].as_str().unwrap();
    assert_eq!(
        script,
        format!(
            "cat \"{}\"",
            path_under(INPUTS_DIR, &up.file_id, "report.csv")
        )
    );
    assert_eq!(
        body["input_files"],
        json!([{
            "file_id": up.file_id,
            "safe_name": "report.csv",
            "size_bytes": 8,
            "sha256": up.sha256,
        }])
    );
    // The stored value of a `file` action field stays the id.
    assert_eq!(body["input"]["data_file"], json!(up.file_id));
    Ok(())
}

#[tokio::test]
async fn docker_and_pod_steps_render_inputs_root() -> Result<()> {
    let app = build_app().await?;
    let mut expected = HashMap::new();
    for (task, action) in [
        ("runner-docker", "cat-file-docker"),
        ("type-docker", "docker-cat"),
        ("runner-pod", "cat-file-pod"),
        ("type-pod", "pod-cat"),
    ] {
        let up = upload(&app, task, "report.csv", b"x").await;
        execute(&app, task, json!({"data_file": up.file_id})).await;
        expected.insert(action.to_string(), up.file_id);
    }

    for _ in 0..4 {
        let (status, body) = claim(&app, &["docker", "kubernetes"], Some(INPUTS_DIR)).await;
        assert_eq!(status, StatusCode::OK, "claim: {body}");
        let action = body["action_name"].as_str().unwrap().to_string();
        let file_id = expected.remove(&action).expect("each step claimed once");
        let spec = &body["action_spec"];
        let text = spec["script"].as_str().or(spec["cmd"].as_str()).unwrap();
        assert_eq!(
            text,
            format!("cat \"{}\"", path_under("/inputs", &file_id, "report.csv")),
            "{action}"
        );
        assert_eq!(
            body["input_files"][0]["file_id"],
            json!(file_id),
            "{action}"
        );
    }
    assert!(expected.is_empty());
    Ok(())
}

#[tokio::test]
async fn step_input_path_into_string_action_field() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "to-string", "report.csv", b"x").await;
    let job_id = execute(&app, "to-string", json!({"data_file": up.file_id})).await;

    let body = claim_local(&app).await;
    let path = path_under(INPUTS_DIR, &up.file_id, "report.csv");
    assert_eq!(body["input"]["path"], json!(path));
    assert_eq!(
        body["action_spec"]["script"].as_str().unwrap(),
        format!("cat \"{path}\"")
    );
    let row = step_row(&app.pool, job_id, "run").await;
    assert_eq!(row.input.unwrap()["path"], json!(path));
    Ok(())
}

#[tokio::test]
async fn file_action_field_receives_id_and_renders_path() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "to-file", "report.csv", b"x").await;
    let job_id = execute(&app, "to-file", json!({"data_file": up.file_id})).await;

    let body = claim_local(&app).await;
    assert_eq!(body["input"]["data"], json!(up.file_id));
    assert_eq!(
        body["action_spec"]["script"].as_str().unwrap(),
        format!(
            "cat \"{}\"",
            path_under(INPUTS_DIR, &up.file_id, "report.csv")
        )
    );
    let row = step_row(&app.pool, job_id, "run").await;
    assert_eq!(row.input.unwrap()["data"], json!(up.file_id));
    Ok(())
}

#[tokio::test]
async fn merge_missing_by_target_type() -> Result<()> {
    let app = build_app().await?;

    // Unset `file` action field ← the id (a valid A3 value).
    let up = upload(&app, "local", "report.csv", b"x").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;
    claim_local(&app).await;
    let row = step_row(&app.pool, job_id, "run").await;
    assert_eq!(row.input.unwrap()["data_file"], json!(up.file_id));

    // Unset `string` action field ← the path under the claim root.
    let up = upload(&app, "merge-to-string", "report.csv", b"x").await;
    let job_id = execute(&app, "merge-to-string", json!({"data_file": up.file_id})).await;
    let body = claim_local(&app).await;
    let path = path_under(INPUTS_DIR, &up.file_id, "report.csv");
    assert_eq!(body["input"]["data_file"], json!(path));
    assert_eq!(
        body["action_spec"]["script"].as_str().unwrap(),
        format!("cat \"{path}\"")
    );
    let row = step_row(&app.pool, job_id, "run").await;
    assert_eq!(row.input.unwrap()["data_file"], json!(path));
    Ok(())
}

/// A retried `runner: local` step is claimed again by a worker with another
/// `inputs_dir` (a fresh one per claim, spec § 5.2): the merged path must
/// follow the new root, not keep the first claim's.
#[tokio::test]
async fn reclaim_after_retry_renders_the_new_worker_root() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "merge-retry", "report.csv", b"x").await;
    let job_id = execute(&app, "merge-retry", json!({"data_file": up.file_id})).await;

    let body = claim_local(&app).await;
    let first = path_under(INPUTS_DIR, &up.file_id, "report.csv");
    assert_eq!(body["input"]["data_file"], json!(first));

    // The attempt fails; the step is retried in place.
    let (status, resp) = worker_post(
        &app,
        &format!("/worker/jobs/{job_id}/steps/run/complete"),
        json!({"exit_code": 1, "error": "boom"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(step_row(&app.pool, job_id, "run").await.status, "ready");
    sqlx::query("UPDATE job_step SET retry_at = NULL WHERE job_id = $1")
        .bind(job_id)
        .execute(&app.pool)
        .await?;

    let (status, body) = claim(&app, &["script"], Some("/w/other")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["job_id"], json!(job_id.to_string()));
    let second = path_under("/w/other", &up.file_id, "report.csv");
    assert_eq!(body["input"]["data_file"], json!(second));
    assert_eq!(
        body["action_spec"]["script"].as_str().unwrap(),
        format!("cat \"{second}\"")
    );
    let row = step_row(&app.pool, job_id, "run").await;
    assert_eq!(row.input.unwrap()["data_file"], json!(second));
    Ok(())
}

#[tokio::test]
async fn collision_case_claim_lists_two_files_with_distinct_paths() -> Result<()> {
    let app = build_app().await?;
    let x = upload(&app, "collide", "report.csv", b"content of a").await;
    let y = upload(&app, "collide", "report.csv", b"content of b").await;
    let job_id = execute(&app, "collide", json!({"a": x.file_id, "b": y.file_id})).await;

    let body = claim_local(&app).await;
    let path_x = path_under(INPUTS_DIR, &x.file_id, "report.csv");
    let path_y = path_under(INPUTS_DIR, &y.file_id, "report.csv");
    assert_ne!(path_x, path_y);
    // S1: task field `a` is file X; S2/S3: action field `a` is file Y.
    assert_eq!(body["input"]["s1a"], json!(path_x));
    assert_eq!(body["input"]["a"], json!(y.file_id));
    assert_eq!(
        body["action_spec"]["script"].as_str().unwrap(),
        format!("echo \"{path_y}\" \"{path_x}\"")
    );
    let listed: Vec<&str> = body["input_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["file_id"].as_str().unwrap())
        .collect();
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&x.file_id.as_str()) && listed.contains(&y.file_id.as_str()));

    // Each path holds its own content.
    for (id, content) in [(&x.file_id, b"content of a"), (&y.file_id, b"content of b")] {
        let (status, _, bytes) =
            worker_get(&app.router, &download_uri(job_id, id), Some(WORKER_TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&bytes[..], &content[..]);
    }
    Ok(())
}

#[tokio::test]
async fn same_upload_in_two_fields_is_one_entry() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "two-fields", "report.csv", b"x").await;
    execute(
        &app,
        "two-fields",
        json!({"a": up.file_id, "b": up.file_id}),
    )
    .await;

    let body = claim_local(&app).await;
    let files = body["input_files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{body}");
    assert_eq!(files[0]["file_id"], json!(up.file_id));
    Ok(())
}

#[tokio::test]
async fn step_without_using_file_still_receives_it() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "unused", "report.csv", b"x").await;
    execute(&app, "unused", json!({"data_file": up.file_id})).await;

    let body = claim_local(&app).await;
    assert_eq!(body["input_files"][0]["file_id"], json!(up.file_id));
    assert_eq!(body["input_files"].as_array().unwrap().len(), 1);
    Ok(())
}

#[tokio::test]
async fn old_worker_without_inputs_dir_fails_step() -> Result<()> {
    let app = build_app().await?;

    // A local step and a container step both need a worker that reads
    // `input_files`: the signal is `inputs_dir`, whatever the root.
    for (task, caps) in [("local", ["script"]), ("type-docker", ["docker"])] {
        let up = upload(&app, task, "report.csv", b"x").await;
        let job_id = execute(&app, task, json!({"data_file": up.file_id})).await;
        let (status, body) = claim(&app, &caps, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{task}: {body}");
        assert_eq!(body["error"], json!(OLD_WORKER), "{task}");
        let row = step_row(&app.pool, job_id, "run").await;
        assert_eq!(row.status, "failed", "{task}");
        assert_eq!(row.error_message.as_deref(), Some(OLD_WORKER), "{task}");
    }

    // A job without files claims as before, also without `inputs_dir`.
    for (task, input) in [("plain", json!({"name": "n"})), ("unused", json!({}))] {
        let job_id = execute(&app, task, input).await;
        let (status, body) = claim(&app, &["script"], None).await;
        assert_eq!(status, StatusCode::OK, "{task}: {body}");
        assert_eq!(body["job_id"], json!(job_id.to_string()), "{task}");
        assert!(body.get("input_files").is_none(), "{task}: {body}");
    }
    Ok(())
}

#[tokio::test]
async fn agent_step_gets_no_input_files() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "ask", "report.csv", b"x").await;
    let job_id = execute(&app, "ask", json!({"data_file": up.file_id})).await;

    // No `inputs_dir`: an agent step never fails the old-worker rule.
    let (status, body) = claim(&app, &["agent"], None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["job_id"], json!(job_id.to_string()));
    assert!(body.get("input_files").is_none(), "{body}");
    // The prompt is informational: the canonical path, never the id alone.
    assert_eq!(
        body["agent_prompt"],
        json!(format!(
            "read {}",
            path_under("/inputs", &up.file_id, "report.csv")
        ))
    );
    Ok(())
}

#[tokio::test]
async fn relative_inputs_dir_is_400() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "local", "report.csv", b"x").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;

    for dir in ["w/in", "", "./in"] {
        let (status, body) = claim(&app, &["script"], Some(dir)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{dir:?}: {body}");
    }
    // Nothing was claimed.
    assert_eq!(step_row(&app.pool, job_id, "run").await.status, "ready");
    Ok(())
}

#[tokio::test]
async fn foreign_path_in_file_target_fails_claim_value_free() -> Result<()> {
    let app = build_app().await?;
    let mine = upload(&app, "foreign", "report.csv", b"x").await;
    let theirs = upload(&app, "local", "theirs.csv", b"y").await;
    execute(&app, "local", json!({"data_file": theirs.file_id})).await;
    // The other job's step is claimed first, so the next claim is ours.
    claim_local(&app).await;

    let cases = [
        // This job's own file, but under another root (the canonical one).
        path_under("/inputs", &mine.file_id, "report.csv"),
        // Another job's file id.
        theirs.file_id.clone(),
        // Another job's file, under the claim root.
        path_under(INPUTS_DIR, &theirs.file_id, "theirs.csv"),
    ];
    for other in cases {
        let job_id = execute(
            &app,
            "foreign",
            json!({"data_file": mine.file_id, "other": other}),
        )
        .await;
        let (status, body) = claim(&app, &["script"], Some(INPUTS_DIR)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{other}: {body}");
        let row = step_row(&app.pool, job_id, "run").await;
        assert_eq!(row.status, "failed");
        let msg = row.error_message.unwrap_or_default();
        assert!(
            msg.contains("Input field 'data': the file cannot be used"),
            "{msg}"
        );
        for leak in [
            mine.file_id.as_str(),
            theirs.file_id.as_str(),
            "report.csv",
            "theirs.csv",
            "/inputs",
            INPUTS_DIR,
        ] {
            assert!(!msg.contains(leak), "error names {leak:?}: {msg}");
            assert!(
                !body["error"].as_str().unwrap_or_default().contains(leak),
                "response names {leak:?}: {body}"
            );
        }
    }
    Ok(())
}

/// Spec revision 8: a `for_each` item is rendered under `/inputs` and
/// stored on the instance; a local claim sees it under its own root, so it
/// is a valid `file` target and the same path as the action's `file` field.
#[tokio::test]
async fn for_each_item_path_is_rebased_to_the_claim_root() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "loop-files", "report.csv", b"x").await;
    let job_id = execute(&app, "loop-files", json!({"data_file": up.file_id})).await;
    let instance = step_row(&app.pool, job_id, "run[0]").await;
    let canonical = path_under("/inputs", &up.file_id, "report.csv");
    assert_eq!(instance.loop_item, Some(json!(canonical)));

    let body = claim_local(&app).await;
    assert_eq!(body["step_name"], "run[0]");
    let path = path_under(INPUTS_DIR, &up.file_id, "report.csv");
    assert_eq!(body["input"]["data"], json!(up.file_id));
    assert_eq!(
        body["action_spec"]["script"].as_str().unwrap(),
        format!("cat \"{path}\" \"{path}\"")
    );
    Ok(())
}

// ─── worker download ────────────────────────────────────────────────────

#[tokio::test]
async fn worker_download_streams_attached_file() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "local", "report.csv", b"a,b\n1,2\n").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;

    let (status, headers, bytes) = worker_get(
        &app.router,
        &download_uri(job_id, &up.file_id),
        Some(WORKER_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..], b"a,b\n1,2\n");
    assert_eq!(headers["content-length"], "8");
    assert_eq!(headers["content-type"], "application/octet-stream");
    Ok(())
}

#[tokio::test]
async fn worker_download_unattached_is_404() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "local", "report.csv", b"x").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;
    let other = upload(&app, "local", "other.csv", b"y").await;
    let other_job = execute(&app, "local", json!({"data_file": other.file_id})).await;
    let loose = upload(&app, "local", "loose.csv", b"z").await;

    for uri in [
        // Attached to another job.
        download_uri(job_id, &other.file_id),
        download_uri(other_job, &up.file_id),
        // Attached to no job.
        download_uri(job_id, &loose.file_id),
        // No such job.
        download_uri(Uuid::new_v4(), &up.file_id),
        format!("/worker/jobs/not-a-uuid/input-files/{}", up.file_id),
        download_uri(job_id, "file_00000000000000000000000000000000"),
    ] {
        let (status, _, bytes) = worker_get(&app.router, &uri, Some(WORKER_TOKEN)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(&up.file_id), "{text}");
    }
    Ok(())
}

#[tokio::test]
async fn worker_download_missing_blob_is_404() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "local", "report.csv", b"x").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;
    let rec = JobInputFileRepo::get_attached(&app.pool, job_id, &up.file_id)
        .await?
        .unwrap();
    app.archive.delete(&rec.storage_key).await?;

    let (status, _, _) = worker_get(
        &app.router,
        &download_uri(job_id, &up.file_id),
        Some(WORKER_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn worker_download_requires_worker_token() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "local", "report.csv", b"x").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;
    let uri = download_uri(job_id, &up.file_id);

    for token in [None, Some("wrong-token")] {
        let (status, _, bytes) = worker_get(&app.router, &uri, token).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert!(bytes.is_empty() || !bytes.starts_with(b"x"));
    }
    Ok(())
}

#[tokio::test]
async fn worker_download_without_archive_is_503_with_retry_after() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "local", "report.csv", b"x").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;
    let uri = download_uri(job_id, &up.file_id);

    let no_archive = build_router(
        state_with(&app.pool, &app._tmp, None),
        CancellationToken::new(),
    );
    let (status, headers, bytes) = worker_get(&no_archive, &uri, Some(WORKER_TOKEN)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "5");
    assert_eq!(json_of(&bytes)["error"], "file storage is not configured");
    Ok(())
}

/// An archive whose every read fails.
struct FailingArchive;

#[async_trait::async_trait]
impl BlobArchive for FailingArchive {
    async fn put(&self, _: &str, _: &str, _: Bytes) -> anyhow::Result<()> {
        anyhow::bail!("simulated storage failure")
    }
    async fn get(&self, _: &str) -> anyhow::Result<Option<Blob>> {
        anyhow::bail!("simulated storage failure")
    }
    async fn delete(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_prefix(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn worker_download_storage_error_is_503_with_retry_after() -> Result<()> {
    let app = build_app().await?;
    let up = upload(&app, "local", "report.csv", b"x").await;
    let job_id = execute(&app, "local", json!({"data_file": up.file_id})).await;

    let failing = build_router(
        state_with(&app.pool, &app._tmp, Some(Arc::new(FailingArchive))),
        CancellationToken::new(),
    );
    let (status, headers, bytes) = worker_get(
        &failing,
        &download_uri(job_id, &up.file_id),
        Some(WORKER_TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "5");
    assert!(!String::from_utf8_lossy(&bytes).contains("simulated"));
    Ok(())
}
