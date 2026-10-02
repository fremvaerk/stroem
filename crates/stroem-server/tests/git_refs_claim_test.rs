//! Claim-time pins, release, withholding and pinned tarballs (spec § 5.4,
//! § 7.2). Uses the shared git-refs fixture (`tests/common/pinned.rs`).

mod common;
use common::pinned::*;

use axum::http::StatusCode;
use serde_json::{json, Value};
use stroem_db::{ClaimIdentity, FailOutcome, JobRepo, JobStepRepo, ReleaseOutcome};
use uuid::Uuid;

// ─── Part D local helpers (R2: `claim_` prefix) ─────────────────────────

/// POST /worker/jobs/claim as a `script` worker; status + body (a failed
/// claim answers 422 `{"error": …}`, which `claim_once` cannot show).
async fn claim_status(router: &axum::Router, worker_id: &str) -> (StatusCode, Value) {
    worker_req(
        router,
        "POST",
        "/worker/jobs/claim",
        Some(json!({"worker_id": worker_id, "capabilities": ["script"]})),
    )
    .await
}

/// Execute `ws/task` with no input and return the new job id.
async fn claim_execute(router: &axum::Router, ws: &str, task: &str) -> Uuid {
    let (status, body) = execute_task(router, ws, task, json!({}), None).await;
    assert_eq!(status, StatusCode::OK, "execute {ws}/{task}: {body}");
    Uuid::parse_str(body["job_id"].as_str().unwrap()).unwrap()
}

async fn claim_ready_at(
    pool: &sqlx::PgPool,
    job_id: Uuid,
    step: &str,
) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar("SELECT ready_at FROM job_step WHERE job_id = $1 AND step_name = $2")
        .bind(job_id)
        .bind(step)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The DB clock (F42): `ready_at` / `retry_at` are written from it, so a
/// host clock skewed against the container must never be the yardstick.
async fn claim_db_now(pool: &sqlx::PgPool) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar("SELECT NOW()")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Make a released step claimable again at once (skip the 10 s wait).
async fn claim_clear_retry_at(pool: &sqlx::PgPool, job_id: Uuid) {
    sqlx::query("UPDATE job_step SET retry_at = NULL WHERE job_id = $1")
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
}

/// `etl` main for the claim tests: `a` runs `build` at `release/2.3` (a
/// pinned step); `b` is a docker step no `script`-only worker can claim, so
/// `a` is always the step a claim picks.
const CLAIM_MAIN_FLOW: &str = r#"
actions:
  build:
    type: script
    script: echo main-build
  build-local:
    type: script
    script: echo local
  remote-only:
    type: docker
    image: alpine:3
    cmd: echo b
tasks:
  pinned-pair:
    flow:
      a:
        action: build
        ref: release/2.3
      b:
        action: remote-only
  plain:
    flow:
      only:
        action: build-local
  live-foreign:
    flow:
      run:
        action: billing.export
"#;

const CLAIM_RELEASE_FLOW: &str = r#"
actions:
  build:
    type: script
    script: echo release-build
"#;

async fn claim_fixture() -> anyhow::Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(CLAIM_MAIN_FLOW.to_string()),
        etl_release: Some(CLAIM_RELEASE_FLOW.to_string()),
        ..Default::default()
    })
    .await
}

/// `etl` release/2.3 plus what the pinned-job claim tests need: a dotted
/// LOCAL action key (`common.x`, the shape of a library-flattened name) and
/// an agent step that names its own workspace (`etl.ask`, which inherits
/// the job pin — `action_ref` set without an explicit `ref:`).
fn claim_etl_release_extra() -> String {
    ETL_RELEASE
        .replace(
            "actions:\n  hello:\n",
            "actions:\n  common.x:\n    type: script\n    runner: local\n    \
             script: \"echo {{ input.greeting }}\"\n    input:\n      greeting:\n        \
             type: string\n        default: from-pinned-common\n  hello:\n",
        )
        .replace(
            "tasks:\n  nightly:\n",
            "tasks:\n  dotted:\n    flow:\n      run:\n        action: common.x\n  \
             agent-self:\n    flow:\n      think:\n        action: etl.ask\n  nightly:\n",
        )
}

async fn claim_pinned_fixture() -> anyhow::Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_release: Some(claim_etl_release_extra()),
        ..Default::default()
    })
    .await
}

// ─── Transient pin failure: release ─────────────────────────────────────

#[tokio::test]
async fn pin_unavailable_at_claim_releases_then_reclaims() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "pinned-pair").await;

    // A cold replica (its PinStore never saw release/2.3) while git is down.
    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let worker = register_worker(&replica.router, &["script"]).await;
    let before = claim_db_now(&fx.pool).await;

    let (status, body) = claim_status(&replica.router, &worker).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["job_id"].is_null(),
        "a released claim answers no work: {body}"
    );

    let a = JobStepRepo::get_step(&fx.pool, job_id, "a").await?.unwrap();
    assert_eq!(a.status, "ready");
    assert_eq!(a.pin_releases, 1);
    assert_eq!(a.retry_attempt, 0, "a release is not a failure");
    assert_eq!(a.retry_history, json!([]), "nor a retry attempt");
    assert!(a.worker_id.is_none() && a.started_at.is_none());
    let retry_in = (a.retry_at.unwrap() - claim_db_now(&fx.pool).await).num_seconds();
    assert!(
        (5..=10).contains(&retry_in),
        "retry_at ≈ now + 10 s, got {retry_in}"
    );
    assert!(
        claim_ready_at(&fx.pool, job_id, "a").await >= before,
        "release resets ready_at so the unmatched sweep does not count the claim"
    );
    let log = job_log_text(&fx.pool, &replica.state, job_id).await;
    assert!(
        log.contains("[pin] etl@release/2.3") && log.contains("not available yet"),
        "{log}"
    );

    // Git is back; skip the 10 s wait. A SECOND worker picks the step up.
    fx.etl.restore_remote();
    claim_clear_retry_at(&fx.pool, job_id).await;
    let second = register_worker(&replica.router, &["script"]).await;
    let (status, body) = claim_status(&replica.router, &second).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["step_name"], "a");
    assert_eq!(body["workspace"], "etl");
    assert_eq!(body["revision"], fx.commits.etl_release.as_str());
    assert_eq!(body["action_spec"]["script"], "echo release-build");
    let a = JobStepRepo::get_step(&fx.pool, job_id, "a").await?.unwrap();
    assert_eq!(a.status, "running");
    assert_eq!(a.worker_id.map(|w| w.to_string()), Some(second));
    assert_eq!(a.pin_releases, 1, "a successful claim does not count");
    Ok(())
}

