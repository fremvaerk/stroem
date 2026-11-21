//! MCP job actions: `restart_job`, `rerun_job` and `approve_step`.
//!
//! Each tool calls the function extracted from its REST handler
//! (`web/api/restart.rs`, `web/api/rerun.rs`, `web/api/approve.rs`), so the
//! REST suites cover the shared check order. These tests cover what the MCP
//! side adds: the parameters, the result shape, the error codes and that the
//! caller's identity and ACL reach the shared function.

use crate::common::pinned::{mcp_call, mcp_is_error, mcp_tool_json};

use anyhow::Result;
use axum::Router;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::{InputFieldDef, WorkspaceConfig};
use stroem_db::{ApiKeyRepo, JobRepo, JobStepRepo, UserRepo};
use stroem_server::auth::generate_api_key;
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    AclAction, AclConfig, AclRule, AuthConfig, DbConfig, JobDefaults, LogStorageConfig, McpConfig,
    RetentionConfig, ServerConfig, WorkspaceSourceDef,
};
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::web::build_router;
use stroem_server::workspace::WorkspaceManager;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The one user of every test app. ACL: View on `view/*`, Deny on `deny/*`,
/// Run everywhere else.
const USER: &str = "actions@test.local";

/// JSON-RPC "invalid params".
const INVALID_PARAMS: i64 = -32602;

const ACTIONS_YAML: &str = r#"
actions:
  noop: { type: script, script: "true" }
  gate:
    type: approval
    message: "Ship it?"
    input: { reason: { type: string, required: true } }
tasks:
  gated:
    flow:
      ask: { action: gate }
      after: { action: noop, depends_on: [ask] }
  line:
    input: { note: { type: string } }
    flow:
      a: { action: noop }
      b: { action: noop, depends_on: [a] }
      c: { action: noop, depends_on: [b] }
  fan:
    flow:
      a: { action: noop }
      b: { action: noop, depends_on: [a] }
      c: { action: noop, depends_on: [a] }
  secretive:
    input:
      token: { type: string, secret: true }
      note: { type: string }
    flow: { a: { action: noop } }
  with-file:
    input:
      data_file: { type: file, required: true }
      note: { type: string }
    flow: { a: { action: noop } }
  view-line:
    folder: view
    flow: { a: { action: noop } }
  deny-line:
    folder: deny
    flow: { a: { action: noop } }
"#;

fn actions_workspace() -> WorkspaceConfig {
    serde_yaml::from_str(ACTIONS_YAML).expect("ACTIONS_YAML parses")
}

fn actions_acl() -> AclConfig {
    let rule = |tasks: &str, action: AclAction| AclRule {
        workspace: "default".to_string(),
        tasks: vec![tasks.to_string()],
        action,
        groups: vec![],
        users: vec![USER.to_string()],
    };
    AclConfig {
        default: AclAction::Run,
        rules: vec![
            rule("view/*", AclAction::View),
            rule("deny/*", AclAction::Deny),
        ],
    }
}

/// An auth-enabled server with MCP and ACL over `ws` (workspace `default`),
/// and an API key of [`USER`].
struct App {
    router: Router,
    pool: PgPool,
    key: String,
    _tmp: TempDir,
}

impl App {
    async fn new(ws: WorkspaceConfig) -> Result<Self> {
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
            worker_token: "actions-test-token-secret-long-enough".to_string(),
            auth: Some(AuthConfig {
                jwt_secret: "actions-test-jwt-secret-long-enough".to_string(),
                refresh_secret: "actions-test-refresh-secret-long-enough".to_string(),
                base_url: None,
                providers: HashMap::new(),
                initial_user: None,
                rate_limit: Default::default(),
            }),
            recovery: Default::default(),
            retention: RetentionConfig::default(),
            acl: Some(actions_acl()),
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

        let user_id = Uuid::new_v4();
        UserRepo::create(&pool, user_id, USER, None, None).await?;
        let (key, hash) = generate_api_key();
        ApiKeyRepo::create(&pool, &hash, user_id, "actions", &key[..12], None).await?;

        let mgr = WorkspaceManager::from_config("default", ws);
        let log_storage = LogStorage::new(&config.log_storage.local_dir);
        let archive: Arc<dyn BlobArchive> =
            Arc::new(LocalBlobArchive::new(tmp.path().join("input-archive")));
        let state = AppState::new(pool.clone(), mgr, config, log_storage, HashMap::new(), None)
            .with_input_blob(Some(archive));
        let router = build_router(state, CancellationToken::new());
        Ok(Self {
            router,
            pool,
            key,
            _tmp: tmp,
        })
    }

