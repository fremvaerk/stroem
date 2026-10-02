//! Git refs — read paths (plan Tasks 16–19): per-job redaction, webhook
//! re-match + redaction, the § 7.8 job ACL rule on every job-scoped path,
//! and state partitions that follow the job.

mod common;

use std::time::Duration;

use anyhow::Result;
use axum::body::Body;
use axum::Router;
use common::pinned::*;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
#[allow(unused_imports)] // the job ACL tests (Task 18)
use stroem_server::config::{AclAction, AclConfig, AclRule};
use stroem_server::state::AppState;
use stroem_server::workspace::pins::PinError;
use tower::ServiceExt;
use uuid::Uuid;

/// Where the fixture commits each workspace's YAML (R2 / Task 9).
const RP_YAML_PATH: &str = "workflow.yaml";

const SECRET_23: &str = "ref-only-s3cret-2-3";
const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";
/// A well-formed commit id that exists in no repository.
const MISSING_COMMIT: &str = "00000000000000000000000000000000000000ff";

/// Every test in this file runs under this bound (testcontainers + git).
const RP_TIMEOUT: Duration = Duration::from_secs(240);

/// Run a test body under [`RP_TIMEOUT`].
async fn rp_bounded(body: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    tokio::time::timeout(RP_TIMEOUT, body)
        .await
        .map_err(|_| anyhow::anyhow!("test timed out after {RP_TIMEOUT:?}"))?
}

/// `etl` main: `nightly` lives in folder `public` and there is no secret.
const RP_MAIN: &str = r#"
actions:
  echo:
    type: script
    runner: local
    script: echo hi
tasks:
  nightly:
    folder: public
    flow:
      run:
        action: echo
"#;

/// `etl` release/2.3: `nightly` moved to `restricted`, plus a secret that
/// exists ONLY on this ref.
const RP_RELEASE_23: &str = r#"
secrets:
  TOKEN: "ref-only-s3cret-2-3"
actions:
  echo:
    type: script
    runner: local
    script: echo hi
tasks:
  nightly:
    folder: restricted
    flow:
      run:
        action: echo
"#;

/// `etl` release/2.4 (created by the tests that need it): back in `public`.
const RP_RELEASE_24: &str = r#"
secrets:
  TOKEN: "ref-only-s3cret-2-4"
actions:
  echo:
    type: script
    runner: local
    script: echo hi
tasks:
  nightly:
    folder: public
    flow:
      run:
        action: echo
"#;

fn rp_opts() -> PinnedFixtureOpts {
    PinnedFixtureOpts {
        etl_main: Some(RP_MAIN.to_string()),
        etl_release: Some(RP_RELEASE_23.to_string()),
        ..Default::default()
    }
}

/// Create `release/2.4` from `release/2.3`; returns its commit.
fn rp_release_24(fx: &PinnedFixture) -> String {
    fx.etl.commit(
        "release/2.4",
        "release/2.3",
        &[(RP_YAML_PATH, RP_RELEASE_24)],
    )
}

struct RpJob<'a> {
    workspace: &'a str,
    task: &'a str,
    git_ref: Option<&'a str>,
    revision: Option<String>,
    task_folder: Option<&'a str>,
    status: &'a str,
    source_type: &'a str,
    source_id: Option<&'a str>,
    output: Option<Value>,
    age_secs: f64,
}

impl Default for RpJob<'_> {
    fn default() -> Self {
        RpJob {
            workspace: "etl",
            task: "nightly",
            git_ref: None,
            revision: None,
            task_folder: None,
            status: "completed",
            source_type: "api",
            source_id: None,
            output: None,
            age_secs: 0.0,
        }
    }
}

/// Insert a job row directly (no creation path), so a test controls
/// `git_ref` / `revision` / `task_folder` exactly.
async fn rp_seed_job(pool: &PgPool, s: RpJob<'_>) -> Uuid {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, source_id, \
                          revision, git_ref, task_folder, output, created_at, completed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
                 NOW() - make_interval(secs => $11), NOW())",
    )
    .bind(job_id)
    .bind(s.workspace)
    .bind(s.task)
    .bind(s.status)
    .bind(s.source_type)
    .bind(s.source_id)
    .bind(s.revision)
    .bind(s.git_ref)
    .bind(s.task_folder)
    .bind(s.output)
    .bind(s.age_secs)
    .execute(pool)
    .await
    .expect("seed job");
    job_id
}