#[tokio::test]
async fn pin_release_cap_fails_the_step() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "pinned-pair").await;
    sqlx::query("UPDATE job_step SET pin_releases = 30 WHERE job_id = $1 AND step_name = 'a'")
        .bind(job_id)
        .execute(&fx.pool)
        .await?;

    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let worker = register_worker(&replica.router, &["script"]).await;
    let (status, body) = claim_status(&replica.router, &worker).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("still unavailable after 30 attempts"),
        "{body}"
    );
    let a = JobStepRepo::get_step(&fx.pool, job_id, "a").await?.unwrap();
    assert_eq!(a.status, "failed");
    assert_eq!(
        a.pin_releases, 30,
        "the cap does not count one more release"
    );
    Ok(())
}

#[tokio::test]
async fn pin_release_after_cancel_settles_step_and_siblings() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "pinned-pair").await;

    // Only the FIRST of cancellation's two calls has run: the job row is
    // terminal, the steps are untouched (`JobRepo::cancel`, then
    // `cancel_pending_steps`, two calls in `Settlement::cancel`).
    assert!(JobRepo::cancel(&fx.pool, job_id).await?);

    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let worker = register_worker(&replica.router, &["script"]).await;
    let (status, body) = claim_status(&replica.router, &worker).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["job_id"].is_null(), "{body}");

    for s in JobStepRepo::get_steps_for_job(&fx.pool, job_id).await? {
        assert_eq!(
            s.status, "cancelled",
            "step {} left {}",
            s.step_name, s.status
        );
    }
    let job = JobRepo::get(&fx.pool, job_id).await?.unwrap();
    assert_eq!(job.status, "cancelled");
    let claimed: bool =
        sqlx::query_scalar("SELECT metrics_recorded_at IS NOT NULL FROM job WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&fx.pool)
            .await?;
    assert!(
        claimed,
        "the step's settlement ran terminal handling (the claim was taken)"
    );
    Ok(())
}

#[tokio::test]
async fn pin_release_before_cancel_leaves_nothing_claimable() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "pinned-pair").await;

    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let worker = register_worker(&replica.router, &["script"]).await;
    let (_, body) = claim_status(&replica.router, &worker).await;
    assert!(body["job_id"].is_null(), "{body}");
    assert_eq!(
        JobStepRepo::get_step(&fx.pool, job_id, "a")
            .await?
            .unwrap()
            .status,
        "ready"
    );

    fx.state.settlement().cancel(job_id).await?;
    for s in JobStepRepo::get_steps_for_job(&fx.pool, job_id).await? {
        assert_eq!(
            s.status, "cancelled",
            "step {} left {}",
            s.step_name, s.status
        );
    }

    // F28: with git back and no retry delay, a released-but-uncancelled step
    // would be claimed with work here. Nothing is.
    fx.etl.restore_remote();
    claim_clear_retry_at(&fx.pool, job_id).await;
    let (status, body) = claim_status(&replica.router, &worker).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["job_id"].is_null(),
        "a cancelled job has no work: {body}"
    );
    Ok(())
}

/// F25: a pinned job's OWN pin (no `action_workspace`) that is unavailable
/// on this replica releases the claim, like a step pin does.
#[tokio::test]
async fn pinned_job_pin_unavailable_releases_a_local_step() -> anyhow::Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = fx.create_pinned_etl_job("nightly").await?;
    let a = JobStepRepo::get_step(&fx.pool, job_id, "a").await?.unwrap();
    assert_eq!(a.status, "ready");
    assert!(
        a.action_workspace.is_none(),
        "a local step of the pinned job"
    );

    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let worker = register_worker(&replica.router, &["script"]).await;
    let (status, body) = claim_status(&replica.router, &worker).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["job_id"].is_null(), "{body}");

    let a = JobStepRepo::get_step(&fx.pool, job_id, "a").await?.unwrap();
    assert_eq!(a.status, "ready");
    assert_eq!(a.pin_releases, 1);
    assert_eq!(a.retry_attempt, 0);
    assert_eq!(
        JobRepo::get(&fx.pool, job_id).await?.unwrap().status,
        "pending",
        "the job is untouched"
    );
    let log = job_log_text(&fx.pool, &replica.state, job_id).await;
    let short = &fx.commits.etl_release[..7];
    assert!(
        log.contains(&format!(
            "[pin] etl@release/2.3 ({short}) not available yet"
        )),
        "{log}"
    );

    fx.etl.restore_remote();
    claim_clear_retry_at(&fx.pool, job_id).await;
    let (status, body) = claim_status(&replica.router, &worker).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["step_name"], "a");
    assert_eq!(body["workspace"], "etl");
    assert_eq!(body["revision"], fx.commits.etl_release.as_str());
    assert_eq!(body["action_spec"]["script"], "echo v1");
    Ok(())
}

// ─── The claim identity guards every later write ────────────────────────