    async fn call(&self, tool: &str, args: Value) -> Value {
        mcp_call(&self.router, Some(&self.key), tool, args).await
    }

    /// The tool's JSON result; panics on an error answer.
    async fn ok(&self, tool: &str, args: Value) -> Value {
        let resp = self.call(tool, args.clone()).await;
        assert!(!mcp_is_error(&resp), "{tool} {args}: {resp}");
        mcp_tool_json(&resp)
    }

    /// The JSON-RPC error `(code, message)` of a refused call.
    async fn err(&self, tool: &str, args: Value) -> (i64, String) {
        let resp = self.call(tool, args.clone()).await;
        let code = resp["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("{tool} {args}: expected a JSON-RPC error: {resp}"));
        let message = resp["error"]["message"].as_str().unwrap_or_default();
        (code, message.to_string())
    }

    async fn count(&self, sql: &'static str) -> Result<i64> {
        Ok(sqlx::query_scalar(sql).fetch_one(&self.pool).await?)
    }
}

/// A top-level job of `task` (pool-only creation from `ws`, source type
/// `api`, no init) that a worker has finished: each `(step, status)` row is
/// set, then the job row.
async fn finished_job(
    pool: &PgPool,
    ws: &WorkspaceConfig,
    task: &str,
    input: Value,
    steps: &[(&str, &str)],
    job_status: &str,
) -> Result<Uuid> {
    let mgr = WorkspaceManager::from_config("default", ws.clone());
    let job_id = stroem_server::job_creator::create_job_for_task_detailed(
        &mgr,
        pool,
        ws,
        "default",
        task,
        input,
        "api",
        None,
        None,
        None,
        None,
        JobDefaults::default(),
        stroem_server::input_files::FileGrant::Indirect,
    )
    .await?
    .job_id;
    for (step, status) in steps {
        let error = (*status == "failed").then_some("boom");
        sqlx::query(
            "UPDATE job_step SET status = $3, error_message = $4, completed_at = NOW() \
             WHERE job_id = $1 AND step_name = $2",
        )
        .bind(job_id)
        .bind(step)
        .bind(status)
        .bind(error)
        .execute(pool)
        .await?;
    }
    sqlx::query("UPDATE job SET status = $2, completed_at = NOW() WHERE job_id = $1")
        .bind(job_id)
        .bind(job_status)
        .execute(pool)
        .await?;
    Ok(job_id)
}

/// `line` failed at `b`: `a` completed, `c` never ran.
async fn failed_line_job(pool: &PgPool) -> Result<Uuid> {
    finished_job(
        pool,
        &actions_workspace(),
        "line",
        json!({"note": "n1"}),
        &[("a", "completed"), ("b", "failed"), ("c", "skipped")],
        "failed",
    )
    .await
}

/// `fan` failed at both branches: `a` completed, `b` and `c` failed.
async fn failed_fan_job(pool: &PgPool) -> Result<Uuid> {
    finished_job(
        pool,
        &actions_workspace(),
        "fan",
        json!({}),
        &[("a", "completed"), ("b", "failed"), ("c", "failed")],
        "failed",
    )
    .await
}

async fn jobs_derived_from(pool: &PgPool, source: Uuid) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE source_job_id = $1")
            .bind(source)
            .fetch_one(pool)
            .await?,
    )
}