struct RpStep<'a> {
    step: &'a str,
    action_type: &'a str,
    status: &'a str,
    output: Option<Value>,
    error: Option<&'a str>,
    worker_id: Option<Uuid>,
}

async fn rp_seed_step(pool: &PgPool, job_id: Uuid, s: RpStep<'_>) {
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, \
                               output, error_message, worker_id, started_at) \
         VALUES ($1, $2, $2, $3, $4, $5, $6, $7, NOW())",
    )
    .bind(job_id)
    .bind(s.step)
    .bind(s.action_type)
    .bind(s.status)
    .bind(s.output)
    .bind(s.error)
    .bind(s.worker_id)
    .execute(pool)
    .await
    .expect("seed step");
}

/// The fixture's `register_worker` returns the id as a string.
async fn rp_worker(fx: &PinnedFixture) -> Uuid {
    register_worker(&fx.router, &["script"])
        .await
        .parse()
        .expect("worker id")
}

/// Raw-body worker request (state tarballs are gzip bytes, not JSON).
#[allow(dead_code)] // the state tests (Task 19)
async fn rp_worker_bytes(
    router: &Router,
    method: &str,
    uri: &str,
    body: Vec<u8>,
) -> (u16, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {FIXTURE_WORKER_TOKEN}"))
        .header("content-type", "application/gzip")
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

/// One MCP `tools/call` (initialize first; the session id, if any, is reused).
async fn rp_mcp_call(router: &Router, token: Option<&str>, tool: &str, args: Value) -> Value {
    let build = |body: Value, sid: Option<&str>| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(t) = token {
            b = b.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(s) = sid {
            b = b.header("Mcp-Session-Id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };
    let init = router
        .clone()
        .oneshot(build(
            json!({"jsonrpc": "2.0", "method": "initialize", "id": 0, "params": {
                "protocolVersion": "2025-03-26", "capabilities": {},
                "clientInfo": {"name": "t", "version": "1"}}}),
            None,
        ))
        .await
        .unwrap();
    let sid = init
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let resp = router
        .clone()
        .oneshot(build(
            json!({"jsonrpc": "2.0", "method": "tools/call", "id": 1,
                   "params": {"name": tool, "arguments": args}}),
            sid.as_deref(),
        ))
        .await
        .unwrap();
    json_body(resp).await
}

fn rp_mcp_json(resp: &Value) -> Value {
    serde_json::from_str(
        resp["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text"),
    )
    .unwrap()
}

fn rp_mcp_is_error(resp: &Value) -> bool {
    resp.get("error").is_some() || resp["result"]["isError"].as_bool().unwrap_or(false)
}

// ── Task 16: per-job redaction ─────────────────────────────────────────

/// The exact fail-closed message of job detail (503 body) and MCP status.
const RP_REDACTION_UNAVAILABLE: &str = "redaction set unavailable, retry";

/// A failed pinned `etl/nightly` job on release/2.3 whose output, approval
/// message and step error all carry the ref-only secret.
async fn rp_seed_pinned_23(fx: &PinnedFixture, revision: String) -> Uuid {
    let job_id = rp_seed_job(
        &fx.pool,
        RpJob {
            git_ref: Some("release/2.3"),
            revision: Some(revision),
            task_folder: Some("restricted"),
            status: "failed",
            output: Some(json!({"leak": SECRET_23})),
            ..Default::default()
        },
    )
    .await;
    rp_seed_step(
        &fx.pool,
        job_id,
        RpStep {
            step: "gate",
            action_type: "approval",
            status: "completed",
            output: Some(json!({"approval_message": format!("approve with {SECRET_23}?")})),
            error: None,
            worker_id: None,
        },
    )
    .await;
    rp_seed_step(
        &fx.pool,
        job_id,
        RpStep {
            step: "run",
            action_type: "script",
            status: "failed",
            output: None,
            error: Some(&format!("exit 1: {SECRET_23}")),
            worker_id: None,
        },
    )
    .await;
    job_id
}

/// A second replica whose PinStore has never loaded anything, then a git
/// outage: every pin it has not loaded is a TRANSIENT `PinUnavailable`. The
/// replica is created BEFORE the remote breaks (it clones the live repos).
/// The caller restores the remote.
async fn rp_cold_replica_during_outage(fx: &PinnedFixture) -> Result<Replica> {
    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    Ok(replica)
}

/// The error of loading `etl` at `commit` on `state`'s replica.
async fn rp_pin_error(state: &AppState, commit: &str) -> PinError {
    match state.workspaces.pins().ensure("etl", commit).await {
        Ok(_) => panic!("the pin {commit} must not load here"),
        Err(err) => err,
    }
}

/// The pin `commit` of `etl` fails transiently on this replica (proves the
/// fail-closed path ran on a `PinUnavailable`, not a permanent error).
async fn rp_assert_pin_transient(state: &AppState, commit: &str) {
    let err = rp_pin_error(state, commit).await;
    assert!(err.is_transient(), "expected PinUnavailable, got {err:?}");
}

/// The pin `commit` of `etl` fails permanently on this replica (e.g.
/// `CommitNotFound`): no retry can ever build its redaction set.
async fn rp_assert_pin_permanent(state: &AppState, commit: &str) {
    let err = rp_pin_error(state, commit).await;
    assert!(
        !err.is_transient(),
        "expected a permanent error, got {err:?}"
    );
}

/// A step entry of a job-detail / MCP-status body, by name.
fn rp_step(body: &Value, name: &str) -> Value {
    body["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .find(|s| s["step_name"] == name)
        .unwrap_or_else(|| panic!("no step {name} in {body}"))
        .clone()
}

/// Identifiers, statuses and timestamps survive a masked answer.
fn rp_assert_identifiers_intact(body: &Value, job_id: Uuid) {
    assert_eq!(body["job_id"], json!(job_id), "{body}");
    assert_eq!(body["workspace"], json!("etl"));
    assert_eq!(body["task_name"], json!("nightly"));
    assert_eq!(body["status"], json!("failed"));
    assert_eq!(body["revision"], json!(MISSING_COMMIT));
    assert!(body["created_at"].as_str().is_some_and(|t| t != MASK));
    let run = rp_step(body, "run");
    assert_eq!(run["status"], json!("failed"));
    assert_eq!(run["action_name"], json!("run"));
    assert_eq!(rp_step(body, "gate")["status"], json!("completed"));
}

#[tokio::test]
async fn job_detail_masks_ref_only_secret_in_every_field() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_opts()).await?;
        let job_id = rp_seed_pinned_23(&fx, fx.commits.etl_release.clone()).await;

        let (status, body) = api_req(
            &fx.router,
            "GET",
            &format!("/api/jobs/{job_id}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.to_string().contains(SECRET_23),
            "secret leaked: {body}"
        );
        let step = |name: &str| {
            body["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["step_name"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(
            step("gate")["approval_message"],
            json!(format!("approve with {MASK}?"))
        );
        assert_eq!(
            step("run")["error_message"],
            json!(format!("exit 1: {MASK}"))
        );
        assert_eq!(body["output"]["leak"], json!(MASK));
        assert_eq!(body["workspace"], json!("etl"), "identifiers untouched");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn job_detail_fails_closed_when_a_pin_cannot_be_loaded() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_opts()).await?;
        let job_id = rp_seed_pinned_23(&fx, fx.commits.etl_release.clone()).await;

        let replica = rp_cold_replica_during_outage(&fx).await?;
        let (status, body) = api_req(
            &replica.router,
            "GET",
            &format!("/api/jobs/{job_id}"),
            None,
            None,
        )
        .await;
        rp_assert_pin_transient(&replica.state, &fx.commits.etl_release).await;
        fx.etl.restore_remote();

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body, json!({"error": RP_REDACTION_UNAVAILABLE}));
        assert!(!body.to_string().contains(SECRET_23));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn mcp_get_job_status_masks_ref_only_secret_and_fails_closed() -> Result<()> {
    rp_bounded(async {
        let mut o = rp_opts();
        o.mcp = true;
        let fx = pinned_workspace_fixture(o).await?;

        let ok = rp_seed_pinned_23(&fx, fx.commits.etl_release.clone()).await;
        let resp = rp_mcp_call(
            &fx.router,
            None,
            "get_job_status",
            json!({"job_id": ok.to_string()}),
        )
        .await;
        assert!(!rp_mcp_is_error(&resp), "{resp}");
        let status = rp_mcp_json(&resp);
        assert!(
            !status.to_string().contains(SECRET_23),
            "secret leaked: {status}"
        );
        let run = status["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["step_name"] == "run")
            .unwrap();
        assert_eq!(run["error_message"], json!(format!("exit 1: {MASK}")));

        let broken = rp_seed_pinned_23(&fx, fx.commits.etl_release.clone()).await;
        let replica = rp_cold_replica_during_outage(&fx).await?;
        let resp = rp_mcp_call(
            &replica.router,
            None,
            "get_job_status",
            json!({"job_id": broken.to_string()}),
        )
        .await;
        rp_assert_pin_transient(&replica.state, &fx.commits.etl_release).await;
        fx.etl.restore_remote();

        assert!(rp_mcp_is_error(&resp), "must fail closed: {resp}");
        assert_eq!(
            resp["error"]["message"],
            json!(RP_REDACTION_UNAVAILABLE),
            "{resp}"
        );
        assert!(!resp.to_string().contains(SECRET_23));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn worker_detail_masks_error_and_fails_closed_per_row() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_opts()).await?;
        let worker = rp_worker(&fx).await;

        // A cold replica that loads release/2.3 while the remote is up. Its
        // fetch may bring every head the remote has, so release/2.4 is only
        // committed AFTER it: the replica has never seen that commit, and
        // once the remote breaks its pin is (transiently) unavailable.
        let replica = fx.second_replica().await?;
        replica
            .state
            .workspaces
            .pins()
            .ensure("etl", &fx.commits.etl_release)
            .await?;
        let release_24 = rp_release_24(&fx);

        // release/2.3: its pin loads, so its own secret is masked in place.
        let good = rp_seed_job(
            &fx.pool,
            RpJob {
                git_ref: Some("release/2.3"),
                revision: Some(fx.commits.etl_release.clone()),
                status: "failed",
                ..Default::default()
            },
        )
        .await;
        rp_seed_step(
            &fx.pool,
            good,
            RpStep {
                step: "run",
                action_type: "script",
                status: "failed",
                output: None,
                error: Some(&format!("exit 1: {SECRET_23}")),
                worker_id: Some(worker),
            },
        )
        .await;

        // release/2.4: its pin is unavailable on the replica below.
        let broken = rp_seed_job(
            &fx.pool,
            RpJob {
                git_ref: Some("release/2.4"),
                revision: Some(release_24.clone()),
                status: "failed",
                ..Default::default()
            },
        )
        .await;
        rp_seed_step(
            &fx.pool,
            broken,
            RpStep {
                step: "run",
                action_type: "script",
                status: "failed",
                output: None,
                error: Some("exit 1: something"),
                worker_id: Some(worker),
            },
        )
        .await;

        // Unpinned: the live set suffices, nothing to fail on.
        let unpinned = rp_seed_job(
            &fx.pool,
            RpJob {
                status: "failed",
                ..Default::default()
            },
        )
        .await;
        rp_seed_step(
            &fx.pool,
            unpinned,
            RpStep {
                step: "run",
                action_type: "script",
                status: "failed",
                output: None,
                error: Some("exit 1: plain"),
                worker_id: Some(worker),
            },
        )
        .await;

        fx.etl.break_remote();
        let (status, body) = api_req(
            &replica.router,
            "GET",
            &format!("/api/workers/{worker}"),
            None,
            None,
        )
        .await;
        rp_assert_pin_transient(&replica.state, &release_24).await;
        fx.etl.restore_remote();

        assert_eq!(status, StatusCode::OK, "{body}");
        let items = body["steps"]["items"].as_array().unwrap();
        let row = |id: Uuid| {
            items
                .iter()
                .find(|r| r["job_id"] == json!(id))
                .unwrap()
                .clone()
        };
        assert_eq!(row(good)["error_message"], json!(format!("exit 1: {MASK}")));
        assert_eq!(
            row(broken)["error_message"],
            json!(MASK),
            "unavailable pin ⇒ whole field masked"
        );
        assert_eq!(
            row(unpinned)["error_message"],
            json!("exit 1: plain"),
            "fail closed per row, not per page"
        );
        Ok(())
    })
    .await
}

// ── Task 16 fix round 1: permanent pin failures, own live secrets ──────

/// A pin that can NEVER load (`CommitNotFound`) must not answer 503 forever:
/// job detail answers 200 with every content string masked whole.
#[tokio::test]
async fn job_detail_masks_everything_when_a_pin_is_permanently_unloadable() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_opts()).await?;
        let job_id = rp_seed_pinned_23(&fx, MISSING_COMMIT.to_string()).await;

        let (status, body) = api_req(
            &fx.router,
            "GET",
            &format!("/api/jobs/{job_id}"),
            None,
            None,
        )
        .await;
        rp_assert_pin_permanent(&fx.state, MISSING_COMMIT).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.to_string().contains(SECRET_23),
            "secret leaked: {body}"
        );
        assert_eq!(body["output"]["leak"], json!(MASK));
        assert_eq!(rp_step(&body, "gate")["approval_message"], json!(MASK));
        assert_eq!(rp_step(&body, "run")["error_message"], json!(MASK));
        rp_assert_identifiers_intact(&body, job_id);
        Ok(())
    })
    .await
}