#[tokio::test]
async fn stale_claim_identity_does_not_fail_a_released_or_reclaimed_step() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "plain").await;
    let worker = register_worker(&fx.router, &["script"]).await;
    let (_, body) = claim_status(&fx.router, &worker).await;
    assert_eq!(body["step_name"], "only");

    let row = JobStepRepo::get_step(&fx.pool, job_id, "only")
        .await?
        .unwrap();
    let old = ClaimIdentity {
        worker_id: row.worker_id.unwrap(),
        started_at: row.started_at.unwrap(),
    };
    let released = JobStepRepo::release_claim(
        &fx.pool,
        job_id,
        "only",
        old,
        chrono::Duration::seconds(10),
        30,
    )
    .await?;
    assert_eq!(released, ReleaseOutcome::Released);

    // A recovery sweep that selected the OLD claim must not fail the released row.
    let out = fx
        .state
        .settlement()
        .claimed_step_failed(job_id, "only", "Worker heartbeat timeout", Some(old))
        .await?;
    assert_eq!(out, FailOutcome::NotApplied);
    assert_eq!(
        JobStepRepo::get_step(&fx.pool, job_id, "only")
            .await?
            .unwrap()
            .status,
        "ready"
    );

    // Nor the same step reclaimed (new started_at).
    claim_clear_retry_at(&fx.pool, job_id).await;
    let (_, body) = claim_status(&fx.router, &worker).await;
    assert_eq!(body["step_name"], "only");
    let out = fx
        .state
        .settlement()
        .claimed_step_failed(job_id, "only", "Step timed out", Some(old))
        .await?;
    assert_eq!(out, FailOutcome::NotApplied);
    let row = JobStepRepo::get_step(&fx.pool, job_id, "only")
        .await?
        .unwrap();
    assert_eq!(row.status, "running");

    // The CURRENT claim does fail it.
    let current = ClaimIdentity {
        worker_id: row.worker_id.unwrap(),
        started_at: row.started_at.unwrap(),
    };
    let out = fx
        .state
        .settlement()
        .claimed_step_failed(job_id, "only", "Step timed out", Some(current))
        .await?;
    assert!(matches!(out, FailOutcome::Failed { .. }), "{out:?}");
    Ok(())
}

/// F25: recovery phases 1 and 2, driven through `sweep_once`, fail a running
/// step through the claim identity they selected — the identity read back
/// from the DB must match the row exactly (timestamp precision included).
#[tokio::test]
async fn recovery_sweep_fails_running_steps_through_their_claim() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;

    // Phase 2: a step past its timeout.
    let timed_out = claim_execute(&fx.router, "etl", "plain").await;
    let worker = register_worker(&fx.router, &["script"]).await;
    let (_, body) = claim_status(&fx.router, &worker).await;
    assert_eq!(body["job_id"], timed_out.to_string());
    sqlx::query(
        "UPDATE job_step SET timeout_secs = 1, started_at = started_at - INTERVAL '1 hour' \
         WHERE job_id = $1",
    )
    .bind(timed_out)
    .execute(&fx.pool)
    .await?;
    stroem_server::recovery::sweep_once(&fx.state).await?;
    let only = JobStepRepo::get_step(&fx.pool, timed_out, "only")
        .await?
        .unwrap();
    assert_eq!(only.status, "failed");
    assert!(
        only.error_message.unwrap_or_default().contains("timed out"),
        "phase 2 failed it"
    );

    // Phase 1: the claiming worker stopped heartbeating.
    let stale = claim_execute(&fx.router, "etl", "plain").await;
    let gone = register_worker(&fx.router, &["script"]).await;
    let (_, body) = claim_status(&fx.router, &gone).await;
    assert_eq!(body["job_id"], stale.to_string());
    sqlx::query(
        "UPDATE worker SET last_heartbeat = NOW() - INTERVAL '1 hour' WHERE worker_id = $1",
    )
    .bind(Uuid::parse_str(&gone)?)
    .execute(&fx.pool)
    .await?;
    stroem_server::recovery::sweep_once(&fx.state).await?;
    let only = JobStepRepo::get_step(&fx.pool, stale, "only")
        .await?
        .unwrap();
    assert_eq!(only.status, "failed");
    assert!(
        only.error_message
            .unwrap_or_default()
            .contains("heartbeat timeout"),
        "phase 1 failed it"
    );
    Ok(())
}

// ─── Permanent pin failure: fail ────────────────────────────────────────

/// Review Focus 3: the pinned commit was force-pushed away and is gone from
/// the remote. A cold replica cannot fetch it, and that is PERMANENT
/// (`CommitNotFound`): the step fails at once, with no release loop.
#[tokio::test]
async fn pin_commit_gone_from_remote_fails_permanently_on_a_cold_replica() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "pinned-pair").await;
    let gone = fx.commits.etl_release.clone();
    let a = JobStepRepo::get_step(&fx.pool, job_id, "a").await?.unwrap();
    assert_eq!(a.action_revision.as_deref(), Some(gone.as_str()));

    // Force-push the branch away, then make the old commit truly absent.
    // git2 writes loose objects; nothing packs the fixture's bare repo, so
    // the commit object is `objects/xx/<38 hex>`. Its tree and blobs may be
    // shared with other commits — removing the commit object alone is enough.
    fx.etl.force_branch("release/2.3", &fx.commits.etl_main);
    let loose = fx
        .etl
        .path
        .join("objects")
        .join(&gone[..2])
        .join(&gone[2..]);
    assert!(
        loose.is_file(),
        "fixture commits are loose objects; found none at {}",
        loose.display()
    );
    std::fs::remove_file(&loose)?;

    let replica = fx.second_replica().await?;
    let worker = register_worker(&replica.router, &["script"]).await;
    let (status, body) = claim_status(&replica.router, &worker).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a gone commit is permanent, not \"no work\": {body}"
    );
    let a = JobStepRepo::get_step(&fx.pool, job_id, "a").await?.unwrap();
    assert_eq!(a.status, "failed");
    assert_eq!(a.pin_releases, 0, "no release for a permanent pin error");
    let error = a.error_message.unwrap_or_default();
    assert!(
        error.contains("cannot be loaded") && error.contains(&gone[..7]),
        "{error}"
    );
    Ok(())
}

