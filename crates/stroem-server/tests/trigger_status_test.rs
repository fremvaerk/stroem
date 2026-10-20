//! Last run, result and missed runs on the trigger list and the task detail
//! (spec 2026-10-20-trigger-run-status-design.md § 5).

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use chrono::{DateTime, Utc};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{ApiKeyRepo, UserGroupRepo, UserRepo};
use stroem_server::auth::generate_api_key;
use stroem_server::config::{
    AclAction, AclConfig, AclRule, AuthConfig, DbConfig, LogStorageConfig, RetentionConfig,
    ServerConfig, WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::{in_memory_entry, WorkspaceManager};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

const ETL_YAML: &str = r#"
actions:
  noop: { type: script, script: "true" }
tasks:
  nightly: { folder: public, flow: { run: { action: noop } } }
  secret-task: { folder: private, flow: { run: { action: noop } } }
  consume: { flow: { run: { action: noop } } }
  handle: { flow: { run: { action: noop } } }
triggers:
  nightly: { type: scheduler, cron: "0 1 * * *", task: nightly }
  every5: { type: scheduler, cron: "*/5 * * * *", task: nightly, concurrency: skip }
  hook: { type: webhook, name: etl-hook, task: nightly }
  feed: { type: event_source, task: consume, target_task: handle }
  off: { type: scheduler, cron: "0 1 * * *", task: nightly, enabled: false }
  cross: { type: scheduler, cron: "0 1 * * *", task: ops.report }
  sec: { type: scheduler, cron: "0 1 * * *", task: secret-task }
"#;

const OPS_YAML: &str = r#"
actions:
  noop: { type: script, script: "true" }
tasks:
  report: { flow: { run: { action: noop } } }
triggers:
  nightly: { type: scheduler, cron: "0 1 * * *", task: report }
"#;

struct Env {
    router: Router,
    pool: PgPool,
    /// API key of a default admin, used when a request names no key.
    admin: String,
    workspaces: Arc<WorkspaceManager>,
    _tmp: TempDir,
}

async fn setup(acl: bool, etl_triggers_off: bool) -> Result<Env> {
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
            "etl".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                display_name: None,
                description: None,
                path: tmp.path().to_string_lossy().to_string(),
            },
        )]),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: "test-token".to_string(),
        auth: Some(AuthConfig {
            jwt_secret: "test-jwt-secret-key".to_string(),
            refresh_secret: "test-refresh-secret-key".to_string(),
            base_url: None,
            providers: HashMap::new(),
            initial_user: None,
            rate_limit: Default::default(),
        }),
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: acl.then(|| AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "etl".to_string(),
                tasks: vec!["public/*".to_string()],
                action: AclAction::View,
                groups: vec!["viewers".to_string()],
                users: vec![],
            }],
        }),
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

    let etl: WorkspaceConfig = serde_yaml::from_str(ETL_YAML)?;
    let ops: WorkspaceConfig = serde_yaml::from_str(OPS_YAML)?;
    let entries = HashMap::from([
        ("etl".to_string(), in_memory_entry("etl", etl, None)),
        ("ops".to_string(), in_memory_entry("ops", ops, None)),
    ]);
    let mut mgr = WorkspaceManager::from_entries(entries);
    if etl_triggers_off {
        mgr = mgr.with_triggers_disabled("etl");
    }
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None);
    let workspaces = Arc::clone(&state.workspaces);
    let router = build_router(state, CancellationToken::new());
    let admin = api_key(&pool, "default-admin@test.com", true, None).await;
    Ok(Env {
        router,
        pool,
        admin,
        workspaces,
        _tmp: tmp,
    })
}

async fn api_key(pool: &PgPool, email: &str, is_admin: bool, group: Option<&str>) -> String {
    let user_id = Uuid::new_v4();
    UserRepo::create(pool, user_id, email, None, None)
        .await
        .unwrap();
    UserRepo::set_admin(pool, user_id, is_admin).await.unwrap();
    if let Some(g) = group {
        UserGroupRepo::add(pool, user_id, g).await.unwrap();
    }
    let (raw, hash) = generate_api_key();
    ApiKeyRepo::create(pool, &hash, user_id, "test", &raw[..12], None)
        .await
        .unwrap();
    raw
}