// ─── restart_job ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn restart_job_all_failed_creates_a_restart_job() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let source = failed_fan_job(&app.pool).await?;

    let body = app
        .ok(
            "restart_job",
            json!({"job_id": source.to_string(), "all_failed": true}),
        )
        .await;
    assert_eq!(body["start_steps"], json!(["b", "c"]), "{body}");
    assert_eq!(body["restart_steps"], json!(["b", "c"]), "{body}");
    assert_eq!(body["carried_over"], json!(["a"]), "{body}");
    assert_eq!(body["carried_failed"], json!([]), "{body}");
    assert_eq!(body["carried_failed_tolerated"], json!([]), "{body}");
    let new_id: Uuid = body["job_id"].as_str().expect("job_id").parse()?;

    let job = JobRepo::get(&app.pool, new_id).await?.expect("new job row");
    assert_eq!(job.source_type, "restart");
    assert_eq!(job.source_job_id, Some(source));
    assert_eq!(job.source_id.as_deref(), Some(USER));
    assert_eq!(job.restart_from_step, None);
    assert_eq!(
        job.restart_from_steps,
        Some(vec!["b".to_string(), "c".to_string()])
    );
    let steps = JobStepRepo::get_steps_for_job(&app.pool, new_id).await?;
    let carried = |name: &str| {
        steps
            .iter()
            .find(|s| s.step_name == name)
            .unwrap_or_else(|| panic!("row {name}"))
            .carried_over
    };
    assert!(carried("a"), "a is carried from the source");
    assert!(!carried("b") && !carried("c"), "b and c rerun");
    Ok(())
}

#[tokio::test]
async fn restart_job_from_step() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let source = failed_line_job(&app.pool).await?;

    let body = app
        .ok(
            "restart_job",
            json!({"job_id": source.to_string(), "from_step": "b"}),
        )
        .await;
    assert_eq!(body["start_steps"], json!(["b"]), "{body}");
    assert_eq!(body["restart_steps"], json!(["b", "c"]), "{body}");
    assert_eq!(body["carried_over"], json!(["a"]), "{body}");
    let new_id: Uuid = body["job_id"].as_str().expect("job_id").parse()?;
    let job = JobRepo::get(&app.pool, new_id).await?.expect("new job row");
    assert_eq!(job.source_type, "restart");
    assert_eq!(job.source_job_id, Some(source));
    assert_eq!(job.restart_from_step.as_deref(), Some("b"));
    assert_eq!(job.restart_from_steps, None);
    Ok(())
}

#[tokio::test]
async fn restart_job_rejects_an_unknown_step_and_a_loop_instance() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let source = failed_line_job(&app.pool).await?;

    for (step, text) in [
        (
            "nope",
            "Step 'nope' is not in the current flow of task 'line'",
        ),
        ("b[0]", "Restart from the loop step 'b', not an instance"),
    ] {
        let (code, message) = app
            .err(
                "restart_job",
                json!({"job_id": source.to_string(), "from_step": step}),
            )
            .await;
        assert_eq!(code, INVALID_PARAMS, "{step}: {message}");
        assert_eq!(message, text, "{step}");
    }
    assert_eq!(jobs_derived_from(&app.pool, source).await?, 0);
    Ok(())
}

#[tokio::test]
async fn restart_job_dry_run_creates_nothing() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let source = failed_fan_job(&app.pool).await?;

    // From `b`, the failed sibling `c` is carried over as it ended.
    for (args, carried, carried_failed) in [
        (
            json!({"job_id": source.to_string(), "all_failed": true, "dry_run": true}),
            json!(["a"]),
            json!([]),
        ),
        (
            json!({"job_id": source.to_string(), "from_step": "b", "dry_run": true}),
            json!(["a", "c"]),
            json!(["c"]),
        ),
    ] {
        let body = app.ok("restart_job", args.clone()).await;
        assert!(body.get("job_id").is_none(), "{args}: {body}");
        assert_eq!(body["carried_over"], carried, "{args}: {body}");
        assert_eq!(body["carried_failed"], carried_failed, "{args}: {body}");
    }
    assert_eq!(
        app.count("SELECT COUNT(*) FROM job").await?,
        1,
        "a dry run creates no job"
    );
    Ok(())
}

