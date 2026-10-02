//! § 7.9 read-path audit (spec 2026-10-02-git-refs-design.md): tests for the
//! job-scoped data the audit found untested elsewhere. Every job-scoped route
//! and MCP tool is already covered in `git_refs_read_paths_test.rs` (deny +
//! mask), `pinned_rerun_restart_test.rs` (re-run / restart source) and the
//! stroem-db stats test; see the audit table in spec § 7.9. What is left:
//! - the `child_jobs[]` summary embedded in job detail, which the audit
//!   classifies as the PARENT's data: no per-child ACL filter;
//! - content COPIED into a job from another job (a hook payload, a child's
//!   output settling into its parent step), which the job's redaction set must
//!   cover through its redaction closure (§ 7.4).

mod common;
use common::pinned::*;

use std::collections::BTreeSet;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;
use stroem_server::config::{AclAction, AclConfig, AclRule};
use uuid::Uuid;

const VIEWER: &str = "viewer@audit.test";
const ADMIN: &str = "admin@audit.test";

/// `manifest` (folder `public`, View for VIEWER) calls `nightly` at
/// release/2.3 from a ROOT step, so the child exists at creation. The
/// fixture's release/2.3 declares `nightly` in folder `nightlies`, which no
/// rule allows.
const AUDIT_ETL_MAIN: &str = r#"
actions:
  call-nightly:
    type: task
    task: nightly
    ref: release/2.3
tasks:
  manifest:
    folder: public
    flow:
      call:
        action: call-nightly
"#;

/// Every key of one `child_jobs[]` entry: identifiers, a status and a
/// timestamp. Redaction skips `child_jobs` whole (`STEP_IDENTIFIER_KEYS`), so a
/// content field added here would leak unredacted.
const CHILD_SUMMARY_KEYS: [&str; 7] = [
    "created_at",
    "id",
    "ref",
    "revision",
    "status",
    "task_name",
    "workspace",
];

fn audit_opts() -> PinnedFixtureOpts {
    PinnedFixtureOpts {
        acl: Some(AclConfig {
            default: AclAction::Deny,
            rules: vec![AclRule {
                workspace: "etl".to_string(),
                tasks: vec!["public/*".to_string()],
                action: AclAction::View,
                groups: vec![],
                users: vec![VIEWER.to_string()],
            }],
        }),
        users: vec![
            FixtureUser {
                email: VIEWER,
                groups: vec![],
                admin: false,
            },
            FixtureUser {
                email: ADMIN,
                groups: vec![],
                admin: true,
            },
        ],
        etl_main: Some(AUDIT_ETL_MAIN.to_string()),
        ..Default::default()
    }
}

