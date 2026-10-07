//! The `json` input type end to end (spec 2026-10-06-json-input-type).

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
use stroem_db::{JobStepRepo, JobStepRow, WorkerRepo};
use stroem_server::config::{
    AclConfig, AuthConfig, DbConfig, LogStorageConfig, RetentionConfig, ServerConfig,
    WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

const WORKER_TOKEN: &str = "json-input-test-worker-token";

struct App {
    router: Router,
    pool: PgPool,
    mgr: Arc<WorkspaceManager>,
    _tmp: TempDir,
}

fn workspace(yaml: &str) -> WorkspaceConfig {
    serde_yaml::from_str(yaml).expect("workspace yaml")
}

async fn app(yaml: &str) -> Result<App> {
    app_with(yaml, None, None).await
}

async fn app_with(yaml: &str, auth: Option<AuthConfig>, acl: Option<AclConfig>) -> Result<App> {
    app_ws(&[("default", yaml)], auth, acl).await
}

/// One server over several in-memory workspaces (cross-workspace tests).
async fn app_ws(
    workspaces: &[(&str, &str)],
    auth: Option<AuthConfig>,
    acl: Option<AclConfig>,
) -> Result<App> {
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
        workspaces: workspaces
            .iter()
            .map(|(name, _)| {
                (
                    name.to_string(),
                    WorkspaceSourceDef::Folder {
                        triggers: true,
                        path: tmp.path().to_string_lossy().to_string(),
                    },
                )
            })
            .collect(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: WORKER_TOKEN.to_string(),
        auth,
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl,
        mcp: None,
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        pin_store: None,
    };
    let mgr = WorkspaceManager::from_configs(
        workspaces
            .iter()
            .map(|(name, yaml)| (name.to_string(), workspace(yaml), Some("rev-1".to_string())))
            .collect(),
    );
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    let mgr = Arc::clone(&state.workspaces);
    let router = build_router(state, CancellationToken::new());
    Ok(App {
        router,
        pool,
        mgr,
        _tmp: tmp,
    })
}

fn api(method: &str, uri: &str, body: Value, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json");
    if let Some(t) = token {
        b = b.header("Authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn worker(method: &str, uri: &str, body: Value) -> Request<Body> {
    api(method, uri, body, Some(WORKER_TOKEN))
}

async fn call(app: &App, req: Request<Body>) -> Result<(StatusCode, Value)> {
    let resp = app.router.clone().oneshot(req).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    let body = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes).to_string()}))
    };
    Ok((status, body))
}

async fn execute(app: &App, task: &str, body: Value) -> Result<(StatusCode, Value)> {
    call(
        app,
        api(
            "POST",
            &format!("/api/workspaces/default/tasks/{task}/execute"),
            body,
            None,
        ),
    )
    .await
}

async fn claim(app: &App) -> Result<(StatusCode, Value)> {
    let id = Uuid::new_v4();
    WorkerRepo::register(
        &app.pool,
        id,
        "json-test-worker",
        &["script".to_string()],
        &[],
        false,
        None,
    )
    .await?;
    call(
        app,
        worker(
            "POST",
            "/worker/jobs/claim",
            json!({"worker_id": id, "capabilities": ["script"]}),
        ),
    )
    .await
}

async fn complete(app: &App, job_id: &str, step: &str, output: Value) -> Result<StatusCode> {
    let (status, _) = call(
        app,
        worker(
            "POST",
            &format!("/worker/jobs/{job_id}/steps/{step}/complete"),
            json!({"output": output, "exit_code": 0}),
        ),
    )
    .await?;
    Ok(status)
}

async fn step_row(app: &App, job_id: &str, step: &str) -> Result<JobStepRow> {
    let job_id: Uuid = job_id.parse()?;
    let steps = JobStepRepo::get_steps_for_job(&app.pool, job_id).await?;
    Ok(steps
        .into_iter()
        .find(|s| s.step_name == step)
        .expect("step row"))
}

// ─── Claim (spec § 6, D10) ───────────────────────────────────────────────────

const PIPELINE: &str = r#"
actions:
  emit: { type: script, script: "true" }
  use:
    type: script
    script: "true"
    input:
      cfg: { type: json }
      count: { type: json }
      label: { type: string }
tasks:
  t:
    flow:
      a: { action: emit }
      b:
        action: use
        depends_on: [a]
        input:
          cfg: "{{ a.output.cfg }}"
          count: "{{ a.output.items | length }}"
          label: "{{ a.output.items | length }}"
"#;