/// MCP status applies the same split: a permanent pin failure is a normal
/// result with every content string masked.
#[tokio::test]
async fn mcp_get_job_status_masks_everything_when_a_pin_is_permanently_unloadable() -> Result<()> {
    rp_bounded(async {
        let mut o = rp_opts();
        o.mcp = true;
        let fx = pinned_workspace_fixture(o).await?;
        let job_id = rp_seed_pinned_23(&fx, MISSING_COMMIT.to_string()).await;

        let resp = rp_mcp_call(
            &fx.router,
            None,
            "get_job_status",
            json!({"job_id": job_id.to_string()}),
        )
        .await;
        rp_assert_pin_permanent(&fx.state, MISSING_COMMIT).await;

        assert!(!rp_mcp_is_error(&resp), "{resp}");
        assert!(
            !resp.to_string().contains(SECRET_23),
            "secret leaked: {resp}"
        );
        let status = rp_mcp_json(&resp);
        assert_eq!(rp_step(&status, "run")["error_message"], json!(MASK));
        rp_assert_identifiers_intact(&status, job_id);
        Ok(())
    })
    .await
}

/// A secret that exists ONLY in the live config of the job's own workspace
/// (not at the pinned commit). `pin_redaction_values` reads the pinned
/// workspace from its pin, so only the live set covers this value.
const RP_LIVE_ONLY_SECRET: &str = "live-only-s3cret-main";

