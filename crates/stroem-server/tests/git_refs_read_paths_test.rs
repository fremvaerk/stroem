//! Git refs — read paths (plan Tasks 16–19): per-job redaction, webhook
//! re-match + redaction, the § 7.8 job ACL rule on every job-scoped path,
//! and state partitions that follow the job.

use std::time::Duration;

use crate::common::pinned::*;
use anyhow::Result;
use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
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
        let resp = mcp_call(
            &fx.router,
            None,
            "get_job_status",
            json!({"job_id": ok.to_string()}),
        )
        .await;
        assert!(!mcp_is_error(&resp), "{resp}");
        let status = mcp_tool_json(&resp);
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
        let resp = mcp_call(
            &replica.router,
            None,
            "get_job_status",
            json!({"job_id": broken.to_string()}),
        )
        .await;
        rp_assert_pin_transient(&replica.state, &fx.commits.etl_release).await;
        fx.etl.restore_remote();

        assert!(mcp_is_error(&resp), "must fail closed: {resp}");
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

        let resp = mcp_call(
            &fx.router,
            None,
            "get_job_status",
            json!({"job_id": job_id.to_string()}),
        )
        .await;
        rp_assert_pin_permanent(&fx.state, MISSING_COMMIT).await;

        assert!(!mcp_is_error(&resp), "{resp}");
        assert!(
            !resp.to_string().contains(SECRET_23),
            "secret leaked: {resp}"
        );
        let status = mcp_tool_json(&resp);
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

// ── Task 18: job ACL ───────────────────────────────────────────────────

const VIEWER: &str = "viewer@test.com";
const RUNNER: &str = "runner@test.com";
const ADMIN: &str = "admin@test.com";
/// A task that exists only on a release: absent from the live config.
const RP_RELEASE_ONLY_TASK: &str = "hotfix";
/// The REST denial of a job-scoped path (`AppError::not_found("Job")`).
const RP_JOB_NOT_FOUND: &str = "Job not found";

/// `viewers` may View, and RUNNER may Run, `etl` tasks in folder `public`.
/// Everything else is denied.
fn rp_acl_opts() -> PinnedFixtureOpts {
    let mut o = rp_opts();
    o.mcp = true;
    o.acl = Some(AclConfig {
        default: AclAction::Deny,
        rules: vec![
            AclRule {
                workspace: "etl".to_string(),
                tasks: vec!["public/*".to_string()],
                action: AclAction::View,
                groups: vec!["viewers".to_string()],
                users: vec![],
            },
            AclRule {
                workspace: "etl".to_string(),
                tasks: vec!["public/*".to_string()],
                action: AclAction::Run,
                groups: vec![],
                users: vec![RUNNER.to_string()],
            },
        ],
    });
    o.users = vec![
        FixtureUser {
            email: VIEWER,
            groups: vec!["viewers"],
            admin: false,
        },
        FixtureUser {
            email: RUNNER,
            groups: vec![],
            admin: false,
        },
        FixtureUser {
            email: ADMIN,
            groups: vec![],
            admin: true,
        },
    ];
    o
}

struct RpAclJobs {
    /// Unpinned; the live `nightly` is in folder `public` → allowed.
    live: Uuid,
    /// release/2.4, `task_folder` `public` → allowed.
    pin24: Uuid,
    /// release/2.3, `task_folder` `restricted` → denied, although its live
    /// namesake sits in the allowed folder `public`.
    pin23: Uuid,
    /// A release-only task (absent live), `task_folder` `public` → allowed.
    rel_public: Uuid,
    /// The same release-only task, `task_folder` `restricted` → denied.
    rel_restricted: Uuid,
}

impl RpAclJobs {
    fn allowed(&self) -> [Uuid; 3] {
        [self.live, self.pin24, self.rel_public]
    }

    fn denied(&self) -> [Uuid; 2] {
        [self.pin23, self.rel_restricted]
    }
}

