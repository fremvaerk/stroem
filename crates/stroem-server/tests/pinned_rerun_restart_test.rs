//! Re-run and Restart of a pinned source job (spec 2026-10-02 § 7.3).
//!
//! The source is a top-level job created by a scheduler trigger with
//! `ref: release/2.3`, for a task that exists ONLY on that branch. Before
//! § 7.3, both entry points looked the task up in the live config first and
//! answered 404/400.

mod common;
use common::pinned::*;

use std::time::Duration;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::{json, Value as JsonValue};
use stroem_db::{JobRepo, JobStepRepo};
use stroem_server::config::{AclAction, AclConfig, AclRule};
use uuid::Uuid;

/// Where the fixture commits each workspace's YAML.
const RR_YAML_PATH: &str = "workflow.yaml";

/// Every test in this file runs under this bound (testcontainers + git).
const RR_TIMEOUT: Duration = Duration::from_secs(240);

/// Run a test body under [`RR_TIMEOUT`].
async fn rr_bounded(body: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    tokio::time::timeout(RR_TIMEOUT, body)
        .await
        .map_err(|_| anyhow::anyhow!("test timed out after {RR_TIMEOUT:?}"))?
}

/// `etl` main only carries the triggers; the task lives on `release/2.3`.
const RR_ETL_MAIN: &str = r#"
triggers:
  nightly:
    type: scheduler
    cron: "0 0 1 1 *"
    task: only-on-release
    ref: release/2.3
  tagged:
    type: scheduler
    cron: "0 0 1 1 *"
    task: only-on-release
    ref: v2.3.0
"#;

const RR_ETL_RELEASE: &str = r#"
actions:
  a:
    type: script
    script: "echo a"
  b:
    type: script
    script: "echo b"
tasks:
  only-on-release:
    folder: rel
    flow:
      a:
        action: a
      b:
        action: b
        depends_on: [a]
"#;

/// `release/2.3` after the task was renamed: `only-on-release` is gone.
const RR_ETL_RELEASE_RENAMED: &str = r#"
actions:
  a:
    type: script
    script: "echo a"
tasks:
  renamed-on-release:
    folder: rel
    flow:
      a:
        action: a
"#;

fn rr_opts() -> PinnedFixtureOpts {
    PinnedFixtureOpts {
        etl_main: Some(RR_ETL_MAIN.to_string()),
        etl_release: Some(RR_ETL_RELEASE.to_string()),
        ..Default::default()
    }
}