/// Review Focus 4: a pinned step whose owner workspace is no longer in the
/// server config. `config_for` → `PinStore::ensure` reports the unknown
/// workspace as `NotGit` (it is not a pin source); whatever the variant, it
/// must be permanent: the step fails, nothing is released.
#[tokio::test]
async fn pin_owner_unconfigured_fails_permanently() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "plain").await;
    // Stamp the ready step as if `ghost`@main had been pinned at creation and
    // `ghost` was then removed from the server config.
    sqlx::query(
        "UPDATE job_step SET action_workspace = 'ghost', action_ref = 'main', \
         action_revision = $2 WHERE job_id = $1 AND step_name = 'only'",
    )
    .bind(job_id)
    .bind("b".repeat(40))
    .execute(&fx.pool)
    .await?;

    let worker = register_worker(&fx.router, &["script"]).await;
    let (status, body) = claim_status(&fx.router, &worker).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let only = JobStepRepo::get_step(&fx.pool, job_id, "only")
        .await?
        .unwrap();
    assert_eq!(only.status, "failed");
    assert_eq!(only.pin_releases, 0, "an unknown owner is not retried");
    let error = only.error_message.unwrap_or_default();
    assert!(
        error.contains("[pin] ghost@main") && error.contains("cannot be loaded"),
        "{error}"
    );

    // No release loop: nothing is left to claim.
    let (_, body) = claim_status(&fx.router, &worker).await;
    assert!(body["job_id"].is_null(), "{body}");
    Ok(())
}

/// A commit whose config does not load: its connection renders an undefined
/// secret, so the loader itself fails. The loader chain can quote secret
/// values, so only the fixed sentence is shown (T6 review #9).
const CLAIM_BROKEN: &str = r#"
secrets:
  token: s3cr3t-token-value
connections:
  db:
    host: "{{ secret.token }}-{{ secret.nope }}"
actions:
  build-local:
    type: script
    runner: local
    script: echo broken
"#;

#[tokio::test]
async fn pin_load_failure_at_claim_shows_only_the_fixed_sentence() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let broken = fx
        .etl
        .commit("broken", "main", &[("workflow.yaml", CLAIM_BROKEN)]);
    let job_id = claim_execute(&fx.router, "etl", "plain").await;
    // A step stamped at a commit that no longer loads (it did not at
    // creation either, so it is stamped by hand).
    sqlx::query(
        "UPDATE job_step SET action_workspace = 'etl', action_ref = 'broken', \
         action_revision = $2 WHERE job_id = $1 AND step_name = 'only'",
    )
    .bind(job_id)
    .bind(&broken)
    .execute(&fx.pool)
    .await?;

    let worker = register_worker(&fx.router, &["script"]).await;
    let (status, body) = claim_status(&fx.router, &worker).await;
    let sentence = format!(
        "[pin] etl@broken ({}) cannot be loaded: its configuration does not load",
        &broken[..7]
    );
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], sentence.as_str());
    let only = JobStepRepo::get_step(&fx.pool, job_id, "only")
        .await?
        .unwrap();
    assert_eq!(only.status, "failed");
    assert_eq!(only.pin_releases, 0);
    assert_eq!(only.error_message.as_deref(), Some(sentence.as_str()));
    let log = job_log_text(&fx.pool, &fx.state, job_id).await;
    assert!(log.contains(&sentence), "{log}");
    assert!(
        !log.contains("s3cr3t") && !log.contains("secret.nope"),
        "{log}"
    );
    Ok(())
}

/// F47: a live cross-workspace step whose owner is unavailable at claim
/// fails before ANY action lookup — never a fall-through to the caller's
/// config by the step's full name.
#[tokio::test]
async fn unavailable_live_owner_fails_before_any_caller_lookup() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "live-foreign").await;
    let run = JobStepRepo::get_step(&fx.pool, job_id, "run")
        .await?
        .unwrap();
    assert_eq!(run.action_workspace.as_deref(), Some("billing"));
    assert!(run.action_ref.is_none(), "a live (unpinned) owner");

    fx.mgr().mark_unavailable_for_test("billing");
    let worker = register_worker(&fx.router, &["script"]).await;
    let (status, body) = claim_status(&fx.router, &worker).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("action owner: workspace 'billing' is not available"),
        "{body}"
    );
    let run = JobStepRepo::get_step(&fx.pool, job_id, "run")
        .await?
        .unwrap();
    assert_eq!(run.status, "failed");
    assert_eq!(run.pin_releases, 0, "a live owner is not a pin");
    Ok(())
}

// ─── Pinned configs at claim (§ 7.2 table) ──────────────────────────────

/// F25: a ref'd step's connection-typed input resolves against its pinned
/// owner: `release-db` exists only at etl@release/2.3, not in the live
/// config the (unpinned) job runs in.
#[tokio::test]
async fn ref_step_claim_resolves_the_pinned_owners_connections() -> anyhow::Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = claim_execute(&fx.router, "etl", "uses-release-db").await;
    let run = JobStepRepo::get_step(&fx.pool, job_id, "run")
        .await?
        .unwrap();
    assert_eq!(run.action_ref.as_deref(), Some("release/2.3"));

    let worker = register_worker(&fx.router, &["script"]).await;
    let (status, body) = claim_status(&fx.router, &worker).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["step_name"], "run");
    assert_eq!(body["revision"], fx.commits.etl_release.as_str());
    assert_eq!(
        body["input"]["db"]["host"], "release-host",
        "the connection resolves from the pinned owner: {body}"
    );
    Ok(())
}

/// F25 + the T9 hand-off (F36): agent steps of a pinned job claim against
/// the PINNED config — `sub` (the task tool) exists only at release/2.3. An
/// inherited self-qualified agent (`etl.ask`: `action_ref` set, never
/// written as `ref:`) is not "explicitly ref'd" and claims the same way.
#[tokio::test]
async fn agent_steps_in_a_pinned_job_claim_against_the_pinned_config() -> anyhow::Result<()> {
    let fx = claim_pinned_fixture().await?;
    let worker = register_worker(&fx.router, &["agent"]).await;

    for (task, expect_action_workspace) in [("agent-wrap", None), ("agent-self", Some("etl"))] {
        let job_id = fx.create_pinned_etl_job(task).await?;
        let think = JobStepRepo::get_step(&fx.pool, job_id, "think")
            .await?
            .unwrap();
        assert_eq!(think.action_workspace.as_deref(), expect_action_workspace);

        let body = claim_once(&fx.router, &worker).await;
        assert_eq!(body["job_id"], job_id.to_string(), "{task}: {body}");
        assert_eq!(body["step_name"], "think", "{task}");
        assert_eq!(body["workspace"], "etl", "{task}");
        assert_eq!(body["revision"], fx.commits.etl_release.as_str(), "{task}");
        assert_eq!(body["agent_provider_name"], "anthropic", "{task}");
        assert!(
            body["agent_tool_tasks"]["sub"].is_object(),
            "{task}: task tools come from the pinned config: {body}"
        );
    }
    Ok(())
}