#[tokio::test]
async fn restart_job_needs_exactly_one_mode() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let source = failed_line_job(&app.pool).await?;
    let id = source.to_string();

    for args in [
        json!({"job_id": id, "from_step": "b", "all_failed": true}),
        json!({"job_id": id}),
        json!({"job_id": id, "dry_run": true}),
        json!({"job_id": id, "all_failed": false}),
        // The shape is checked before the job lookup, like the REST route.
        json!({"job_id": Uuid::new_v4().to_string()}),
    ] {
        let (code, message) = app.err("restart_job", args.clone()).await;
        assert_eq!(code, INVALID_PARAMS, "{args}: {message}");
        assert_eq!(
            message, "Specify exactly one of from_step or all_failed",
            "{args}"
        );
    }
    let (code, _) = app
        .err(
            "restart_job",
            json!({"job_id": "not-a-uuid", "all_failed": true}),
        )
        .await;
    assert_eq!(code, INVALID_PARAMS);
    let (_, message) = app
        .err(
            "restart_job",
            json!({"job_id": Uuid::new_v4().to_string(), "all_failed": true}),
        )
        .await;
    assert_eq!(message, "Job not found");
    assert_eq!(jobs_derived_from(&app.pool, source).await?, 0);
    Ok(())
}

#[tokio::test]
async fn restart_job_rejects_sources_it_cannot_restart() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let ws = actions_workspace();
    let restart = |job: Uuid, mode: Value| {
        let mut args = mode;
        args["job_id"] = json!(job.to_string());
        app.err("restart_job", args)
    };

    // Not terminal.
    let running = finished_job(&app.pool, &ws, "line", json!({}), &[], "running").await?;
    let (code, message) = restart(running, json!({"from_step": "a"})).await;
    assert_eq!(code, INVALID_PARAMS, "{message}");
    assert_eq!(message, "Job is not in a terminal state");

    // A `type: task` child and a hook job are not top-level.
    let parent = failed_line_job(&app.pool).await?;
    let child = failed_line_job(&app.pool).await?;
    sqlx::query("UPDATE job SET parent_job_id = $2 WHERE job_id = $1")
        .bind(child)
        .bind(parent)
        .execute(&app.pool)
        .await?;
    let hook = failed_line_job(&app.pool).await?;
    sqlx::query("UPDATE job SET source_type = 'hook' WHERE job_id = $1")
        .bind(hook)
        .execute(&app.pool)
        .await?;
    for job in [child, hook] {
        let (code, message) = restart(job, json!({"all_failed": true})).await;
        assert_eq!(code, INVALID_PARAMS, "{message}");
        assert_eq!(message, "Only top-level jobs can be restarted");
    }

    // Legacy source without raw_input.
    let legacy = failed_line_job(&app.pool).await?;
    sqlx::query("UPDATE job SET raw_input = NULL WHERE job_id = $1")
        .bind(legacy)
        .execute(&app.pool)
        .await?;
    let (_, message) = restart(legacy, json!({"from_step": "b"})).await;
    assert_eq!(message, "Source job predates Re-run prefill (no raw_input)");

    // Nothing failed or cancelled.
    let ok = finished_job(
        &app.pool,
        &ws,
        "line",
        json!({"note": "ok"}),
        &[("a", "completed"), ("b", "completed"), ("c", "completed")],
        "completed",
    )
    .await?;
    for mode in [
        json!({"all_failed": true}),
        json!({"all_failed": true, "dry_run": true}),
    ] {
        let (code, message) = restart(ok, mode.clone()).await;
        assert_eq!(code, INVALID_PARAMS, "{mode}: {message}");
        assert_eq!(
            message,
            "No failed or cancelled steps to restart in the current flow of task 'line'"
        );
    }

    assert_eq!(
        app.count("SELECT COUNT(*) FROM job WHERE source_type = 'restart'")
            .await?,
        0,
        "a refused restart creates nothing"
    );
    Ok(())
}