/// Five completed jobs, each with one completed `run` step on `worker`. The
/// two denied jobs are the most recent, so a LIMIT-before-filter bug lets
/// them fill the first page.
async fn rp_seed_acl_jobs(fx: &PinnedFixture, worker: Uuid) -> RpAclJobs {
    let c24 = rp_release_24(fx);
    let c23 = fx.commits.etl_release.clone();
    let live = rp_seed_job(
        &fx.pool,
        RpJob {
            age_secs: 180.0,
            ..Default::default()
        },
    )
    .await;
    let pin24 = rp_seed_job(
        &fx.pool,
        RpJob {
            git_ref: Some("release/2.4"),
            revision: Some(c24),
            task_folder: Some("public"),
            age_secs: 120.0,
            ..Default::default()
        },
    )
    .await;
    let rel_public = rp_seed_job(
        &fx.pool,
        RpJob {
            task: RP_RELEASE_ONLY_TASK,
            git_ref: Some("release/2.3"),
            revision: Some(c23.clone()),
            task_folder: Some("public"),
            age_secs: 90.0,
            ..Default::default()
        },
    )
    .await;
    let pin23 = rp_seed_job(
        &fx.pool,
        RpJob {
            git_ref: Some("release/2.3"),
            revision: Some(c23.clone()),
            task_folder: Some("restricted"),
            age_secs: 60.0,
            ..Default::default()
        },
    )
    .await;
    let rel_restricted = rp_seed_job(
        &fx.pool,
        RpJob {
            task: RP_RELEASE_ONLY_TASK,
            git_ref: Some("release/2.3"),
            revision: Some(c23),
            task_folder: Some("restricted"),
            age_secs: 30.0,
            ..Default::default()
        },
    )
    .await;
    for id in [live, pin24, rel_public, pin23, rel_restricted] {
        rp_seed_step(
            &fx.pool,
            id,
            RpStep {
                step: "run",
                action_type: "script",
                status: "completed",
                output: None,
                error: None,
                worker_id: Some(worker),
            },
        )
        .await;
    }
    RpAclJobs {
        live,
        pin24,
        pin23,
        rel_public,
        rel_restricted,
    }
}

/// The `job_id`s of a REST job list body.
fn rp_list_ids(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items in {body}"))
        .iter()
        .map(|j| j["job_id"].as_str().unwrap().to_string())
        .collect()
}

/// The job-scoped REST requests of one job: `(method, uri, body)`.
fn rp_job_paths(id: Uuid) -> Vec<(&'static str, String, Option<Value>)> {
    vec![
        ("GET", format!("/api/jobs/{id}"), None),
        ("GET", format!("/api/jobs/{id}/logs"), None),
        ("GET", format!("/api/jobs/{id}/steps/run/logs"), None),
        ("GET", format!("/api/jobs/{id}/artifacts"), None),
        ("GET", format!("/api/jobs/{id}/artifacts/out.txt"), None),
        ("POST", format!("/api/jobs/{id}/cancel"), None),
        (
            "POST",
            format!("/api/jobs/{id}/restart"),
            Some(json!({"from_step": "run", "dry_run": true})),
        ),
        (
            "POST",
            format!("/api/jobs/{id}/steps/run/approve"),
            Some(json!({"approved": true})),
        ),
    ]
}