struct Seed<'a> {
    ws: &'a str,
    task: &'a str,
    source_type: &'a str,
    source_id: &'a str,
    status: &'a str,
    ago_secs: f64,
    input: Option<Value>,
    output: Option<Value>,
    git_ref: Option<&'a str>,
    folder: Option<&'a str>,
    retry_of: Option<Uuid>,
    attempt: i32,
}

impl Default for Seed<'_> {
    fn default() -> Self {
        Seed {
            ws: "etl",
            task: "nightly",
            source_type: "trigger",
            source_id: "etl/nightly",
            status: "completed",
            ago_secs: 0.0,
            input: None,
            output: None,
            git_ref: None,
            folder: None,
            retry_of: None,
            attempt: 0,
        }
    }
}

async fn seed(pool: &PgPool, s: Seed<'_>) -> Uuid {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type, source_id, \
         input, output, git_ref, task_folder, retry_of_job_id, retry_attempt, created_at) \
         VALUES ($1, $2, $3, 'distributed', $4, $5, $6, $7, $8, $9, $10, $11, $12, \
         NOW() - make_interval(secs => $13))",
    )
    .bind(job_id)
    .bind(s.ws)
    .bind(s.task)
    .bind(s.status)
    .bind(s.source_type)
    .bind(s.source_id)
    .bind(s.input)
    .bind(s.output)
    .bind(s.git_ref)
    .bind(s.folder)
    .bind(s.retry_of)
    .bind(s.attempt)
    .bind(s.ago_secs)
    .execute(pool)
    .await
    .unwrap();
    job_id
}

const HOUR: f64 = 3600.0;
const DAY: f64 = 86400.0;

async fn get(env: &Env, uri: &str, key: Option<&str>) -> (StatusCode, String) {
    let k = key.unwrap_or(&env.admin);
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {k}"));
    let resp = env
        .router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn list(env: &Env, ws: &str, key: Option<&str>) -> Vec<Value> {
    let (status, body) = get(env, &format!("/api/workspaces/{ws}/triggers"), key).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn find<'a>(list: &'a [Value], name: &str) -> &'a Value {
    list.iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("trigger {name} not listed in {list:?}"))
}