#[tokio::test]
async fn restart_job_follows_the_job_acl() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let ws = actions_workspace();
    let viewed = finished_job(
        &app.pool,
        &ws,
        "view-line",
        json!({}),
        &[("a", "failed")],
        "failed",
    )
    .await?;
    let denied = finished_job(
        &app.pool,
        &ws,
        "deny-line",
        json!({}),
        &[("a", "failed")],
        "failed",
    )
    .await?;

    for mode in [
        json!({"all_failed": true}),
        json!({"all_failed": true, "dry_run": true}),
    ] {
        let mut args = mode.clone();
        args["job_id"] = json!(viewed.to_string());
        let (_, message) = app.err("restart_job", args).await;
        assert_eq!(
            message, "Insufficient permissions to restart this job",
            "View, {mode}"
        );
        let mut args = mode.clone();
        args["job_id"] = json!(denied.to_string());
        let (_, message) = app.err("restart_job", args).await;
        assert_eq!(message, "Job not found", "Deny, {mode}");
    }
    assert_eq!(
        app.count("SELECT COUNT(*) FROM job WHERE source_type = 'restart'")
            .await?,
        0
    );
    Ok(())
}

/// Restart replays `raw_input` with no form in front of it: a required field
/// added to the task since the source ran (no default) refuses the restart
/// (CLAUDE.md § Job Lineage), as invalid params, not an internal error.
#[tokio::test]
async fn restart_job_rejects_a_newly_required_input() -> Result<()> {
    let mut drifted = actions_workspace();
    let extra: InputFieldDef = serde_yaml::from_str("{type: string, required: true}")?;
    drifted
        .tasks
        .get_mut("line")
        .unwrap()
        .input
        .insert("extra".to_string(), extra);
    let app = App::new(drifted).await?;
    // The source ran under the old task, without `extra`.
    let source = failed_line_job(&app.pool).await?;

    let (code, message) = app
        .err(
            "restart_job",
            json!({"job_id": source.to_string(), "from_step": "b"}),
        )
        .await;
    assert_eq!(code, INVALID_PARAMS, "{message}");
    assert!(
        message.contains("required") && message.contains("extra"),
        "{message}"
    );
    assert_eq!(jobs_derived_from(&app.pool, source).await?, 0);

    // Planning is pure: the dry run still answers.
    let body = app
        .ok(
            "restart_job",
            json!({"job_id": source.to_string(), "from_step": "b", "dry_run": true}),
        )
        .await;
    assert_eq!(body["restart_steps"], json!(["b", "c"]));
    Ok(())
}

// ─── approve_step ────────────────────────────────────────────────────────────

/// A `gated` job created over MCP, so creation-time init runs and its root
/// approval step `ask` is suspended.
async fn suspended_gate(app: &App) -> Result<Uuid> {
    let body = app
        .ok(
            "execute_task",
            json!({"workspace": "default", "task_name": "gated"}),
        )
        .await;
    let job_id: Uuid = body["job_id"].as_str().expect("job_id").parse()?;
    let ask = step(&app.pool, job_id, "ask").await?;
    assert_eq!(ask.status, "suspended", "the gate waits for a decision");
    Ok(job_id)
}

async fn step(pool: &PgPool, job_id: Uuid, name: &str) -> Result<stroem_db::JobStepRow> {
    Ok(JobStepRepo::get_steps_for_job(pool, job_id)
        .await?
        .into_iter()
        .find(|s| s.step_name == name)
        .unwrap_or_else(|| panic!("row {name}")))
}

#[tokio::test]
async fn approve_step_approves_a_suspended_gate() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let job = suspended_gate(&app).await?;

    let body = app
        .ok(
            "approve_step",
            json!({"job_id": job.to_string(), "step_name": "ask", "approved": true,
                   "input": {"reason": "tested"}}),
        )
        .await;
    assert_eq!(body, json!({"status": "approved"}));

    let ask = step(&app.pool, job, "ask").await?;
    assert_eq!(ask.status, "completed");
    let output = ask.output.expect("approval output");
    assert_eq!(output["approved"], json!(true), "{output}");
    assert_eq!(output["approved_by"], json!(USER), "{output}");
    assert_eq!(output["input"], json!({"reason": "tested"}), "{output}");
    assert_eq!(step(&app.pool, job, "after").await?.status, "ready");
    Ok(())
}

