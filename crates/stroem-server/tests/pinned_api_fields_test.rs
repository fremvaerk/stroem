//! API exposure of pins (spec 2026-10-02 § 11).

mod common;
use common::pinned::*;

use std::time::Duration;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::{json, Value as JsonValue};

/// Every test runs under this bound (testcontainers + git).
const AF_TIMEOUT: Duration = Duration::from_secs(240);

async fn af_bounded(body: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    tokio::time::timeout(AF_TIMEOUT, body)
        .await
        .map_err(|_| anyhow::anyhow!("test timed out after {AF_TIMEOUT:?}"))?
}

/// `etl` main: `call` is a root `type: task` step, so its child exists at
/// creation; `later` runs an action at release/2.3.
const AF_ETL_MAIN: &str = r#"
actions:
  hello:
    type: script
    runner: local
    script: echo main
  call-nightly:
    type: task
    task: nightly
    ref: release/2.3
tasks:
  manifest:
    flow:
      call:
        action: call-nightly
      later:
        action: hello
        ref: release/2.3
"#;

#[tokio::test(flavor = "multi_thread")]
async fn job_detail_and_list_expose_pins() -> Result<()> {
    af_bounded(async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts {
            etl_main: Some(AF_ETL_MAIN.to_string()),
            ..Default::default()
        })
        .await?;
        let c1 = fx.commits.etl_release.clone();

        let (st, body) = execute_task(&fx.router, "etl", "manifest", json!({}), None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let job_id = body["job_id"].as_str().unwrap().to_string();

        let (st, detail) = api_req(
            &fx.router,
            "GET",
            &format!("/api/jobs/{job_id}"),
            None,
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{detail}");
        assert_eq!(
            detail["ref"],
            JsonValue::Null,
            "the manifest job itself is unpinned"
        );
        let steps = detail["steps"].as_array().unwrap();
        let later = steps.iter().find(|s| s["step_name"] == "later").unwrap();
        assert_eq!(later["action_ref"], "release/2.3");
        assert_eq!(later["action_revision"], c1.as_str());
        assert_eq!(later["action_workspace"], "etl");
        assert_eq!(later["task_ref"], JsonValue::Null);

        let call = steps.iter().find(|s| s["step_name"] == "call").unwrap();
        assert_eq!(call["task_ref"], "release/2.3");
        assert_eq!(call["task_revision"], c1.as_str());
        assert_eq!(call["task_workspace"], "etl");
        let child_ref = &call["child_jobs"][0];
        assert_eq!(child_ref["ref"], "release/2.3");
        assert_eq!(child_ref["revision"], c1.as_str());

        let child_id = child_ref["job_id"]
            .as_str()
            .or_else(|| child_ref["id"].as_str())
            .unwrap();
        let (st, child) = api_req(
            &fx.router,
            "GET",
            &format!("/api/jobs/{child_id}"),
            None,
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{child}");
        assert_eq!(child["ref"], "release/2.3");
        assert_eq!(child["revision"], c1.as_str());

        let (st, list) = api_req(&fx.router, "GET", "/api/jobs?limit=10", None, None).await;
        assert_eq!(st, StatusCode::OK, "{list}");
        let listed_child = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["job_id"].as_str() == Some(child_id))
            .expect("child listed");
        assert_eq!(listed_child["ref"], "release/2.3");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_status_and_list_carry_ref() -> Result<()> {
    af_bounded(async {
        let fx = pinned_workspace_fixture(PinnedFixtureOpts {
            mcp: true,
            ..Default::default()
        })
        .await?;
        let job_id = fx.create_pinned_etl_job("nightly").await?;

        let status = mcp_tool_json(
            &mcp_call(
                &fx.router,
                None,
                "get_job_status",
                json!({"job_id": job_id.to_string()}),
            )
            .await,
        );
        assert_eq!(status["ref"], "release/2.3", "{status}");
        assert_eq!(status["revision"], fx.commits.etl_release.as_str());

        let list = mcp_tool_json(&mcp_call(&fx.router, None, "list_jobs", json!({})).await);
        let item = list["jobs"]
            .as_array()
            .or_else(|| list.as_array())
            .expect("jobs array")
            .iter()
            .find(|j| j["job_id"].as_str() == Some(&job_id.to_string()))
            .expect("pinned job listed");
        assert_eq!(item["ref"], "release/2.3", "{list}");
        Ok(())
    })
    .await
}
