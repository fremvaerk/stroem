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
use tower::ServiceExt;
use uuid::Uuid;

/// Where the fixture commits each workspace's YAML (R2 / Task 9).
const RP_YAML_PATH: &str = "workflow.yaml";

const SECRET_23: &str = "ref-only-s3cret-2-3";
const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";
/// A well-formed commit id that exists in no repository.
#[allow(dead_code)] // the permanent-failure tests (Tasks 17–19)
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

/// The pin `commit` of `etl` fails transiently on `replica` (proves the
/// fail-closed path ran on a `PinUnavailable`, not a permanent error).
async fn rp_assert_pin_transient(replica: &Replica, commit: &str) {
    match replica.state.workspaces.pins().ensure("etl", commit).await {
        Ok(_) => panic!("the remote is broken: the pin cannot load"),
        Err(err) => assert!(err.is_transient(), "expected PinUnavailable, got {err:?}"),
    }
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
        rp_assert_pin_transient(&replica, &fx.commits.etl_release).await;
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
        rp_assert_pin_transient(&replica, &fx.commits.etl_release).await;
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
        rp_assert_pin_transient(&replica, &release_24).await;
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