/// The T9 review (spec § 14 prefix strip): a dotted LOCAL key (`common.x`,
/// shaped like a library-flattened name) in a pinned job keeps
/// `action_workspace` NULL and is looked up at claim by its FULL key in the
/// pinned config — its input default is merged and its body rendered.
#[tokio::test]
async fn dotted_local_action_in_a_pinned_job_claims_by_full_key() -> anyhow::Result<()> {
    let fx = claim_pinned_fixture().await?;
    let job_id = fx.create_pinned_etl_job("dotted").await?;
    let run = JobStepRepo::get_step(&fx.pool, job_id, "run")
        .await?
        .unwrap();
    assert!(run.action_workspace.is_none(), "{:?}", run.action_workspace);
    assert!(run.action_ref.is_none());
    assert_eq!(run.action_name, "common.x");

    let worker = register_worker(&fx.router, &["script"]).await;
    let (status, body) = claim_status(&fx.router, &worker).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["step_name"], "run");
    assert_eq!(body["workspace"], "etl");
    assert_eq!(body["revision"], fx.commits.etl_release.as_str());
    assert_eq!(body["input"]["greeting"], "from-pinned-common", "{body}");
    assert_eq!(body["action_spec"]["script"], "echo from-pinned-common");
    Ok(())
}

/// Fix round 1, finding 2: a claim-decided failure (recovery phases 1 and 2,
/// `fail_claimed_step`) never overwrites a step the worker FINISHED after
/// the claim was observed — a completed row keeps the very same
/// `(worker_id, started_at)`, so only the `running` requirement stops it.
#[tokio::test]
async fn claimed_failure_never_overwrites_a_completed_step() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "plain").await;
    let worker = register_worker(&fx.router, &["script"]).await;
    let (_, body) = claim_status(&fx.router, &worker).await;
    assert_eq!(body["step_name"], "only");
    let row = JobStepRepo::get_step(&fx.pool, job_id, "only")
        .await?
        .unwrap();
    let selected = ClaimIdentity {
        worker_id: row.worker_id.unwrap(),
        started_at: row.started_at.unwrap(),
    };

    // The worker reports success after a sweep selected the claim.
    let (status, body) = worker_req(
        &fx.router,
        "POST",
        &format!("/worker/jobs/{job_id}/steps/only/complete"),
        Some(json!({"exit_code": 0, "output": {"ok": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let done = JobStepRepo::get_step(&fx.pool, job_id, "only")
        .await?
        .unwrap();
    assert_eq!(done.status, "completed");
    assert_eq!(
        (done.worker_id, done.started_at),
        (Some(selected.worker_id), Some(selected.started_at))
    );

    let out = fx
        .state
        .settlement()
        .claimed_step_failed(job_id, "only", "Worker heartbeat timeout", Some(selected))
        .await?;
    assert_eq!(out, FailOutcome::NotApplied);
    let after = JobStepRepo::get_step(&fx.pool, job_id, "only")
        .await?
        .unwrap();
    assert_eq!(after.status, "completed");
    assert_eq!(after.error_message, None);
    assert_eq!(
        JobRepo::get(&fx.pool, job_id).await?.unwrap().status,
        "completed"
    );
    Ok(())
}

// ─── Fix round 1, finding 1: bounded, disconnect-proof pin loads ────────

/// A replica whose PinStore already holds etl@release/2.3's CHECKOUT, so the
/// only blocking pin work left for a claim is the config load — the unit a
/// `hold_loads_for_test` then parks.
async fn claim_replica_with_warm_tree(fx: &PinnedFixture) -> anyhow::Result<Replica> {
    let replica = fx.second_replica().await?;
    let pins = replica.state.workspaces.pins();
    pins.ensure_tree("etl", &fx.commits.etl_release).await?;
    assert_eq!(pins.load_count(), 0, "no config load yet");
    Ok(replica)
}

/// A pin load that outlasts the claim budget (below the worker's request
/// timeout) releases the claim at the budget instead of answering after
/// the worker gave up; the load itself goes on and warms the cache.
#[tokio::test]
async fn pin_load_past_the_claim_budget_releases_and_keeps_loading() -> anyhow::Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(CLAIM_MAIN_FLOW.to_string()),
        etl_release: Some(CLAIM_RELEASE_FLOW.to_string()),
        claim_load_budget_secs: Some(1),
        ..Default::default()
    })
    .await?;
    let job_id = claim_execute(&fx.router, "etl", "pinned-pair").await;
    let replica = claim_replica_with_warm_tree(&fx).await?;
    let pins = replica.state.workspaces.pins();
    let worker = register_worker(&replica.router, &["script"]).await;

    let hold = pins.hold_loads_for_test();
    let started = std::time::Instant::now();
    // Bounded here too: an unbounded claim would wait on the held load
    // forever (the hold opens on unwind, so the parked load never leaks).
    let (status, body) = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        claim_status(&replica.router, &worker),
    )
    .await
    .expect("the claim answers within its budget, whatever the pin load does");
    let took = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["job_id"].is_null(), "released at the budget: {body}");
    assert!(
        took >= std::time::Duration::from_secs(1) && took < std::time::Duration::from_secs(10),
        "answered at the 1 s budget, not when the load finished: {took:?}"
    );
    assert_eq!(hold.entered(), 1, "the config load was held, not failed");

    let a = JobStepRepo::get_step(&fx.pool, job_id, "a").await?.unwrap();
    assert_eq!(a.status, "ready");
    assert_eq!(a.pin_releases, 1);
    assert_eq!(a.retry_attempt, 0);
    assert!(a.retry_at.is_some(), "released with a retry delay");
    assert!(a.worker_id.is_none() && a.started_at.is_none());
    let log = job_log_text(&fx.pool, &replica.state, job_id).await;
    assert!(
        log.contains("[pin] etl@release/2.3 (")
            && log.contains("not available yet on this server")
            && log.contains("claim budget"),
        "{log}"
    );

    // The load the claim stopped waiting for finishes and fills the cache:
    // the next ensure joins or reads it, with no second config load.
    drop(hold);
    pins.ensure("etl", &fx.commits.etl_release).await?;
    assert_eq!(
        pins.load_count(),
        1,
        "the claim's load went on in the background"
    );
    claim_clear_retry_at(&fx.pool, job_id).await;
    let (status, body) = claim_status(&replica.router, &worker).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["step_name"], "a");
    assert_eq!(body["action_spec"]["script"], "echo release-build");
    Ok(())
}