/// Like REST, an absent `input` is not `{}`: the required-field check does
/// not run and the output gets no `input` key.
#[tokio::test]
async fn approve_step_without_input_adds_no_input() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let job = suspended_gate(&app).await?;

    let body = app
        .ok(
            "approve_step",
            json!({"job_id": job.to_string(), "step_name": "ask", "approved": true}),
        )
        .await;
    assert_eq!(body, json!({"status": "approved"}));
    let output = step(&app.pool, job, "ask").await?.output.expect("output");
    assert!(output.get("input").is_none(), "{output}");
    Ok(())
}

#[tokio::test]
async fn approve_step_rejects_with_a_reason() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let job = suspended_gate(&app).await?;

    let body = app
        .ok(
            "approve_step",
            json!({"job_id": job.to_string(), "step_name": "ask", "approved": false,
                   "rejection_reason": "not today"}),
        )
        .await;
    assert_eq!(body, json!({"status": "rejected"}));
    let ask = step(&app.pool, job, "ask").await?;
    assert_eq!(ask.status, "failed");
    assert_eq!(ask.error_message.as_deref(), Some("not today"));
    let row = JobRepo::get(&app.pool, job).await?.expect("job row");
    assert_eq!(row.status, "failed");
    Ok(())
}

#[tokio::test]
async fn approve_step_refuses_a_step_it_cannot_decide() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let job = suspended_gate(&app).await?;
    let decide = |step_name: &str, extra: Value| {
        let mut args = json!({"job_id": job.to_string(), "step_name": step_name, "approved": true});
        if let (Some(a), Some(e)) = (args.as_object_mut(), extra.as_object()) {
            a.extend(e.clone());
        }
        app.err("approve_step", args)
    };

    let (_, message) = decide("nope", json!({})).await;
    assert_eq!(message, "Step not found");
    let (code, message) = decide("after", json!({})).await;
    assert_eq!(code, INVALID_PARAMS, "{message}");
    assert_eq!(
        message,
        "Step 'after' is not in suspended state (current: pending)"
    );
    let (code, message) = decide("ask", json!({"input": {}})).await;
    assert_eq!(code, INVALID_PARAMS, "{message}");
    assert_eq!(message, "Required field 'reason' is missing");

    app.ok(
        "approve_step",
        json!({"job_id": job.to_string(), "step_name": "ask", "approved": true}),
    )
    .await;
    let (code, message) = decide("ask", json!({})).await;
    assert_eq!(code, INVALID_PARAMS, "{message}");
    assert_eq!(
        message,
        "Step 'ask' is not in suspended state (current: completed)"
    );
    Ok(())
}

#[tokio::test]
async fn approve_step_follows_the_job_acl() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let ws = actions_workspace();
    // The ACL is checked before the step, so the steps' state does not matter.
    let viewed = finished_job(&app.pool, &ws, "view-line", json!({}), &[], "running").await?;
    let denied = finished_job(&app.pool, &ws, "deny-line", json!({}), &[], "running").await?;
    let args = |job: Uuid| json!({"job_id": job.to_string(), "step_name": "a", "approved": true});

    let (_, message) = app.err("approve_step", args(viewed)).await;
    assert_eq!(message, "Insufficient permissions to approve this step");
    let (_, message) = app.err("approve_step", args(denied)).await;
    assert_eq!(message, "Job not found");
    let (_, message) = app.err("approve_step", args(Uuid::new_v4())).await;
    assert_eq!(message, "Job not found");
    let (code, _) = app
        .err(
            "approve_step",
            json!({"job_id": "not-a-uuid", "step_name": "a", "approved": true}),
        )
        .await;
    assert_eq!(code, INVALID_PARAMS);
    Ok(())
}

// ─── rerun_job ───────────────────────────────────────────────────────────────

/// The new job's stored `raw_input` and resolved `input`.
async fn inputs_of(pool: &PgPool, job_id: Uuid) -> Result<(Value, Value)> {
    let job = JobRepo::get(pool, job_id).await?.expect("job row");
    Ok((
        job.raw_input.unwrap_or(Value::Null),
        job.input.unwrap_or(Value::Null),
    ))
}