#[tokio::test]
async fn job_detail_of_a_pinned_job_masks_its_own_workspace_live_secret() -> Result<()> {
    rp_bounded(async {
        let mut o = rp_opts();
        o.etl_main = Some(format!(
            "\nsecrets:\n  LIVE: \"{RP_LIVE_ONLY_SECRET}\"{RP_MAIN}"
        ));
        let fx = pinned_workspace_fixture(o).await?;
        let job_id = rp_seed_job(
            &fx.pool,
            RpJob {
                git_ref: Some("release/2.3"),
                revision: Some(fx.commits.etl_release.clone()),
                status: "failed",
                output: Some(json!({
                    "live": format!("token={RP_LIVE_ONLY_SECRET}"),
                    "pinned": SECRET_23,
                })),
                ..Default::default()
            },
        )
        .await;

        let (status, body) = api_req(
            &fx.router,
            "GET",
            &format!("/api/jobs/{job_id}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.to_string().contains(RP_LIVE_ONLY_SECRET),
            "live secret leaked: {body}"
        );
        assert_eq!(body["output"]["live"], json!(format!("token={MASK}")));
        assert_eq!(body["output"]["pinned"], json!(MASK));
        Ok(())
    })
    .await
}

// ── Task 17: webhooks ──────────────────────────────────────────────────

fn rp_hook_yaml(git_ref: &str, secret: &str) -> String {
    format!(
        r#"
triggers:
  on-nightly:
    type: webhook
    name: nightly-hook
    task: nightly
    ref: {git_ref}
    secret: {secret}
    force_refresh: true
"#
    )
}

fn rp_hook_opts() -> PinnedFixtureOpts {
    let mut o = rp_opts();
    o.etl_main = Some(format!(
        "{RP_MAIN}{}",
        rp_hook_yaml("release/2.3", "hook-secret-1")
    ));
    o
}

/// Re-commit `etl` main's YAML.
fn rp_commit_main(fx: &PinnedFixture, yaml: &str) {
    fx.etl.commit("main", "main", &[(RP_YAML_PATH, yaml)]);
}

async fn rp_job_git_ref(pool: &PgPool, job_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT git_ref FROM job WHERE job_id = $1::uuid")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn rp_webhook_job_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE source_type = 'webhook'")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// How long a test polls for a side effect of a spawned request.
const RP_POLL_BOUND: Duration = Duration::from_secs(10);

/// The job a spawned webhook call created (`source_id`), polled for at most
/// [`RP_POLL_BOUND`]. Bails out as soon as the call has returned without one.
async fn rp_await_webhook_job<T>(
    pool: &PgPool,
    source_id: &str,
    call: &tokio::task::JoinHandle<T>,
) -> Result<Uuid> {
    let deadline = tokio::time::Instant::now() + RP_POLL_BOUND;
    loop {
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT job_id FROM job WHERE source_type = 'webhook' AND source_id = $1",
        )
        .bind(source_id)
        .fetch_optional(pool)
        .await?;
        if let Some(id) = id {
            return Ok(id);
        }
        anyhow::ensure!(
            !call.is_finished(),
            "the webhook call returned without creating a job"
        );
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "no webhook job after {RP_POLL_BOUND:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Mark `job_id` completed with `output` and broadcast its completion, as
/// terminal handling would.
async fn rp_complete_job(fx: &PinnedFixture, job_id: Uuid, output: Value) -> Result<()> {
    sqlx::query(
        "UPDATE job SET status = 'completed', output = $2, completed_at = NOW() WHERE job_id = $1",
    )
    .bind(job_id)
    .bind(&output)
    .execute(&fx.pool)
    .await?;
    fx.state
        .job_completion
        .notify(stroem_server::job_completion::JobCompletionEvent {
            job_id,
            status: "completed".to_string(),
            output: Some(output),
        })
        .await;
    Ok(())
}

#[tokio::test]
async fn webhook_refresh_that_changes_the_ref_runs_the_new_ref() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_hook_opts()).await?;
        let release_24 = rp_release_24(&fx);
        rp_commit_main(
            &fx,
            &format!("{RP_MAIN}{}", rp_hook_yaml("release/2.4", "hook-secret-1")),
        );

        let (status, body) = api_req(
            &fx.router,
            "POST",
            "/hooks/nightly-hook?secret=hook-secret-1",
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let job_id = body["job_id"].as_str().unwrap();
        assert_eq!(
            rp_job_git_ref(&fx.pool, job_id).await.as_deref(),
            Some("release/2.4")
        );
        let (revision, folder): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT revision, task_folder FROM job WHERE job_id = $1::uuid")
                .bind(job_id)
                .fetch_one(&fx.pool)
                .await?;
        assert_eq!(revision, Some(release_24), "pinned to the new ref's commit");
        assert_eq!(folder.as_deref(), Some("public"), "the folder at 2.4");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn webhook_refresh_that_removes_the_webhook_answers_404() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_hook_opts()).await?;
        rp_commit_main(&fx, RP_MAIN);

        let (status, body) = api_req(
            &fx.router,
            "POST",
            "/hooks/nightly-hook?secret=hook-secret-1",
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(
            rp_webhook_job_count(&fx.pool).await,
            0,
            "no job may be created"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn webhook_refresh_that_rotates_the_secret_rejects_the_old_one() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_hook_opts()).await?;
        rp_commit_main(
            &fx,
            &format!("{RP_MAIN}{}", rp_hook_yaml("release/2.3", "hook-secret-2")),
        );

        // Old secret passes the cached pre-check, triggers the refresh, then
        // fails against the fresh definition.
        let (status, body) = api_req(
            &fx.router,
            "POST",
            "/hooks/nightly-hook?secret=hook-secret-1",
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(
            rp_webhook_job_count(&fx.pool).await,
            0,
            "no job may be created"
        );

        let (status, body) = api_req(
            &fx.router,
            "POST",
            "/hooks/nightly-hook?secret=hook-secret-2",
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        Ok(())
    })
    .await
}

/// F16 / spec § 7.5 step 1: the pre-refresh check against the CACHED secret
/// is kept, so a caller holding only a newly rotated-in secret gets 401 —
/// and cannot make the server fetch from git — until the server has loaded it.
#[tokio::test]
async fn webhook_rotated_in_secret_before_any_refresh_is_rejected_without_a_refresh() -> Result<()>
{
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_hook_opts()).await?;
        rp_commit_main(
            &fx,
            &format!("{RP_MAIN}{}", rp_hook_yaml("release/2.3", "hook-secret-2")),
        );

        let (status, body) = api_req(
            &fx.router,
            "POST",
            "/hooks/nightly-hook?secret=hook-secret-2",
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(
            rp_webhook_job_count(&fx.pool).await,
            0,
            "no job may be created"
        );
        assert_eq!(
            fx.mgr().get_revision("etl"),
            Some(fx.commits.etl_main.clone()),
            "an unauthenticated call must not reload the workspace"
        );
        Ok(())
    })
    .await
}