/// The worker hangs up while the claim waits for a pin (the handler is
/// dropped). The selection runs in its own task: once the config loads with
/// nobody left to answer, it releases the claim instead of leaving the step
/// `running` under a live worker that never got it.
#[tokio::test]
async fn claim_dropped_mid_pin_load_releases_the_step() -> anyhow::Result<()> {
    let fx = claim_fixture().await?;
    let job_id = claim_execute(&fx.router, "etl", "pinned-pair").await;
    let replica = claim_replica_with_warm_tree(&fx).await?;
    let pins = replica.state.workspaces.pins();
    let worker = register_worker(&replica.router, &["script"]).await;

    let hold = pins.hold_loads_for_test();
    {
        let mut claim = Box::pin(claim_status(&replica.router, &worker));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                answered = &mut claim => panic!("answered while the load was held: {answered:?}"),
                () = async {
                    while hold.entered() == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                } => {}
            }
        })
        .await
        .expect("the claim's config load reaches the gate");
        // `claim` drops here: the worker's request is gone mid-load.
    }
    assert_eq!(
        JobStepRepo::get_step(&fx.pool, job_id, "a")
            .await?
            .unwrap()
            .status,
        "running",
        "claimed, and nobody answered yet"
    );

    drop(hold);
    let released = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let a = JobStepRepo::get_step(&fx.pool, job_id, "a")
                .await
                .unwrap()
                .unwrap();
            if a.status != "running" {
                return a;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the unanswered claim is released");
    assert_eq!(released.status, "ready");
    assert_eq!(released.pin_releases, 1);
    assert_eq!(released.retry_attempt, 0, "a release, not a failure");
    assert!(released.worker_id.is_none());
    let log = job_log_text(&fx.pool, &replica.state, job_id).await;
    assert!(log.contains("stopped waiting"), "{log}");
    Ok(())
}

// ─── Owner-side render errors are withheld at claim (§ 7.2, § 9) ────────

/// `billing` main (live): an action whose BODY wraps an owner secret in a
/// filter chain that fails, and a plain `export` for the caller-input case.
/// The `"` in the secret makes `json_encode | round` quote a doubly
/// JSON-escaped form that no scrub matches.
const WH_BILLING_MAIN: &str = r#"
secrets:
  TOKEN: 'live"zq7-owner-secret'
actions:
  export:
    type: script
    script: "echo {{ input.token }}"
    input:
      token:
        type: string
  export-body:
    type: script
    script: "echo {{ secret.TOKEN | json_encode | round }}"
"#;

/// `billing` at tag `v4.1.0`: `TOKEN` has a value that exists ONLY at this
/// commit (so only the pin's redaction values know it), wrapped by a failing
/// filter chain in an input DEFAULT; and an unshared connection a caller may
/// not name.
const WH_BILLING_TAGGED: &str = r#"
secrets:
  TOKEN: 'tag"only-zq9-secret'
connection_types:
  pg:
    host:
      type: string
connections:
  private-db:
    type: pg
    host: private-host
actions:
  export:
    type: script
    script: "echo {{ input.token }}"
    input:
      token:
        type: string
        default: "{{ secret.TOKEN | json_encode | round }}"
  export-db:
    type: script
    script: echo db
    input:
      db:
        type: pg
"#;

/// `etl` main: callers of the foreign actions (pinned and live), two
/// caller-side errors on a foreign step, and an own-workspace ref.
const WH_ETL_MAIN: &str = r#"
secrets:
  CALLER_TOKEN: caller-secret-value
  TOKEN2: own-secret-value
tasks:
  call-foreign-default:
    flow:
      s:
        action: billing.export
        ref: v4.1.0
  call-foreign-body:
    flow:
      s:
        action: billing.export-body
  bad-caller-input:
    flow:
      s:
        action: billing.export
        input:
          token: "{{ secret.CALLER_TOKEN | round }}"
  bad-caller-connection:
    flow:
      s:
        action: billing.export-db
        ref: v4.1.0
        input:
          db: "{{ 'private-db' }}"
  call-own-ref:
    flow:
      s:
        action: export-local
        ref: release/2.3
"#;

/// `etl` at `release/2.3`: a default that fails on an own secret. `TOKEN2`
/// is defined at both commits, so the test does not depend on which config
/// the owner role resolves against.
const WH_ETL_RELEASE: &str = r#"
secrets:
  TOKEN2: own-secret-value
actions:
  export-local:
    type: script
    script: "echo {{ input.v }}"
    input:
      v:
        type: string
        default: "{{ secret.TOKEN2 | round }}"
"#;

async fn claim_withholding_fixture() -> anyhow::Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(WH_ETL_MAIN.to_string()),
        etl_release: Some(WH_ETL_RELEASE.to_string()),
        billing_main: Some(WH_BILLING_MAIN.to_string()),
        billing_tagged: Some(WH_BILLING_TAGGED.to_string()),
        ..Default::default()
    })
    .await
}