async fn rerun(app: &App, args: Value) -> Result<Uuid> {
    let body = app.ok("rerun_job", args).await;
    Ok(body["job_id"].as_str().expect("job_id").parse()?)
}

#[tokio::test]
async fn rerun_job_replays_every_field_it_is_not_given() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let source = failed_line_job(&app.pool).await?;

    let new_id = rerun(&app, json!({"job_id": source.to_string()})).await?;
    let job = JobRepo::get(&app.pool, new_id).await?.expect("job row");
    assert_eq!(job.source_type, "rerun");
    assert_eq!(job.source_job_id, Some(source));
    assert_eq!(job.source_id.as_deref(), Some(USER));
    assert_eq!(job.task_name, "line");
    let (raw, input) = inputs_of(&app.pool, new_id).await?;
    assert_eq!(raw, json!({"note": "n1"}));
    assert_eq!(input["note"], json!("n1"), "{input}");

    // A given field overrides the source's value.
    let new_id = rerun(
        &app,
        json!({"job_id": source.to_string(), "input": {"note": "n2"}}),
    )
    .await?;
    assert_eq!(inputs_of(&app.pool, new_id).await?.0, json!({"note": "n2"}));
    // So does a stringified object (some MCP clients send one).
    let new_id = rerun(
        &app,
        json!({"job_id": source.to_string(), "input": "{\"note\": \"n3\"}"}),
    )
    .await?;
    assert_eq!(inputs_of(&app.pool, new_id).await?.0, json!({"note": "n3"}));
    Ok(())
}

/// The caller never sees or sends the secret: it is replayed from the
/// source job's stored input.
#[tokio::test]
async fn rerun_job_replays_a_secret_the_caller_does_not_know() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let source = finished_job(
        &app.pool,
        &actions_workspace(),
        "secretive",
        json!({"token": "s3cr3t-value", "note": "x"}),
        &[("a", "failed")],
        "failed",
    )
    .await?;

    for args in [
        json!({"job_id": source.to_string()}),
        json!({"job_id": source.to_string(), "input": {"note": "y"}}),
    ] {
        let new_id = rerun(&app, args.clone()).await?;
        let (raw, input) = inputs_of(&app.pool, new_id).await?;
        assert_eq!(raw["token"], json!("s3cr3t-value"), "{args}");
        assert_eq!(input["token"], json!("s3cr3t-value"), "{args}");
    }
    Ok(())
}

async fn attached_file(pool: &PgPool, job_id: Uuid, field: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT file_id FROM job_input_file WHERE job_id = $1 AND field = $2")
            .bind(job_id)
            .bind(field)
            .fetch_optional(pool)
            .await?,
    )
}

/// A required `file` field is replayed like any other field: the new job is
/// attached to the source's file (lineage, A2), with no upload and no value
/// in `input`.
#[tokio::test]
async fn rerun_job_replays_a_file_field() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let body = app
        .ok(
            "execute_task",
            json!({"workspace": "default", "task_name": "with-file", "input": {
                "data_file": {"name": "a.csv", "content": "x,y\n"}, "note": "n"}}),
        )
        .await;
    let source: Uuid = body["job_id"].as_str().expect("job_id").parse()?;
    let source_file = attached_file(&app.pool, source, "data_file")
        .await?
        .expect("the source has its file");

    let new_id = rerun(&app, json!({"job_id": source.to_string()})).await?;
    assert_eq!(
        attached_file(&app.pool, new_id, "data_file").await?,
        Some(source_file.clone())
    );
    // An override of another field keeps the file too.
    let new_id = rerun(
        &app,
        json!({"job_id": source.to_string(), "input": {"note": "m"}}),
    )
    .await?;
    assert_eq!(
        attached_file(&app.pool, new_id, "data_file").await?,
        Some(source_file)
    );
    Ok(())
}

