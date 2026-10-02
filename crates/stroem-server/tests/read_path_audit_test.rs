//! § 7.9 read-path audit (spec 2026-10-02-git-refs-design.md): tests for the
//! job-scoped data the audit found untested elsewhere. Every job-scoped route
//! and MCP tool is already covered in `git_refs_read_paths_test.rs` (deny +
//! mask), `pinned_rerun_restart_test.rs` (re-run / restart source) and the
//! stroem-db stats test; see the audit table in spec § 7.9. What is left is the
//! `child_jobs[]` summary embedded in job detail, which the audit classifies
//! as the PARENT's data: no per-child ACL filter.

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