/// Execute `etl/task` and claim its only step `s`, which must fail at claim:
/// (status, body, the step's `error_message`, the job log). Bounded, so a
/// hang fails the test (and drops its container) instead of stalling it.
async fn claim_failure(fx: &PinnedFixture, task: &str) -> (StatusCode, Value, String, String) {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let job_id = claim_execute(&fx.router, "etl", task).await;
        let worker = register_worker(&fx.router, &["script"]).await;
        let (status, body) = claim_status(&fx.router, &worker).await;
        let step = JobStepRepo::get_step(&fx.pool, job_id, "s")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(step.status, "failed", "{task}: {body}");
        let log = job_log_text(&fx.pool, &fx.state, job_id).await;
        (status, body, step.error_message.unwrap_or_default(), log)
    })
    .await
    .expect("execute + claim answer within 60 s")
}

fn claim_assert_no_secret(text: &str, fragment: &str) {
    assert!(!text.contains(fragment), "secret leaked: {text}");
}

/// The owner's input default at a ref fails on a secret that exists only at
/// that commit: the fixed sentence everywhere, nothing of the value.
#[tokio::test]
async fn foreign_owner_default_render_error_is_withheld_at_claim() -> anyhow::Result<()> {
    let fx = claim_withholding_fixture().await?;
    let expected = "rendering action 'export' of workspace 'billing' failed; details withheld";
    let (status, body, error, log) = claim_failure(&fx, "call-foreign-default").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], expected);
    assert_eq!(error, expected);
    assert!(log.contains(expected), "{log}");
    for text in [body.to_string(), error, log] {
        claim_assert_no_secret(&text, "zq9");
    }
    Ok(())
}

/// The owner's action BODY (live cross-workspace action, no ref) fails on an
/// owner secret: withheld the same way.
#[tokio::test]
async fn foreign_owner_body_render_error_is_withheld_at_claim() -> anyhow::Result<()> {
    let fx = claim_withholding_fixture().await?;
    let expected = "rendering action 'export-body' of workspace 'billing' failed; details withheld";
    let (status, body, error, log) = claim_failure(&fx, "call-foreign-body").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], expected);
    assert_eq!(error, expected);
    assert!(log.contains(expected), "{log}");
    for text in [body.to_string(), error, log] {
        claim_assert_no_secret(&text, "zq7");
    }
    Ok(())
}

/// The caller's own step `input:` on a FOREIGN step is the caller's template
/// in the caller's context: visible, scrubbed.
#[tokio::test]
async fn caller_input_render_error_stays_visible_and_scrubbed() -> anyhow::Result<()> {
    let fx = claim_withholding_fixture().await?;
    let (status, body, error, log) = claim_failure(&fx, "bad-caller-input").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        error.contains("Failed to render step input template"),
        "{error}"
    );
    assert!(error.contains("round"), "{error}");
    assert!(!error.contains("details withheld"), "{error}");
    assert_eq!(body["error"], error.as_str());
    for text in [body.to_string(), error, log] {
        claim_assert_no_secret(&text, "caller-secret-value");
    }
    Ok(())
}

/// A Caller-bucket `prepare_step_action_input` error on a foreign step at a
/// ref — the caller names the owner's UNSHARED connection — is the caller's
/// own mistake: visible.
#[tokio::test]
async fn cross_owner_caller_connection_error_stays_visible() -> anyhow::Result<()> {
    let fx = claim_withholding_fixture().await?;
    let (status, body, error, _log) = claim_failure(&fx, "bad-caller-connection").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        error.contains("'billing.private-db' exists but is not shared"),
        "{error}"
    );
    assert!(!error.contains("details withheld"), "{error}");
    assert_eq!(body["error"], error.as_str());
    Ok(())
}

/// An own-workspace ref crosses no boundary: the owner-side error stays
/// visible, scrubbed.
#[tokio::test]
async fn own_workspace_ref_render_error_stays_visible_and_scrubbed() -> anyhow::Result<()> {
    let fx = claim_withholding_fixture().await?;
    let (status, body, error, log) = claim_failure(&fx, "call-own-ref").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        error.contains("Failed to merge action input defaults"),
        "{error}"
    );
    assert!(!error.contains("details withheld"), "{error}");
    for text in [body.to_string(), error, log] {
        claim_assert_no_secret(&text, "own-secret-value");
    }
    Ok(())
}

// ─── Task 14: pinned tarballs (spec § 5.4) ──────────────────────────────

/// GET the worker tarball endpoint; (status, headers, body bytes).
async fn claim_tarball_get(
    router: &axum::Router,
    ws: &str,
    revision: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let uri = match revision {
        Some(r) => format!("/worker/workspace/{ws}.tar.gz?revision={r}"),
        None => format!("/worker/workspace/{ws}.tar.gz"),
    };
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {FIXTURE_WORKER_TOKEN}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

/// (path without leading "./", content when it is a regular UTF-8 file)
fn claim_tar_entries(gz: &[u8]) -> Vec<(String, Option<String>)> {
    use std::io::Read;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gz));
    archive
        .entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let path = e
                .path()
                .unwrap()
                .to_string_lossy()
                .trim_start_matches("./")
                .to_string();
            let mut s = String::new();
            let content = e.read_to_string(&mut s).ok().map(|_| s);
            (path, content)
        })
        .collect()
}

fn claim_tar_file(gz: &[u8], path: &str) -> Option<String> {
    claim_tar_entries(gz)
        .into_iter()
        .find(|(p, _)| p == path)
        .and_then(|(_, c)| c)
}

fn claim_tar_has_git_dir(gz: &[u8]) -> bool {
    claim_tar_entries(gz)
        .iter()
        .any(|(p, _)| p == ".git" || p.starts_with(".git/"))
}

/// Two commits on `etl` main touching `data.txt`; the live revision ends at the second.
async fn claim_two_main_commits(fx: &PinnedFixture) -> anyhow::Result<(String, String)> {
    let first = fx.etl.commit("main", "main", &[("data.txt", "v1")]);
    fx.mgr().reload("etl").await?;
    let second = fx.etl.commit("main", "main", &[("data.txt", "v2")]);
    fx.mgr().reload("etl").await?;
    assert_eq!(
        fx.mgr().get_revision("etl").as_deref(),
        Some(second.as_str())
    );
    Ok((first, second))
}