#[tokio::test]
async fn pinned_job_in_denied_folder_is_denied_on_every_rest_path() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_acl_opts()).await?;
        let worker = rp_worker(&fx).await;
        let jobs = rp_seed_acl_jobs(&fx, worker).await;
        let viewer = fx.login(VIEWER).await;
        let t = Some(viewer.as_str());

        // Every job-scoped path denies both denied jobs, as an unknown job.
        for id in jobs.denied() {
            for (method, uri, body) in rp_job_paths(id) {
                let (s, b) = api_req(&fx.router, method, &uri, t, body).await;
                assert_eq!(
                    s,
                    StatusCode::NOT_FOUND,
                    "{method} {uri} must be denied: {b}"
                );
                assert_eq!(b["error"], json!(RP_JOB_NOT_FOUND), "{method} {uri}: {b}");
            }
        }

        // The allowed jobs: readable, and the mutating paths answer View-only
        // (403), proving the ACL saw `View` rather than `Deny`.
        for id in jobs.allowed() {
            let (s, b) = api_req(&fx.router, "GET", &format!("/api/jobs/{id}"), t, None).await;
            assert_eq!(s, StatusCode::OK, "{id}: {b}");
            let (s, b) = api_req(
                &fx.router,
                "GET",
                &format!("/api/jobs/{id}/artifacts"),
                t,
                None,
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{id}: {b}");
            for (method, uri, body) in rp_job_paths(id).into_iter().filter(|p| p.0 == "POST") {
                let (s, b) = api_req(&fx.router, method, &uri, t, body).await;
                assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}: {b}");
            }
        }

        // Lists, counts and dashboard stats agree, release-only task included.
        let (s, b) = api_req(&fx.router, "GET", "/api/jobs", t, None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let ids = rp_list_ids(&b);
        for id in jobs.allowed() {
            assert!(ids.contains(&id.to_string()), "{id} missing: {b}");
        }
        for id in jobs.denied() {
            assert!(!ids.contains(&id.to_string()), "{id} leaked: {b}");
        }
        assert_eq!(b["total"], json!(3), "{b}");
        let (_, b) = api_req(&fx.router, "GET", "/api/jobs?limit=1", t, None).await;
        assert_eq!(rp_list_ids(&b), vec![jobs.rel_public.to_string()], "{b}");
        assert_eq!(b["total"], json!(3), "{b}");
        let (_, b) = api_req(&fx.router, "GET", "/api/jobs?workspace=etl", t, None).await;
        assert_eq!(b["total"], json!(3), "{b}");
        let (_, b) = api_req(
            &fx.router,
            "GET",
            "/api/jobs?workspace=etl&task_name=nightly",
            t,
            None,
        )
        .await;
        assert_eq!(b["total"], json!(2), "{b}");
        let (_, b) = api_req(
            &fx.router,
            "GET",
            &format!("/api/jobs?workspace=etl&task_name={RP_RELEASE_ONLY_TASK}"),
            t,
            None,
        )
        .await;
        assert_eq!(rp_list_ids(&b), vec![jobs.rel_public.to_string()], "{b}");
        assert_eq!(b["total"], json!(1), "{b}");
        let (s, b) = api_req(&fx.router, "GET", "/api/stats", t, None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["completed"], json!(3), "{b}");

        // Worker detail hides the denied jobs' steps.
        let (s, b) = api_req(
            &fx.router,
            "GET",
            &format!("/api/workers/{worker}"),
            t,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let rows = rp_list_ids(&b["steps"]);
        assert_eq!(rows.len(), 3, "{b}");
        for id in jobs.denied() {
            assert!(!rows.contains(&id.to_string()), "{id} leaked: {b}");
        }

        // The admin still sees all five.
        let admin = fx.login(ADMIN).await;
        let a = Some(admin.as_str());
        let (_, b) = api_req(&fx.router, "GET", "/api/jobs", a, None).await;
        assert_eq!(b["total"], json!(5), "{b}");
        let (_, b) = api_req(&fx.router, "GET", "/api/stats", a, None).await;
        assert_eq!(b["completed"], json!(5), "{b}");

        // Re-run source: RUNNER may Run the live `nightly`, but a pinned
        // source is authorised FIRST by its own folder (§ 7.3, § 7.8); a
        // denied pinned source answers exactly like a missing source job
        // (404 `Source job not found`, spec 2026-10-06-json-input-type rev 10).
        sqlx::query("UPDATE job SET raw_input = '{}'::jsonb WHERE job_id = ANY($1)")
            .bind(vec![jobs.pin23, jobs.pin24])
            .execute(&fx.pool)
            .await?;
        let runner = fx.login(RUNNER).await;
        let r = Some(runner.as_str());
        let rerun = |src: Uuid| json!({"input": {}, "source_job_id": src});
        let (s, b) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/nightly/execute",
            r,
            Some(rerun(jobs.pin23)),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
        assert_eq!(b["error"], json!("Source job not found"), "{b}");
        let (s, b) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/nightly/execute",
            r,
            Some(rerun(jobs.pin24)),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn pinned_job_in_denied_folder_is_denied_on_the_websocket() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_acl_opts()).await?;
        let worker = rp_worker(&fx).await;
        let jobs = rp_seed_acl_jobs(&fx, worker).await;
        let token = fx.login(VIEWER).await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let router = fx.router.clone();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let url =
            |id: Uuid| format!("ws://127.0.0.1:{port}/api/jobs/{id}/logs/stream?token={token}");
        for id in jobs.denied() {
            match tokio_tungstenite::connect_async(url(id)).await {
                Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                    assert_eq!(resp.status(), 404, "{id}");
                    let body =
                        String::from_utf8_lossy(resp.body().as_deref().unwrap_or(&[])).into_owned();
                    assert!(body.contains(RP_JOB_NOT_FOUND), "{id}: {body}");
                }
                Err(e) => panic!("expected an HTTP 404 handshake rejection, got: {e}"),
                Ok(_) => panic!("denied job {id} must not upgrade"),
            }
        }
        for id in jobs.allowed() {
            let res = tokio_tungstenite::connect_async(url(id)).await;
            assert!(res.is_ok(), "{id}: {:?}", res.err());
        }

        server.abort();
        Ok(())
    })
    .await
}