/// Only fields the task declares today are replayed: a key the task dropped
/// since the source ran is left out instead of failing the re-run.
#[tokio::test]
async fn rerun_job_leaves_out_a_field_the_task_dropped() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let source = failed_line_job(&app.pool).await?;
    sqlx::query("UPDATE job SET raw_input = $2 WHERE job_id = $1")
        .bind(source)
        .bind(json!({"note": "n1", "old": "gone"}))
        .execute(&app.pool)
        .await?;

    let new_id = rerun(&app, json!({"job_id": source.to_string()})).await?;
    assert_eq!(inputs_of(&app.pool, new_id).await?.0, json!({"note": "n1"}));
    Ok(())
}

#[tokio::test]
async fn rerun_job_rejects_a_newly_required_field() -> Result<()> {
    let mut drifted = actions_workspace();
    let extra: InputFieldDef = serde_yaml::from_str("{type: string, required: true}")?;
    drifted
        .tasks
        .get_mut("line")
        .unwrap()
        .input
        .insert("extra".to_string(), extra);
    let app = App::new(drifted).await?;
    let source = failed_line_job(&app.pool).await?;

    let (code, message) = app
        .err("rerun_job", json!({"job_id": source.to_string()}))
        .await;
    assert_eq!(code, INVALID_PARAMS, "{message}");
    assert_eq!(
        message,
        "the source job has no value for required field 'extra'"
    );
    // Given in `input`, the re-run goes ahead.
    rerun(
        &app,
        json!({"job_id": source.to_string(), "input": {"extra": "now"}}),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn rerun_job_follows_the_source_and_task_acl() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let ws = actions_workspace();
    let viewed = finished_job(&app.pool, &ws, "view-line", json!({}), &[], "completed").await?;
    let denied = finished_job(&app.pool, &ws, "deny-line", json!({}), &[], "completed").await?;

    let (_, message) = app
        .err("rerun_job", json!({"job_id": denied.to_string()}))
        .await;
    assert_eq!(message, "Source job not found");
    let (_, message) = app
        .err("rerun_job", json!({"job_id": Uuid::new_v4().to_string()}))
        .await;
    assert_eq!(message, "Source job not found");
    let (_, message) = app
        .err("rerun_job", json!({"job_id": viewed.to_string()}))
        .await;
    assert_eq!(message, "View-only access");
    let (code, _) = app.err("rerun_job", json!({"job_id": "not-a-uuid"})).await;
    assert_eq!(code, INVALID_PARAMS);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM job WHERE source_type = 'rerun'")
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn rerun_job_rejects_sources_it_cannot_rerun() -> Result<()> {
    let app = App::new(actions_workspace()).await?;
    let parent = failed_line_job(&app.pool).await?;
    let child = failed_line_job(&app.pool).await?;
    sqlx::query("UPDATE job SET parent_job_id = $2 WHERE job_id = $1")
        .bind(child)
        .bind(parent)
        .execute(&app.pool)
        .await?;
    let hook = failed_line_job(&app.pool).await?;
    sqlx::query("UPDATE job SET source_type = 'hook' WHERE job_id = $1")
        .bind(hook)
        .execute(&app.pool)
        .await?;
    for job in [child, hook] {
        let (code, message) = app
            .err("rerun_job", json!({"job_id": job.to_string()}))
            .await;
        assert_eq!(code, INVALID_PARAMS, "{message}");
        assert_eq!(message, "Only top-level jobs can be re-run");
    }

    let legacy = failed_line_job(&app.pool).await?;
    sqlx::query("UPDATE job SET raw_input = NULL WHERE job_id = $1")
        .bind(legacy)
        .execute(&app.pool)
        .await?;
    let (_, message) = app
        .err("rerun_job", json!({"job_id": legacy.to_string()}))
        .await;
    assert_eq!(message, "Source job predates Re-run prefill (no raw_input)");

    let (code, _) = app
        .err(
            "rerun_job",
            json!({"job_id": parent.to_string(), "input": [1, 2]}),
        )
        .await;
    assert_eq!(code, INVALID_PARAMS);
    assert_eq!(
        app.count("SELECT COUNT(*) FROM job WHERE source_type = 'rerun'")
            .await?,
        0
    );
    Ok(())
}