const CLAIM_TARBALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);

#[tokio::test]
async fn superseded_git_revision_is_served_from_pin_store() -> anyhow::Result<()> {
    tokio::time::timeout(CLAIM_TARBALL_TIMEOUT, async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
        let (first, _second) = claim_two_main_commits(&fx).await?;
        let (status, headers, bytes) = claim_tarball_get(&fx.router, "etl", Some(&first)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["X-Revision"], first.as_str());
        assert_eq!(claim_tar_file(&bytes, "data.txt").as_deref(), Some("v1"));
        assert!(
            !claim_tar_has_git_dir(&bytes),
            "pinned tarballs carry no .git"
        );
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn erroring_git_workspace_still_serves_a_pinned_revision() -> anyhow::Result<()> {
    tokio::time::timeout(CLAIM_TARBALL_TIMEOUT, async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
        let (_first, second) = claim_two_main_commits(&fx).await?;
        fx.mgr().mark_unavailable_for_test("etl");

        // The live path is gated, as today...
        let (status, _, _) = claim_tarball_get(&fx.router, "etl", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // ...but a git revision does not depend on the live entry.
        let (status, _, bytes) = claim_tarball_get(&fx.router, "etl", Some(&second)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(claim_tar_file(&bytes, "data.txt").as_deref(), Some("v2"));
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn missing_git_commit_is_404() -> anyhow::Result<()> {
    tokio::time::timeout(CLAIM_TARBALL_TIMEOUT, async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
        let missing = "0".repeat(40);
        let (status, _, _) = claim_tarball_get(&fx.router, "etl", Some(&missing)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn pinned_revision_during_git_outage_on_a_cold_replica_is_503() -> anyhow::Result<()> {
    tokio::time::timeout(CLAIM_TARBALL_TIMEOUT, async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
        // The replica needs the remote up to start; break it afterwards.
        let replica = fx.second_replica().await?;
        fx.etl.break_remote();
        // `release/2.3`'s commit is not the live revision, so it takes the pin path,
        // and the cold replica must fetch it.
        let (status, headers, _) =
            claim_tarball_get(&replica.router, "etl", Some(&fx.commits.etl_release)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers["Retry-After"], "5");
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn healthy_current_revision_keeps_the_live_tarball() -> anyhow::Result<()> {
    tokio::time::timeout(CLAIM_TARBALL_TIMEOUT, async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
        let (_first, second) = claim_two_main_commits(&fx).await?;
        let (status, _, bytes) = claim_tarball_get(&fx.router, "etl", Some(&second)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(claim_tar_file(&bytes, "data.txt").as_deref(), Some("v2"));
        assert!(
            claim_tar_has_git_dir(&bytes),
            "the healthy current revision is built from the live clone, unchanged"
        );
        Ok(())
    })
    .await?
}

/// The cache write after a live build is detached; give it time to land.
async fn claim_wait_for_detached_cache_put() {
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

/// A revision first served live (cached WITH `.git`), then superseded, then
/// requested again: the pinned branch must not reuse the live bytes.
#[tokio::test]
async fn live_cached_revision_once_superseded_is_served_without_git() -> anyhow::Result<()> {
    tokio::time::timeout(CLAIM_TARBALL_TIMEOUT, async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
        let first = fx.etl.commit("main", "main", &[("data.txt", "v1")]);
        fx.mgr().reload("etl").await?;
        let (status, _, bytes) = claim_tarball_get(&fx.router, "etl", Some(&first)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(claim_tar_has_git_dir(&bytes), "live build carries .git");
        claim_wait_for_detached_cache_put().await;

        fx.etl.commit("main", "main", &[("data.txt", "v2")]);
        fx.mgr().reload("etl").await?;
        let (status, headers, bytes) = claim_tarball_get(&fx.router, "etl", Some(&first)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["X-Revision"], first.as_str());
        assert_eq!(headers["ETag"], format!("\"{first}\"").as_str());
        assert_eq!(claim_tar_file(&bytes, "data.txt").as_deref(), Some("v1"));
        assert!(!claim_tar_has_git_dir(&bytes), "pinned bytes carry no .git");
        Ok(())
    })
    .await?
}

/// The reverse: a pinned build first, then that SHA becomes current and
/// healthy again — the live path must serve its own `.git` bytes.
#[tokio::test]
async fn pinned_cached_revision_made_current_again_is_served_with_git() -> anyhow::Result<()> {
    tokio::time::timeout(CLAIM_TARBALL_TIMEOUT, async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
        let (first, _second) = claim_two_main_commits(&fx).await?;
        let (status, _, bytes) = claim_tarball_get(&fx.router, "etl", Some(&first)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!claim_tar_has_git_dir(&bytes));
        claim_wait_for_detached_cache_put().await;

        fx.etl.force_branch("main", &first);
        fx.mgr().reload("etl").await?;
        assert_eq!(
            fx.mgr().get_revision("etl").as_deref(),
            Some(first.as_str())
        );
        let (status, headers, bytes) = claim_tarball_get(&fx.router, "etl", Some(&first)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["ETag"], format!("\"{first}\"").as_str());
        assert!(
            claim_tar_has_git_dir(&bytes),
            "live path serves its .git bytes"
        );
        Ok(())
    })
    .await?
}

/// If-None-Match short-circuits on the plain SHA for the pinned path too.
#[tokio::test]
async fn pinned_revision_if_none_match_is_304() -> anyhow::Result<()> {
    tokio::time::timeout(CLAIM_TARBALL_TIMEOUT, async {
        use tower::ServiceExt;
        let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
        let (first, _second) = claim_two_main_commits(&fx).await?;
        let req = axum::http::Request::builder()
            .uri(format!("/worker/workspace/etl.tar.gz?revision={first}"))
            .header("Authorization", format!("Bearer {FIXTURE_WORKER_TOKEN}"))
            .header("If-None-Match", format!("\"{first}\""))
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = fx.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        Ok(())
    })
    .await?
}