/// F17 / spec § 7.5 step 2: a reload that leaves the workspace errored (no
/// config) is a server condition — 500, never 404.
#[tokio::test]
async fn webhook_refresh_that_errors_the_workspace_answers_500() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_hook_opts()).await?;
        fx.etl.break_remote();

        let (status, body) = api_req(
            &fx.router,
            "POST",
            "/hooks/nightly-hook?secret=hook-secret-1",
            None,
            Some(json!({})),
        )
        .await;
        let errored = fx.mgr().get_config("etl").await.is_none();
        fx.etl.restore_remote();

        assert!(errored, "the failed reload must leave `etl` errored");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(
            rp_webhook_job_count(&fx.pool).await,
            0,
            "no job may be created"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn webhook_status_poll_masks_ref_only_secret_in_every_branch() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_hook_opts()).await?;
        let head = fx.commits.etl_release.clone();
        let seed = |status: &'static str| RpJob {
            git_ref: Some("release/2.3"),
            revision: Some(head.clone()),
            status,
            source_type: "webhook",
            source_id: Some("etl/on-nightly"),
            output: Some(json!({"leak": SECRET_23})),
            ..Default::default()
        };
        let done = rp_seed_job(&fx.pool, seed("completed")).await;
        let woken = rp_seed_job(&fx.pool, seed("running")).await;
        let slow = rp_seed_job(&fx.pool, seed("running")).await;

        // No wait.
        let (s, b) = api_req(
            &fx.router,
            "GET",
            &format!("/hooks/nightly-hook/jobs/{done}?secret=hook-secret-1"),
            None,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["output"]["leak"], json!(MASK));

        // wait=true on a running job, woken by a completion event: the wait
        // branch's re-query arm. The job stays `running` in the DB, so
        // neither the early return nor the post-subscribe re-check can answer;
        // only the event can (the timeout arm would be a 202 after 30 s).
        // The handler subscribes at an unknown moment and an event sent
        // before that is dropped, so keep notifying until it answers.
        let router = fx.router.clone();
        let call = tokio::spawn(async move {
            api_req(
                &router,
                "GET",
                &format!(
                    "/hooks/nightly-hook/jobs/{woken}?secret=hook-secret-1&wait=true&timeout=30"
                ),
                None,
                None,
            )
            .await
        });
        let deadline = tokio::time::Instant::now() + RP_POLL_BOUND;
        while !call.is_finished() && tokio::time::Instant::now() < deadline {
            fx.state
                .job_completion
                .notify(stroem_server::job_completion::JobCompletionEvent {
                    job_id: woken,
                    status: "completed".to_string(),
                    output: None,
                })
                .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::ensure!(
            call.is_finished(),
            "the wait was not woken within {RP_POLL_BOUND:?}"
        );
        let (s, b) = call.await?;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b["status"],
            json!("running"),
            "answered from the re-query: {b}"
        );
        assert_eq!(b["output"]["leak"], json!(MASK));

        // wait=true timing out on a running job (202 branch).
        let (s, b) = api_req(
            &fx.router,
            "GET",
            &format!("/hooks/nightly-hook/jobs/{slow}?secret=hook-secret-1&wait=true&timeout=1"),
            None,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
        assert_eq!(b["output"]["leak"], json!(MASK));
        Ok(())
    })
    .await
}