#[tokio::test]
async fn pinned_job_in_denied_folder_is_denied_over_mcp_and_list_paginates_after_acl() -> Result<()>
{
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_acl_opts()).await?;
        let worker = rp_worker(&fx).await;
        let jobs = rp_seed_acl_jobs(&fx, worker).await;
        // `/mcp` rejects login JWTs (no audience); an API key authenticates.
        let viewer = fx.api_key(VIEWER, false).await;
        let t = Some(viewer.as_str());

        let call = |tool: &'static str, args: Value| mcp_call(&fx.router, t, tool, args);
        let deny_message = |resp: &Value| resp["error"]["message"].clone();

        for id in jobs.denied() {
            let job = json!({"job_id": id.to_string()});
            for tool in [
                "get_job_status",
                "get_job_logs",
                "list_artifacts",
                "cancel_job",
            ] {
                let resp = call(tool, job.clone()).await;
                assert_eq!(
                    deny_message(&resp),
                    json!(RP_JOB_NOT_FOUND),
                    "{tool} {id}: {resp}"
                );
            }
            let resp = call(
                "get_artifact",
                json!({"job_id": id.to_string(), "name": "out.txt"}),
            )
            .await;
            assert_eq!(
                deny_message(&resp),
                json!(RP_JOB_NOT_FOUND),
                "get_artifact {id}: {resp}"
            );
        }

        for id in jobs.allowed() {
            let job = json!({"job_id": id.to_string()});
            for tool in ["get_job_status", "get_job_logs", "list_artifacts"] {
                let resp = call(tool, job.clone()).await;
                assert!(!mcp_is_error(&resp), "{tool} {id}: {resp}");
            }
            // Past the job ACL: the artifact itself is what is missing.
            let resp = call(
                "get_artifact",
                json!({"job_id": id.to_string(), "name": "out.txt"}),
            )
            .await;
            assert_eq!(
                deny_message(&resp),
                json!("Artifact not found"),
                "{id}: {resp}"
            );
            let resp = call("cancel_job", job).await;
            assert_eq!(
                deny_message(&resp),
                json!("Insufficient permissions: cancel requires Run access"),
                "{id}: {resp}"
            );
        }

        let resp = call("list_jobs", json!({})).await;
        assert!(!mcp_is_error(&resp), "{resp}");
        let list = mcp_tool_json(&resp);
        assert_eq!(list["count"], json!(3), "{list}");
        let listed: Vec<Value> = list["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["job_id"].clone())
            .collect();
        for id in jobs.denied() {
            assert!(
                !listed.contains(&json!(id.to_string())),
                "{id} leaked: {list}"
            );
        }

        // The denied jobs are the most recent: filtering after LIMIT returns 0.
        let list = mcp_tool_json(&call("list_jobs", json!({"limit": 1})).await);
        assert_eq!(list["count"], json!(1), "{list}");
        assert_eq!(
            list["jobs"][0]["job_id"],
            json!(jobs.rel_public.to_string()),
            "{list}"
        );

        let list = mcp_tool_json(
            &call(
                "list_jobs",
                json!({"workspace": "etl", "task_name": RP_RELEASE_ONLY_TASK}),
            )
            .await,
        );
        assert_eq!(list["count"], json!(1), "{list}");
        assert_eq!(
            list["jobs"][0]["job_id"],
            json!(jobs.rel_public.to_string()),
            "{list}"
        );

        // An admin API key lists all five.
        let admin = fx.api_key(ADMIN, true).await;
        let resp = mcp_call(&fx.router, Some(admin.as_str()), "list_jobs", json!({})).await;
        assert_eq!(mcp_tool_json(&resp)["count"], json!(5), "{resp}");
        Ok(())
    })
    .await
}

// ── Task 19: state partitions follow the job ───────────────────────────

fn rp_state_opts() -> PinnedFixtureOpts {
    let mut o = rp_opts();
    o.state_storage = true;
    o
}

async fn rp_job_on_ref(fx: &PinnedFixture, git_ref: Option<&str>) -> Uuid {
    rp_seed_job(
        &fx.pool,
        RpJob {
            git_ref,
            status: "running",
            ..Default::default()
        },
    )
    .await
}