/// Source job: pinned to `release/2.3` at its first commit, `a` completed
/// with `a_output`, `b` failed.
async fn rr_failed_pinned_source(fx: &PinnedFixture, a_output: JsonValue) -> Result<Uuid> {
    let source = fire_etl_trigger(fx, "nightly").await?;
    let row = JobRepo::get(&fx.pool, source).await?.expect("source job");
    assert_eq!(row.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        row.revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    let w = register_worker(&fx.router, &["script"]).await;
    claim_and_complete(fx, &w, source, "a", json!({"output": a_output})).await;
    claim_and_complete(
        fx,
        &w,
        source,
        "b",
        json!({"exit_code": 1, "error": "b broke"}),
    )
    .await;
    assert_eq!(
        JobRepo::get(&fx.pool, source).await?.unwrap().status,
        "failed"
    );
    Ok(source)
}

/// Move `release/2.3` (same flow, new commit). The fixture's PinStore re-lists
/// on every resolve (poll_interval = 0), so the next resolution sees it.
fn rr_hotfix(fx: &PinnedFixture) -> String {
    fx.etl
        .commit("release/2.3", "release/2.3", &[("data/hotfix.txt", "2")])
}

fn rr_error(body: &JsonValue) -> &str {
    body["error"].as_str().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_of_pinned_source_reresolves_the_ref_for_a_release_only_task() -> Result<()> {
    rr_bounded(async {
        let fx = pinned_workspace_fixture(rr_opts()).await?;
        let source = rr_failed_pinned_source(&fx, json!({"ok": true})).await?;
        let c2 = rr_hotfix(&fx);
        assert_ne!(c2, fx.commits.etl_release);

        let (st, body) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/only-on-release/execute",
            None,
            Some(json!({"input": {}, "source_job_id": source})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "re-run of a pinned source: {body}");
        let rerun: Uuid = body["job_id"].as_str().unwrap().parse()?;

        let job = JobRepo::get(&fx.pool, rerun).await?.unwrap();
        assert_eq!(job.source_type, "rerun");
        assert_eq!(job.source_job_id, Some(source));
        assert_eq!(job.git_ref.as_deref(), Some("release/2.3"));
        assert_eq!(
            job.revision.as_deref(),
            Some(c2.as_str()),
            "re-run must re-resolve the branch"
        );
        assert_eq!(job.task_folder.as_deref(), Some("rel"));
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_of_pinned_source_under_another_task_name_is_400() -> Result<()> {
    rr_bounded(async {
        let fx = pinned_workspace_fixture(rr_opts()).await?;
        let source = rr_failed_pinned_source(&fx, json!({"ok": true})).await?;
        let (st, body) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/something-else/execute",
            None,
            Some(json!({"input": {}, "source_job_id": source})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            rr_error(&body).contains("is a run of task 'only-on-release'"),
            "{body}"
        );

        // F19: an unknown source id never enters the pinned path; the status
        // is today's (the task is not in the live config → 404).
        let (st, body) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/only-on-release/execute",
            None,
            Some(json!({"input": {}, "source_job_id": Uuid::new_v4()})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_of_pinned_source_plans_against_the_pinned_flow() -> Result<()> {
    rr_bounded(async {
        let fx = pinned_workspace_fixture(rr_opts()).await?;
        let source = rr_failed_pinned_source(&fx, json!({"ok": true})).await?;
        let c2 = rr_hotfix(&fx);

        let (st, plan) = api_req(
            &fx.router,
            "POST",
            &format!("/api/jobs/{source}/restart"),
            None,
            Some(json!({"from_step": "b", "dry_run": true})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "dry run: {plan}");
        assert_eq!(plan["restart_steps"], json!(["b"]));
        assert_eq!(plan["carried_over"], json!(["a"]));

        let (st, body) = api_req(
            &fx.router,
            "POST",
            &format!("/api/jobs/{source}/restart"),
            None,
            Some(json!({"from_step": "b"})),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "restart: {body}");
        let new_id: Uuid = body["job_id"].as_str().unwrap().parse()?;

        let job = JobRepo::get(&fx.pool, new_id).await?.unwrap();
        assert_eq!(job.source_type, "restart");
        assert_eq!(job.git_ref.as_deref(), Some("release/2.3"));
        assert_eq!(job.revision.as_deref(), Some(c2.as_str()));
        assert_eq!(job.task_folder.as_deref(), Some("rel"));

        let steps = JobStepRepo::get_steps_for_job(&fx.pool, new_id).await?;
        let a = steps.iter().find(|s| s.step_name == "a").expect("row a");
        assert!(a.carried_over, "a is carried from the source");
        assert_eq!(a.status, "completed");
        let b = steps.iter().find(|s| s.step_name == "b").expect("row b");
        assert!(!b.carried_over);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_rerun_authorises_against_the_source_task_folder() -> Result<()> {
    const REL: &str = "rel@test.local";
    const LIVE: &str = "live@test.local";
    rr_bounded(async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts {
            acl: Some(AclConfig {
                default: AclAction::Deny,
                rules: vec![
                    AclRule {
                        workspace: "*".into(),
                        tasks: vec!["rel/*".into()],
                        action: AclAction::Run,
                        groups: vec![],
                        users: vec![REL.into()],
                    },
                    AclRule {
                        workspace: "*".into(),
                        tasks: vec!["live/*".into()],
                        action: AclAction::Run,
                        groups: vec![],
                        users: vec![LIVE.into()],
                    },
                ],
            }),
            users: vec![
                FixtureUser {
                    email: REL,
                    groups: vec![],
                    admin: false,
                },
                FixtureUser {
                    email: LIVE,
                    groups: vec![],
                    admin: false,
                },
            ],
            ..rr_opts()
        })
        .await?;
        let source = rr_failed_pinned_source(&fx, json!({"ok": true})).await?;
        let rel = fx.api_key(REL, false).await;
        let live = fx.api_key(LIVE, false).await;
        let uri = "/api/workspaces/etl/tasks/only-on-release/execute";
        let body = json!({"input": {}, "source_job_id": source});

        let (st, resp) = api_req(&fx.router, "POST", uri, Some(&live), Some(body.clone())).await;
        assert_eq!(
            st,
            StatusCode::NOT_FOUND,
            "live-folder user must not re-run a rel/ job: {resp}"
        );

        // F19: the ACL check comes BEFORE any validation that reveals the
        // source's data — a denied user learns nothing about the source's task.
        let (st, resp) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/something-else/execute",
            Some(&live),
            Some(body.clone()),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{resp}");
        assert!(!resp.to_string().contains("only-on-release"), "{resp}");

        let (st, resp) = api_req(&fx.router, "POST", uri, Some(&rel), Some(body)).await;
        assert_eq!(st, StatusCode::OK, "rel-folder user re-runs: {resp}");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_and_restart_of_pinned_source_are_400_when_the_tip_lost_the_task() -> Result<()> {
    rr_bounded(async {
        let fx = pinned_workspace_fixture(rr_opts()).await?;
        let source = rr_failed_pinned_source(&fx, json!({"ok": true})).await?;
        let c2 = fx.etl.commit(
            "release/2.3",
            "release/2.3",
            &[(RR_YAML_PATH, RR_ETL_RELEASE_RENAMED)],
        );
        let short = &c2[..7];

        let (st, body) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/only-on-release/execute",
            None,
            Some(json!({"input": {}, "source_job_id": source})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "re-run: {body}");
        assert_eq!(
            rr_error(&body),
            format!("Task 'only-on-release' does not exist at ref 'release/2.3' ({short})"),
            "{body}"
        );

        let (st, body) = api_req(
            &fx.router,
            "POST",
            &format!("/api/jobs/{source}/restart"),
            None,
            Some(json!({"from_step": "b", "dry_run": true})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "restart: {body}");
        assert_eq!(
            rr_error(&body),
            format!("Task 'only-on-release' does not exist at ref 'release/2.3' ({short})"),
            "{body}"
        );
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_and_restart_of_pinned_source_are_400_when_the_branch_was_deleted() -> Result<()> {
    rr_bounded(async {
        let fx = pinned_workspace_fixture(rr_opts()).await?;
        let source = rr_failed_pinned_source(&fx, json!({"ok": true})).await?;
        fx.etl.delete_branch("release/2.3");

        let (st, body) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/only-on-release/execute",
            None,
            Some(json!({"input": {}, "source_job_id": source})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "re-run: {body}");
        assert!(
            rr_error(&body).contains("ref 'release/2.3' not found in workspace 'etl'"),
            "RefNotFound: {body}"
        );

        let (st, body) = api_req(
            &fx.router,
            "POST",
            &format!("/api/jobs/{source}/restart"),
            None,
            Some(json!({"from_step": "b"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "restart: {body}");
        assert!(
            rr_error(&body).contains("ref 'release/2.3' not found in workspace 'etl'"),
            "RefNotFound: {body}"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE source_job_id = $1")
            .bind(source)
            .fetch_one(&fx.pool)
            .await?;
        assert_eq!(count, 0, "nothing is created for a deleted branch");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_of_tag_pinned_source_keeps_the_tag_commit() -> Result<()> {
    rr_bounded(async {
        let fx = pinned_workspace_fixture(rr_opts()).await?;
        fx.etl.tag("v2.3.0", &fx.commits.etl_release);
        let source = fire_etl_trigger(&fx, "tagged").await?;
        let row = JobRepo::get(&fx.pool, source).await?.expect("source job");
        assert_eq!(row.git_ref.as_deref(), Some("v2.3.0"));
        assert_eq!(
            row.revision.as_deref(),
            Some(fx.commits.etl_release.as_str())
        );
        // The branch moves on; the tag does not.
        let c2 = rr_hotfix(&fx);
        assert_ne!(c2, fx.commits.etl_release);

        let (st, body) = api_req(
            &fx.router,
            "POST",
            "/api/workspaces/etl/tasks/only-on-release/execute",
            None,
            Some(json!({"input": {}, "source_job_id": source})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "re-run of a tag-pinned source: {body}");
        let rerun: Uuid = body["job_id"].as_str().unwrap().parse()?;

        let job = JobRepo::get(&fx.pool, rerun).await?.unwrap();
        assert_eq!(job.git_ref.as_deref(), Some("v2.3.0"));
        assert_eq!(
            job.revision.as_deref(),
            Some(fx.commits.etl_release.as_str()),
            "a tag re-resolves to the commit it names"
        );
        assert_eq!(job.task_folder.as_deref(), Some("rel"));
        Ok(())
    })
    .await
}

/// Declared ONLY at release/2.3's first commit (the source's); the branch then
/// moves to a commit without it.
const RR_SOURCE_ONLY_SECRET: &str = "rr-only-at-the-source-commit";

fn rr_secret_opts() -> PinnedFixtureOpts {
    PinnedFixtureOpts {
        etl_main: Some(RR_ETL_MAIN.to_string()),
        etl_release: Some(format!(
            "secrets:\n  TOKEN: \"{RR_SOURCE_ONLY_SECRET}\"\n{RR_ETL_RELEASE}"
        )),
        ..Default::default()
    }
}

/// Restart carried rows (spec § 7.4 redaction closure): the source's `a`
/// output, holding a secret of the SOURCE commit, is copied into a restart
/// pinned at the branch's new commit, which no longer declares that secret.
/// Only the source lineage (restart → source) can mask it.
#[tokio::test(flavor = "multi_thread")]
async fn restart_masks_a_carried_secret_of_the_source_commit() -> Result<()> {
    rr_bounded(async {
        let fx = pinned_workspace_fixture(rr_secret_opts()).await?;
        let carried = json!({"token": RR_SOURCE_ONLY_SECRET});
        let source = rr_failed_pinned_source(&fx, carried.clone()).await?;

        let c2 = fx.etl.commit(
            "release/2.3",
            "release/2.3",
            &[(RR_YAML_PATH, RR_ETL_RELEASE)],
        );
        let pinned = fx.mgr().pins().ensure("etl", &c2).await?;
        assert!(
            !fx.mgr()
                .pin_redaction_values("etl", &pinned)
                .await
                .iter()
                .any(|v| v == RR_SOURCE_ONLY_SECRET),
            "precondition: the new commit does not declare the secret"
        );

        let (st, body) = api_req(
            &fx.router,
            "POST",
            &format!("/api/jobs/{source}/restart"),
            None,
            Some(json!({"from_step": "b"})),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "restart: {body}");
        let restart: Uuid = body["job_id"].as_str().unwrap().parse()?;
        let job = JobRepo::get(&fx.pool, restart).await?.unwrap();
        assert_eq!(job.revision.as_deref(), Some(c2.as_str()));
        let a = JobStepRepo::get_steps_for_job(&fx.pool, restart)
            .await?
            .into_iter()
            .find(|s| s.step_name == "a")
            .expect("row a");
        assert!(a.carried_over);
        assert_eq!(
            a.output,
            Some(carried),
            "precondition: the source's output is carried raw"
        );

        let (st, detail) = api_req(
            &fx.router,
            "GET",
            &format!("/api/jobs/{restart}"),
            None,
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{detail}");
        assert!(
            !detail.to_string().contains(RR_SOURCE_ONLY_SECRET),
            "carried secret leaked: {detail}"
        );
        let a = detail["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .find(|s| s["step_name"] == "a")
            .expect("step a")
            .clone();
        assert_eq!(
            a["output"],
            json!({"token": "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}"}),
            "{a}"
        );
        Ok(())
    })
    .await
}