/// A pin that cannot be loaded: a TRANSIENT failure answers 503 with the
/// `job_id`; a PERMANENT one (`CommitNotFound`) answers 200 with the output
/// masked whole — never the raw value.
#[tokio::test]
async fn webhook_status_poll_fails_closed_with_job_id() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_hook_opts()).await?;
        let seed = |revision: String| RpJob {
            git_ref: Some("release/2.3"),
            revision: Some(revision),
            status: "completed",
            source_type: "webhook",
            source_id: Some("etl/on-nightly"),
            output: Some(json!({"leak": SECRET_23})),
            ..Default::default()
        };

        let lost = rp_seed_job(&fx.pool, seed(MISSING_COMMIT.to_string())).await;
        let (s, b) = api_req(
            &fx.router,
            "GET",
            &format!("/hooks/nightly-hook/jobs/{lost}?secret=hook-secret-1"),
            None,
            None,
        )
        .await;
        rp_assert_pin_permanent(&fx.state, MISSING_COMMIT).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["output"]["leak"], json!(MASK));
        assert_eq!(b["job_id"], json!(lost.to_string()));

        let job = rp_seed_job(&fx.pool, seed(fx.commits.etl_release.clone())).await;
        let replica = rp_cold_replica_during_outage(&fx).await?;
        let (s, b) = api_req(
            &replica.router,
            "GET",
            &format!("/hooks/nightly-hook/jobs/{job}?secret=hook-secret-1"),
            None,
            None,
        )
        .await;
        rp_assert_pin_transient(&replica.state, &fx.commits.etl_release).await;
        fx.etl.restore_remote();

        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
        assert_eq!(b["job_id"], json!(job.to_string()));
        assert_eq!(b["error"], json!(RP_REDACTION_UNAVAILABLE));
        assert!(!b.to_string().contains(SECRET_23));
        Ok(())
    })
    .await
}