fn ts(v: &Value) -> DateTime<Utc> {
    v.as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn trigger_list_shows_last_run_result_and_missed_mark() -> Result<()> {
    let env = setup(false, false).await?;
    let run = seed(
        &env.pool,
        Seed {
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    let t = find(&l, "nightly");
    assert_eq!(t["last_run"]["status"], "completed");
    assert_eq!(t["last_run"]["job_id"], run.to_string());
    assert_eq!(t["last_run_hidden"], false);
    assert!(t["last_run"].get("retry_job_id").is_none());
    assert!(t["missed"]["count"].as_u64().unwrap() >= 1);
    assert_eq!(t["missed"]["capped"], false);
    let first = t["missed"]["first"].as_str().unwrap();
    assert!(
        first.contains("T01:00:00") && first.ends_with('Z'),
        "{first}"
    );
    assert!(ts(&t["missed"]["first"]) > ts(&t["last_run"]["created_at"]));
    Ok(())
}

#[tokio::test]
async fn trigger_never_run_has_no_mark() -> Result<()> {
    let env = setup(false, false).await?;
    let l = list(&env, "etl", None).await;
    let t = find(&l, "nightly");
    assert!(t["last_run"].is_null());
    assert!(t["missed"].is_null());
    assert_eq!(t["last_run_hidden"], false);
    Ok(())
}

#[tokio::test]
async fn run_after_the_gap_resets_the_mark() -> Result<()> {
    let env = setup(false, false).await?;
    seed(
        &env.pool,
        Seed {
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    seed(
        &env.pool,
        Seed {
            ago_secs: 60.0,
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    assert!(find(&l, "nightly")["missed"].is_null());
    Ok(())
}

#[tokio::test]
async fn manual_rerun_and_restart_do_not_count() -> Result<()> {
    let env = setup(false, false).await?;
    let run = seed(
        &env.pool,
        Seed {
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    for (i, st) in ["user", "rerun", "restart"].into_iter().enumerate() {
        seed(
            &env.pool,
            Seed {
                source_type: st,
                source_id: "someone",
                ago_secs: DAY - i as f64 * HOUR,
                ..Default::default()
            },
        )
        .await;
    }
    let l = list(&env, "etl", None).await;
    let t = find(&l, "nightly");
    assert_eq!(t["last_run"]["job_id"], run.to_string());
    assert!(t["missed"]["count"].as_u64().unwrap() >= 1);
    Ok(())
}

#[tokio::test]
async fn task_retry_gives_the_result() -> Result<()> {
    let env = setup(false, false).await?;
    let run = seed(
        &env.pool,
        Seed {
            status: "failed",
            ago_secs: 3.0 * HOUR,
            ..Default::default()
        },
    )
    .await;
    let retry = seed(
        &env.pool,
        Seed {
            source_type: "retry",
            source_id: "retry",
            status: "completed",
            ago_secs: 2.0 * HOUR,
            retry_of: Some(run),
            attempt: 1,
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    let t = find(&l, "nightly");
    assert_eq!(t["last_run"]["status"], "completed");
    assert_eq!(t["last_run"]["retry_job_id"], retry.to_string());
    assert_eq!(t["last_run"]["job_id"], run.to_string());
    Ok(())
}

#[tokio::test]
async fn skipped_rows_count_as_fires_not_as_the_run() -> Result<()> {
    let env = setup(false, false).await?;
    let run = seed(
        &env.pool,
        Seed {
            source_id: "etl/every5",
            status: "running",
            ago_secs: HOUR,
            ..Default::default()
        },
    )
    .await;
    for i in 0..12 {
        seed(
            &env.pool,
            Seed {
                source_id: "etl/every5",
                status: "skipped",
                ago_secs: 55.0 * 60.0 - i as f64 * 300.0,
                ..Default::default()
            },
        )
        .await;
    }
    let l = list(&env, "etl", None).await;
    let t = find(&l, "every5");
    assert_eq!(t["last_run"]["status"], "running");
    assert_eq!(t["last_run"]["job_id"], run.to_string());
    assert!(t["missed"].is_null());
    Ok(())
}

#[tokio::test]
async fn skipped_only_trigger_shows_the_skipped_row() -> Result<()> {
    let env = setup(false, false).await?;
    seed(
        &env.pool,
        Seed {
            source_id: "etl/every5",
            status: "skipped",
            ago_secs: 2.0 * HOUR,
            ..Default::default()
        },
    )
    .await;
    let newest = seed(
        &env.pool,
        Seed {
            source_id: "etl/every5",
            status: "skipped",
            ago_secs: 1.0 * HOUR,
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    let t = find(&l, "every5");
    assert_eq!(t["last_run"]["status"], "skipped");
    assert_eq!(t["last_run"]["job_id"], newest.to_string());
    assert!(t["last_run"].get("retry_job_id").is_none());
    Ok(())
}

#[tokio::test]
async fn webhook_and_event_source_never_get_a_mark() -> Result<()> {
    let env = setup(false, false).await?;
    seed(
        &env.pool,
        Seed {
            source_type: "webhook",
            source_id: "etl/hook",
            ago_secs: 5.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    let consumer = seed(
        &env.pool,
        Seed {
            task: "consume",
            source_type: "event_source",
            source_id: "etl/feed",
            status: "running",
            ago_secs: 5.0 * DAY,
            input: Some(json!({"_fingerprint": "f"})),
            ..Default::default()
        },
    )
    .await;
    seed(
        &env.pool,
        Seed {
            task: "handle",
            source_type: "event_source",
            source_id: "etl/feed",
            ago_secs: HOUR,
            input: Some(json!({"x": 1})),
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    let hook = find(&l, "hook");
    assert!(hook["missed"].is_null());
    assert_eq!(hook["last_run"]["status"], "completed");
    let feed = find(&l, "feed");
    assert!(feed["missed"].is_null());
    assert_eq!(feed["last_run"]["job_id"], consumer.to_string());
    Ok(())
}

#[tokio::test]
async fn disabled_trigger_and_triggers_false_workspace_get_no_mark() -> Result<()> {
    let env = setup(false, false).await?;
    seed(
        &env.pool,
        Seed {
            source_id: "etl/off",
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    let off = find(&l, "off");
    assert!(off["missed"].is_null());
    assert_eq!(off["last_run"]["status"], "completed");

    let env = setup(false, true).await?;
    seed(
        &env.pool,
        Seed {
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    let t = find(&l, "nightly");
    assert!(t["missed"].is_null());
    assert_eq!(t["last_run"]["status"], "completed");
    Ok(())
}

#[tokio::test]
async fn cross_workspace_target_matched_by_source_id() -> Result<()> {
    let env = setup(false, false).await?;
    let run = seed(
        &env.pool,
        Seed {
            ws: "ops",
            task: "report",
            source_id: "etl/cross",
            ago_secs: HOUR,
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    assert_eq!(find(&l, "cross")["last_run"]["job_id"], run.to_string());
    Ok(())
}

#[tokio::test]
async fn same_trigger_name_in_two_workspaces_stays_apart() -> Result<()> {
    let env = setup(false, false).await?;
    seed(
        &env.pool,
        Seed {
            ago_secs: HOUR,
            ..Default::default()
        },
    )
    .await;
    seed(
        &env.pool,
        Seed {
            ws: "ops",
            task: "report",
            source_id: "ops/nightly",
            status: "failed",
            ago_secs: HOUR,
            ..Default::default()
        },
    )
    .await;
    let etl = list(&env, "etl", None).await;
    let ops = list(&env, "ops", None).await;
    assert_eq!(find(&etl, "nightly")["last_run"]["status"], "completed");
    assert_eq!(find(&ops, "nightly")["last_run"]["status"], "failed");
    Ok(())
}

#[tokio::test]
async fn task_detail_triggers_carry_the_same_fields() -> Result<()> {
    let env = setup(false, false).await?;
    seed(
        &env.pool,
        Seed {
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    seed(
        &env.pool,
        Seed {
            source_type: "webhook",
            source_id: "etl/hook",
            status: "failed",
            ago_secs: HOUR,
            ..Default::default()
        },
    )
    .await;
    let l = list(&env, "etl", None).await;
    let (status, body) = get(&env, "/api/workspaces/etl/tasks/nightly", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail: Value = serde_json::from_str(&body)?;
    let triggers = detail["triggers"].as_array().unwrap();
    for name in ["nightly", "hook"] {
        assert_eq!(find(triggers, name), find(&l, name), "{name}");
    }
    assert!(
        find(triggers, "nightly")["missed"]["count"]
            .as_u64()
            .unwrap()
            >= 1
    );
    Ok(())
}

#[tokio::test]
async fn acl_hides_denied_trigger_and_withholds_a_denied_jobs_run() -> Result<()> {
    let env = setup(true, false).await?;
    let viewer = api_key(&env.pool, "viewer@test.com", false, Some("viewers")).await;
    let admin = api_key(&env.pool, "admin@test.com", true, None).await;
    let run = seed(
        &env.pool,
        Seed {
            git_ref: Some("main"),
            folder: Some("private"),
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;

    let l = list(&env, "etl", Some(&viewer)).await;
    let t = find(&l, "nightly");
    assert!(t["last_run"].is_null());
    assert_eq!(t["last_run_hidden"], true);
    assert!(t["missed"]["count"].as_u64().unwrap() >= 1);
    assert!(l.iter().all(|t| t["name"] != "sec"), "{l:?}");

    let l = list(&env, "etl", Some(&admin)).await;
    let t = find(&l, "nightly");
    assert_eq!(t["last_run"]["job_id"], run.to_string());
    assert_eq!(t["last_run_hidden"], false);
    assert!(l.iter().any(|t| t["name"] == "sec"));
    Ok(())
}

#[tokio::test]
async fn responses_never_carry_job_input_or_output() -> Result<()> {
    let env = setup(false, false).await?;
    seed(
        &env.pool,
        Seed {
            ago_secs: 2.0 * DAY,
            input: Some(json!({"token": "s3cr3t"})),
            output: Some(json!({"leak": "s3cr3t"})),
            ..Default::default()
        },
    )
    .await;
    let (status, body) = get(&env, "/api/workspaces/etl/triggers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("s3cr3t"), "{body}");
    let (status, body) = get(&env, "/api/workspaces/etl/tasks/nightly", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("s3cr3t"), "{body}");
    Ok(())
}

async fn attention(env: &Env, key: Option<&str>) -> Vec<Value> {
    let (status, body) = get(env, "/api/triggers/attention", key).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn row_names(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| {
            format!(
                "{}/{}",
                r["workspace"].as_str().unwrap(),
                r["name"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test]
async fn attention_lists_missed_and_failed_only() -> Result<()> {
    let env = setup(false, false).await?;
    seed(
        &env.pool,
        Seed {
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    seed(
        &env.pool,
        Seed {
            source_id: "etl/every5",
            status: "failed",
            ago_secs: 100.0,
            ..Default::default()
        },
    )
    .await;
    seed(
        &env.pool,
        Seed {
            source_id: "etl/cross",
            ago_secs: 100.0,
            ..Default::default()
        },
    )
    .await;
    seed(
        &env.pool,
        Seed {
            ws: "ops",
            source_id: "ops/nightly",
            status: "cancelled",
            ago_secs: 100.0,
            ..Default::default()
        },
    )
    .await;
    let rows = attention(&env, None).await;
    assert_eq!(row_names(&rows), ["etl/nightly", "etl/every5"], "{rows:?}");
    assert_eq!(rows[0]["last_run_failed"], false);
    assert!(rows[0]["missed"]["count"].as_u64().unwrap() >= 1);
    assert_eq!(rows[1]["last_run_failed"], true);
    assert!(rows[1]["missed"].is_null());
    assert_eq!(rows[1]["last_run"]["status"], "failed");
    Ok(())
}

#[tokio::test]
async fn attention_skips_a_workspace_in_load_error() -> Result<()> {
    let env = setup(false, false).await?;
    for (ws, id, ago) in [("etl", "etl/every5", 100.0), ("ops", "ops/nightly", 200.0)] {
        seed(
            &env.pool,
            Seed {
                ws,
                source_id: id,
                status: "failed",
                ago_secs: ago,
                ..Default::default()
            },
        )
        .await;
    }
    assert_eq!(
        row_names(&attention(&env, None).await),
        ["etl/every5", "ops/nightly"]
    );
    env.workspaces.mark_unavailable_for_test("ops");
    assert_eq!(row_names(&attention(&env, None).await), ["etl/every5"]);
    Ok(())
}

#[tokio::test]
async fn attention_respects_acl() -> Result<()> {
    let env = setup(true, false).await?;
    let viewer = api_key(&env.pool, "viewer@test.com", false, Some("viewers")).await;
    let admin = api_key(&env.pool, "admin@test.com", true, None).await;
    // Pinned job in a private folder: the viewer may not see this failed run,
    // but the trigger is also missed.
    seed(
        &env.pool,
        Seed {
            status: "failed",
            git_ref: Some("main"),
            folder: Some("private"),
            ago_secs: 2.0 * DAY,
            ..Default::default()
        },
    )
    .await;
    // Failed run of a trigger whose task is denied to the viewer.
    seed(
        &env.pool,
        Seed {
            task: "secret-task",
            source_id: "etl/sec",
            status: "failed",
            ago_secs: 100.0,
            ..Default::default()
        },
    )
    .await;
    // Failed run the viewer may see.
    seed(
        &env.pool,
        Seed {
            source_id: "etl/every5",
            status: "failed",
            ago_secs: 100.0,
            ..Default::default()
        },
    )
    .await;

    let rows = attention(&env, Some(&viewer)).await;
    assert_eq!(row_names(&rows), ["etl/nightly", "etl/every5"], "{rows:?}");
    assert!(rows[0]["last_run"].is_null());
    assert_eq!(rows[0]["last_run_hidden"], true);
    assert_eq!(rows[0]["last_run_failed"], false);
    assert_eq!(rows[1]["last_run_failed"], true);

    let rows = attention(&env, Some(&admin)).await;
    let names = row_names(&rows);
    assert!(names.contains(&"etl/sec".to_string()), "{names:?}");
    let nightly = rows.iter().find(|r| r["name"] == "nightly").unwrap();
    assert_eq!(nightly["last_run_failed"], true);
    Ok(())
}