#[tokio::test]
async fn claim_gives_json_fields_native_values() -> Result<()> {
    let app = app(PIPELINE).await?;
    let (s, body) = execute(&app, "t", json!({"input": {}})).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let job = body["job_id"].as_str().unwrap().to_string();

    let (s, a) = claim(&app).await?;
    assert_eq!(
        (s, a["step_name"].as_str()),
        (StatusCode::OK, Some("a")),
        "{a}"
    );
    let out = json!({"cfg": {"region": "eu", "replicas": 3}, "items": [1, 2, 3]});
    assert_eq!(complete(&app, &job, "a", out).await?, StatusCode::OK);

    let (s, b) = claim(&app).await?;
    assert_eq!(
        (s, b["step_name"].as_str()),
        (StatusCode::OK, Some("b")),
        "{b}"
    );
    assert_eq!(
        b["input"]["cfg"],
        json!({"region": "eu", "replicas": 3}),
        "{b}"
    );
    assert_eq!(
        b["input"]["count"],
        json!(3),
        "json field gets a number: {b}"
    );
    assert_eq!(
        b["input"]["label"],
        json!("3"),
        "string field unchanged: {b}"
    );
    Ok(())
}

#[tokio::test]
async fn claim_fails_a_mixed_template_in_a_json_field() -> Result<()> {
    let app = app(&PIPELINE.replace(
        "count: \"{{ a.output.items | length }}\"",
        "count: \"n={{ a.output.items | length }}\"",
    ))
    .await?;
    let (_, body) = execute(&app, "t", json!({"input": {}})).await?;
    let job = body["job_id"].as_str().unwrap().to_string();
    claim(&app).await?;
    complete(&app, &job, "a", json!({"cfg": {}, "items": [1]})).await?;

    let (s, _) = claim(&app).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let b = step_row(&app, &job, "b").await?;
    assert_eq!(b.status, "failed");
    let err = b.error_message.unwrap_or_default();
    assert!(err.contains("Input field 'count'"), "{err}");
    assert!(err.contains("a json field takes a literal value"), "{err}");
    Ok(())
}

const SINGLE: &str = r#"
actions:
  use:
    type: script
    script: "true"
    input:
      n: { type: json }
      extra: { type: string, default: persisted }
tasks:
  t:
    input:
      cfg: { type: json }
    flow:
      b:
        action: use
        input:
          n: "{{ input.cfg.items | length }}"
"#;

#[tokio::test]
async fn claim_uses_the_persisted_schema_after_a_live_retype() -> Result<()> {
    let app = app(SINGLE).await?;
    let (_, body) = execute(&app, "t", json!({"input": {"cfg": {"items": [1, 2, 3]}}})).await?;
    assert!(body["job_id"].is_string(), "{body}");
    // After creation: `n` becomes a string and the default changes.
    app.mgr
        .replace_config_for_test(
            "default",
            workspace(
                &SINGLE
                    .replace("n: { type: json }", "n: { type: string }")
                    .replace("default: persisted", "default: live"),
            ),
        )
        .await;
    let (s, b) = claim(&app).await?;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["input"]["n"], json!(3), "persisted json type wins: {b}");
    assert_eq!(
        b["input"]["extra"],
        json!("persisted"),
        "persisted default wins: {b}"
    );
    Ok(())
}

#[tokio::test]
async fn claim_uses_the_persisted_schema_after_the_action_is_deleted() -> Result<()> {
    let app = app(SINGLE).await?;
    execute(&app, "t", json!({"input": {"cfg": {"items": [1, 2, 3]}}})).await?;
    let mut live = workspace(SINGLE);
    live.actions.remove("use");
    app.mgr.replace_config_for_test("default", live).await;
    let (s, b) = claim(&app).await?;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["input"]["n"], json!(3), "{b}");
    assert_eq!(b["input"]["extra"], json!("persisted"), "{b}");
    Ok(())
}

#[tokio::test]
async fn claim_keeps_a_persisted_string_type_after_a_live_retype_to_json() -> Result<()> {
    let yaml = SINGLE.replace("n: { type: json }", "n: { type: string }");
    let app = app(&yaml).await?;
    execute(&app, "t", json!({"input": {"cfg": {"items": [1, 2, 3]}}})).await?;
    app.mgr
        .replace_config_for_test("default", workspace(SINGLE))
        .await;
    let (s, b) = claim(&app).await?;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["input"]["n"],
        json!("3"),
        "persisted string type wins: {b}"
    );
    Ok(())
}

#[tokio::test]
async fn claim_ignores_a_live_retype_to_a_connection_type() -> Result<()> {
    let app = app(SINGLE).await?;
    execute(&app, "t", json!({"input": {"cfg": {"items": [1, 2, 3]}}})).await?;
    let live = format!(
        "connection_types:\n  pg:\n    host: {{ type: string }}\n{}",
        SINGLE.replace("n: { type: json }", "n: { type: pg }")
    );
    app.mgr
        .replace_config_for_test("default", workspace(&live))
        .await;
    let (s, b) = claim(&app).await?;
    assert_eq!(
        s,
        StatusCode::OK,
        "no connection resolution of a json value: {b}"
    );
    assert_eq!(b["input"]["n"], json!(3), "{b}");
    Ok(())
}