#[tokio::test]
async fn state_upload_uses_job_coordinates_not_the_path() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_state_opts()).await?;
        let job = rp_job_on_ref(&fx, Some("release/2.3")).await;

        // A cross-workspace step's worker sends the action OWNER's workspace
        // in the path; today that answered 400. The server writes the job's
        // own partition instead.
        let (status, _) = rp_worker_bytes(
            &fx.router,
            "POST",
            &format!("/worker/state/billing/nightly/{job}"),
            b"snap-23".to_vec(),
        )
        .await;
        assert_eq!(status, 201);

        let row = stroem_db::TaskStateRepo::get_latest_for_ref(
            &fx.pool,
            "etl",
            "nightly",
            Some("release/2.3"),
        )
        .await?
        .unwrap();
        assert_eq!(row.job_id, Some(job));
        assert!(stroem_db::TaskStateRepo::get_latest_for_ref(
            &fx.pool,
            "billing",
            "nightly",
            Some("release/2.3")
        )
        .await?
        .is_none());
        assert!(
            stroem_db::TaskStateRepo::get_latest(&fx.pool, "etl", "nightly")
                .await?
                .is_none()
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn state_download_with_job_id_reads_the_jobs_partition_only() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_state_opts()).await?;
        let on23 = rp_job_on_ref(&fx, Some("release/2.3")).await;
        let on24 = rp_job_on_ref(&fx, Some("release/2.4")).await;
        let unpinned = rp_job_on_ref(&fx, None).await;

        rp_worker_bytes(
            &fx.router,
            "POST",
            &format!("/worker/state/etl/nightly/{on23}"),
            b"snap-23".to_vec(),
        )
        .await;
        rp_worker_bytes(
            &fx.router,
            "POST",
            &format!("/worker/state/etl/nightly/{unpinned}"),
            b"snap-null".to_vec(),
        )
        .await;

        let (s, body) = rp_worker_bytes(
            &fx.router,
            "GET",
            &format!("/worker/state/billing/x?job_id={on23}"),
            vec![],
        )
        .await;
        assert_eq!(
            (s, body),
            (200, b"snap-23".to_vec()),
            "path is ignored when job_id is given"
        );
        let (s, _) = rp_worker_bytes(
            &fx.router,
            "GET",
            &format!("/worker/state/etl/nightly?job_id={on24}"),
            vec![],
        )
        .await;
        assert_eq!(s, 204, "release/2.4 has no state of its own");
        // No job_id (an old worker): today's path coordinates, NULL partition.
        let (s, body) =
            rp_worker_bytes(&fx.router, "GET", "/worker/state/etl/nightly", vec![]).await;
        assert_eq!((s, body), (200, b"snap-null".to_vec()));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn global_state_partitions_follow_the_job() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_state_opts()).await?;
        let on23 = rp_job_on_ref(&fx, Some("release/2.3")).await;
        let on24 = rp_job_on_ref(&fx, Some("release/2.4")).await;

        let (s, _) = rp_worker_bytes(
            &fx.router,
            "POST",
            &format!("/worker/global-state/billing/{on23}"),
            b"g-23".to_vec(),
        )
        .await;
        assert_eq!(s, 201);
        let (s, body) = rp_worker_bytes(
            &fx.router,
            "GET",
            &format!("/worker/global-state/billing?job_id={on23}"),
            vec![],
        )
        .await;
        assert_eq!((s, body), (200, b"g-23".to_vec()));
        let (s, _) = rp_worker_bytes(
            &fx.router,
            "GET",
            &format!("/worker/global-state/etl?job_id={on24}"),
            vec![],
        )
        .await;
        assert_eq!(s, 204);
        assert!(stroem_db::WorkspaceStateRepo::get_latest(&fx.pool, "etl")
            .await?
            .is_none());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn latest_snapshots_reads_the_given_ref_partition() -> Result<()> {
    rp_bounded(async {
        let fx = pinned_workspace_fixture(rp_state_opts()).await?;
        let on23 = rp_job_on_ref(&fx, Some("release/2.3")).await;
        let unpinned = rp_job_on_ref(&fx, None).await;
        rp_worker_bytes(
            &fx.router,
            "POST",
            &format!("/worker/state/etl/nightly/{on23}"),
            b"a".to_vec(),
        )
        .await;
        rp_worker_bytes(
            &fx.router,
            "POST",
            &format!("/worker/state/etl/nightly/{unpinned}"),
            b"b".to_vec(),
        )
        .await;

        use stroem_server::render_context::latest_snapshots;
        let s = latest_snapshots(&fx.pool, "etl", "nightly", Some("release/2.3"), "test").await;
        assert!(s.task.unwrap().storage_key.contains(&on23.to_string()));
        let s = latest_snapshots(&fx.pool, "etl", "nightly", None, "test").await;
        assert!(s.task.unwrap().storage_key.contains(&unpinned.to_string()));
        let s = latest_snapshots(&fx.pool, "etl", "nightly", Some("release/9.9"), "test").await;
        assert!(s.task.is_none());
        Ok(())
    })
    .await
}