/// `etl` main with a sync webhook on `nightly` at release/2.3.
fn rp_sync_hook_opts() -> PinnedFixtureOpts {
    let mut o = rp_opts();
    o.etl_main = Some(format!(
        "{RP_MAIN}{}",
        r#"
triggers:
  on-sync:
    type: webhook
    name: nightly-sync
    task: nightly
    ref: release/2.3
    mode: sync
    timeout_secs: 20
"#
    ));
    o
}

#[tokio::test]
async fn sync_webhook_masks_ref_only_secret() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_sync_hook_opts()).await?;

        let router = fx.router.clone();
        let call = tokio::spawn(async move {
            api_req(
                &router,
                "POST",
                "/hooks/nightly-sync",
                None,
                Some(json!({})),
            )
            .await
        });

        // Wait for the webhook's job row, then complete it with a ref-only secret.
        let job_id = rp_await_webhook_job(&fx.pool, "etl/on-sync", &call).await?;
        rp_complete_job(&fx, job_id, json!({"leak": SECRET_23})).await?;

        let (status, body) = call.await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["job_id"], json!(job_id.to_string()));
        assert_eq!(body["output"]["leak"], json!(MASK));
        Ok(())
    })
    .await
}

/// F31: the sync answer fails closed. The job's own pin (release/2.3) was
/// loaded when the job was created, so the outage is staged on a pin it
/// references through a step (`action_ref` release/2.4), committed only
/// after creation: this replica has never fetched it.
#[tokio::test]
async fn sync_webhook_fails_closed_with_job_id() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_sync_hook_opts()).await?;

        let router = fx.router.clone();
        let call = tokio::spawn(async move {
            api_req(
                &router,
                "POST",
                "/hooks/nightly-sync",
                None,
                Some(json!({})),
            )
            .await
        });
        let job_id = rp_await_webhook_job(&fx.pool, "etl/on-sync", &call).await?;

        let release_24 = rp_release_24(&fx);
        fx.etl.break_remote();
        sqlx::query(
            "UPDATE job_step SET action_ref = 'release/2.4', action_revision = $2 \
             WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(&release_24)
        .execute(&fx.pool)
        .await?;
        rp_complete_job(&fx, job_id, json!({"leak": SECRET_23})).await?;

        let (status, body) = call.await?;
        rp_assert_pin_transient(&fx.state, &release_24).await;
        fx.etl.restore_remote();

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["job_id"], json!(job_id.to_string()));
        assert_eq!(body["error"], json!(RP_REDACTION_UNAVAILABLE));
        assert!(!body.to_string().contains(SECRET_23));
        Ok(())
    })
    .await
}