/// A viewer of the parent sees the summary of a child it may not open. The
/// summary carries identifiers only, all of which the parent already exposes
/// (its step stamps the child's workspace, ref and commit; the child's result
/// settles into that step). The child's own paths stay behind the child's
/// own ACL (§ 7.8).
#[tokio::test(flavor = "multi_thread")]
async fn child_job_summary_in_job_detail_is_identifiers_only_and_the_child_stays_job_scoped(
) -> Result<()> {
    bounded(async {
        let fx = pinned_workspace_fixture(audit_opts()).await?;
        let admin = fx.login(ADMIN).await;
        let viewer = fx.login(VIEWER).await;

        let (st, body) = execute_task(&fx.router, "etl", "manifest", json!({}), Some(&admin)).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let parent = body["job_id"].as_str().expect("job_id").to_string();

        // The viewer may open the parent, and its `call` step lists the child.
        let (st, detail) = api_req(
            &fx.router,
            "GET",
            &format!("/api/jobs/{parent}"),
            Some(&viewer),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{detail}");
        let call = detail["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .find(|s| s["step_name"] == "call")
            .unwrap_or_else(|| panic!("no call step: {detail}"))
            .clone();
        let summaries = call["child_jobs"].as_array().expect("child_jobs");
        assert_eq!(summaries.len(), 1, "{call}");
        let summary = &summaries[0];

        // Identifiers only, and redaction relies on it.
        let keys: BTreeSet<&str> = summary
            .as_object()
            .expect("summary object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, BTreeSet::from(CHILD_SUMMARY_KEYS), "{summary}");
        assert!(
            stroem_server::redaction::STEP_IDENTIFIER_KEYS.contains(&"child_jobs"),
            "redaction skips child_jobs whole; the key-set check above is what keeps that safe"
        );

        // Nothing beyond what the parent's own step already stamps.
        assert_eq!(summary["task_name"], json!("nightly"), "{summary}");
        assert_eq!(summary["workspace"], call["task_workspace"], "{call}");
        assert_eq!(summary["ref"], call["task_ref"], "{call}");
        assert_eq!(summary["revision"], call["task_revision"], "{call}");
        assert_eq!(summary["ref"], json!("release/2.3"), "{summary}");

        // The child is a pinned job in a folder the viewer may not read.
        let child: Uuid = summary["id"].as_str().expect("child id").parse()?;
        let row = stroem_db::JobRepo::get(&fx.pool, child)
            .await?
            .expect("child row");
        assert_eq!(row.git_ref.as_deref(), Some("release/2.3"));
        assert_eq!(row.task_folder.as_deref(), Some("nightlies"));

        // Its own data stays job-scoped: denied to the viewer as an unknown
        // job, readable by the admin (so the 404 is the ACL, not a missing row).
        for uri in [
            format!("/api/jobs/{child}"),
            format!("/api/jobs/{child}/logs"),
            format!("/api/jobs/{child}/artifacts"),
        ] {
            let (st, b) = api_req(&fx.router, "GET", &uri, Some(&viewer), None).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{uri}: {b}");
            assert_eq!(b["error"], json!("Job not found"), "{uri}: {b}");
            let (st, b) = api_req(&fx.router, "GET", &uri, Some(&admin), None).await;
            assert_eq!(st, StatusCode::OK, "admin {uri}: {b}");
        }
        Ok(())
    })
    .await
}

// ── Redaction closure: content copied between jobs (§ 7.4) ─────────────

/// Exists ONLY at release/2.3 of `etl`: the live set never masks it.
const REF_ONLY_SECRET: &str = "ref-only-audit-s3cret-23";
const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// `etl` main: every job below is UNPINNED. Only the `boom` / `emit` steps
/// run an action at release/2.3.
const CLOSURE_MAIN: &str = r#"
actions:
  notify:
    type: script
    runner: local
    script: echo notify
  call-child:
    type: task
    task: child
tasks:
  failing:
    on_error:
      - action: notify
        input:
          msg: "{{ hook.error_message }}"
    flow:
      boom:
        action: boom
        ref: release/2.3
  parent:
    flow:
      call:
        action: call-child
  child:
    flow:
      leak:
        action: emit
        ref: release/2.3
"#;

const CLOSURE_RELEASE: &str = r#"
secrets:
  TOKEN: "ref-only-audit-s3cret-23"
actions:
  boom:
    type: script
    runner: local
    script: echo boom
  emit:
    type: script
    runner: local
    script: echo emit
tasks: {}
"#;

fn closure_opts() -> PinnedFixtureOpts {
    PinnedFixtureOpts {
        etl_main: Some(CLOSURE_MAIN.to_string()),
        etl_release: Some(CLOSURE_RELEASE.to_string()),
        ..Default::default()
    }
}

/// Job detail of `job` (auth off): 200, and the ref-only secret appears
/// nowhere in the body, while the mask does.
async fn assert_detail_masks_ref_only_secret(fx: &PinnedFixture, job: Uuid) -> serde_json::Value {
    let (st, detail) = api_req(&fx.router, "GET", &format!("/api/jobs/{job}"), None, None).await;
    assert_eq!(st, StatusCode::OK, "{detail}");
    let text = detail.to_string();
    assert!(
        !text.contains(REF_ONLY_SECRET),
        "ref-only secret leaked: {detail}"
    );
    assert!(text.contains(MASK), "nothing was masked: {detail}");
    detail
}

/// Hook payload: an UNPINNED job's step runs an action at release/2.3 and
/// fails quoting a release-only secret. The `on_error` hook copies that error
/// into an UNPINNED hook job's input, whose own pins are none, so only its
/// hook-source chain carries release/2.3.
#[tokio::test(flavor = "multi_thread")]
async fn hook_job_detail_masks_a_secret_of_its_sources_step_pin() -> Result<()> {
    bounded(async {
        let fx = pinned_workspace_fixture(closure_opts()).await?;
        let worker = register_worker(&fx.router, &["script"]).await;
        let (st, body) = execute_task(&fx.router, "etl", "failing", json!({}), None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let source: Uuid = body["job_id"].as_str().expect("job_id").parse()?;

        claim_and_complete(
            &fx,
            &worker,
            source,
            "boom",
            json!({"exit_code": 1, "error": format!("auth rejected token {REF_ONLY_SECRET}")}),
        )
        .await;

        let hook: Uuid = sqlx::query_scalar(
            "SELECT job_id FROM job WHERE source_type = 'hook' AND source_job_id = $1",
        )
        .bind(source)
        .fetch_one(&fx.pool)
        .await?;
        let row = stroem_db::JobRepo::get(&fx.pool, hook)
            .await?
            .expect("hook row");
        assert_eq!(row.git_ref, None, "the hook job is unpinned");
        assert!(
            row.input
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default()
                .contains(REF_ONLY_SECRET),
            "precondition: the hook payload carries the secret: {:?}",
            row.input
        );

        // The source masks it through its own step pin; the hook job must too.
        assert_detail_masks_ref_only_secret(&fx, source).await;
        let detail = assert_detail_masks_ref_only_secret(&fx, hook).await;
        assert_eq!(
            detail["input"]["msg"],
            json!(format!("Step 'boom': auth rejected token {MASK}")),
            "{detail}"
        );
        Ok(())
    })
    .await
}

/// Child output propagation: an UNPINNED parent's `type: task` step settles
/// with the output of an UNPINNED child, whose own step ran at release/2.3
/// and printed a release-only secret. The parent references no pin itself;
/// its descendants do.
#[tokio::test(flavor = "multi_thread")]
async fn parent_job_detail_masks_a_secret_of_its_childs_step_pin() -> Result<()> {
    bounded(async {
        let fx = pinned_workspace_fixture(closure_opts()).await?;
        let worker = register_worker(&fx.router, &["script"]).await;
        let (st, body) = execute_task(&fx.router, "etl", "parent", json!({}), None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let parent: Uuid = body["job_id"].as_str().expect("job_id").parse()?;
        let child: Uuid = sqlx::query_scalar("SELECT job_id FROM job WHERE parent_job_id = $1")
            .bind(parent)
            .fetch_one(&fx.pool)
            .await?;

        claim_and_complete(
            &fx,
            &worker,
            child,
            "leak",
            json!({"exit_code": 0, "output": {"token": REF_ONLY_SECRET}}),
        )
        .await;

        let call = stroem_db::JobStepRepo::get_steps_for_job(&fx.pool, parent)
            .await?
            .into_iter()
            .find(|s| s.step_name == "call")
            .expect("call step");
        assert_eq!(call.status, "completed");
        assert!(
            call.output
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default()
                .contains(REF_ONLY_SECRET),
            "precondition: the child's output settled into the parent step: {:?}",
            call.output
        );
        let parent_row = stroem_db::JobRepo::get(&fx.pool, parent)
            .await?
            .expect("parent row");
        assert_eq!(parent_row.git_ref, None, "the parent is unpinned");

        assert_detail_masks_ref_only_secret(&fx, child).await;
        let detail = assert_detail_masks_ref_only_secret(&fx, parent).await;
        let step = detail["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .find(|s| s["step_name"] == "call")
            .expect("call step")
            .clone();
        assert_eq!(step["output"], json!({"leak": {"token": MASK}}), "{step}");
        Ok(())
    })
    .await
}