#[tokio::test]
async fn claim_fails_an_unreadable_persisted_schema_through_retry() -> Result<()> {
    let yaml = SINGLE.replace(
        "        action: use\n",
        "        action: use\n        retry: { max_attempts: 2, delay: 1s }\n",
    );
    let app = app(&yaml).await?;
    let (_, body) = execute(&app, "t", json!({"input": {"cfg": {"items": [1]}}})).await?;
    let job = body["job_id"].as_str().unwrap().to_string();
    let job_id: Uuid = job.parse()?;
    sqlx::query(
        "UPDATE job_step SET action_spec = jsonb_set(action_spec, '{input}', '\"garbage-canary\"'::jsonb) \
         WHERE job_id = $1 AND step_name = 'b'",
    )
    .bind(job_id)
    .execute(&app.pool)
    .await?;

    let (s, _) = claim(&app).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let b = step_row(&app, &job, "b").await?;
    assert_eq!(b.status, "ready", "first failure is retried, not released");
    assert_eq!(b.retry_attempt, 1);

    sqlx::query("UPDATE job_step SET retry_at = NOW() - INTERVAL '1 second' WHERE job_id = $1 AND step_name = 'b'")
        .bind(job_id)
        .execute(&app.pool)
        .await?;
    let (s, _) = claim(&app).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let b = step_row(&app, &job, "b").await?;
    assert_eq!(b.status, "failed");
    let err = b.error_message.unwrap_or_default();
    assert!(
        err.contains("the step's persisted action definition has an unreadable input schema"),
        "{err}"
    );
    assert!(!err.contains("garbage-canary"), "{err}");
    Ok(())
}

#[tokio::test]
async fn for_each_instances_get_native_item_and_index() -> Result<()> {
    let app = app(r#"
actions:
  use:
    type: script
    script: "true"
    input:
      item: { type: json }
      idx: { type: json }
tasks:
  t:
    flow:
      each:
        action: use
        for_each: [{ n: 1 }, { n: 2 }]
        input:
          item: "{{ each.item }}"
          idx: "{{ each.index }}"
"#)
    .await?;
    execute(&app, "t", json!({"input": {}})).await?;
    for _ in 0..2 {
        let (s, c) = claim(&app).await?;
        assert_eq!(s, StatusCode::OK, "{c}");
        let idx = c["input"]["idx"].as_u64().expect("index is a number");
        assert_eq!(c["input"]["item"], json!({"n": idx + 1}), "{c}");
    }
    Ok(())
}

/// Spec § 8: an owner-side json error (the owner's own default) is WITHHELD
/// at claim; a caller-side one (the caller's step input) is shown.
#[tokio::test]
async fn cross_workspace_json_errors_follow_the_withholding_rule() -> Result<()> {
    let owner = r#"
secrets:
  S: "owner-secret-canary"
actions:
  bad-default:
    type: script
    script: "true"
    input:
      d: { type: json, default: "x {{ secret.S }}" }
  ok:
    type: script
    script: "true"
    input:
      c: { type: json }
"#;
    let caller = r#"
tasks:
  owner-side:
    flow:
      s: { action: B.bad-default }
  caller-side:
    flow:
      s:
        action: B.ok
        input:
          c: "n {{ 1 }}"
"#;
    let app = app_ws(&[("A", caller), ("B", owner)], None, None).await?;
    for (task, withheld) in [("owner-side", true), ("caller-side", false)] {
        let (s, body) = call(
            &app,
            api(
                "POST",
                &format!("/api/workspaces/A/tasks/{task}/execute"),
                json!({"input": {}}),
                None,
            ),
        )
        .await?;
        assert_eq!(s, StatusCode::OK, "{body}");
        let job = body["job_id"].as_str().unwrap().to_string();
        let (s, _) = claim(&app).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{task}");
        let err = step_row(&app, &job, "s")
            .await?
            .error_message
            .unwrap_or_default();
        assert!(!err.contains("owner-secret-canary"), "{task}: {err}");
        if withheld {
            assert!(err.contains("details withheld"), "{task}: {err}");
            assert!(!err.contains("a json field takes"), "{task}: {err}");
        } else {
            assert!(err.contains("Input field 'c'"), "{task}: {err}");
            assert!(
                err.contains("a json field takes a literal value"),
                "{task}: {err}"
            );
        }
    }
    Ok(())
}
