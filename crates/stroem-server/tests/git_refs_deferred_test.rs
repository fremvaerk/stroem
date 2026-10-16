//! Templated refs (`docs/superpowers/specs/2026-10-15-templated-refs-design.md`):
//! deferred steps, their resolution at promotion, the ref memo, the lease,
//! privacy of the rendered value, `type: task` and webhook templated refs.

use crate::common::pinned::*;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

/// A claim request may only carry known abilities (spec D9): `deferred` (or any
/// other unknown token) is a 400 before the claim SQL runs.
#[tokio::test]
async fn claim_with_an_unknown_capability_is_a_400() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    for caps in [
        json!(["deferred"]),
        json!(["script", "deferred"]),
        json!(["scipt"]),
    ] {
        let (status, body) = worker_req(
            &fx.router,
            "POST",
            "/worker/jobs/claim",
            Some(json!({"worker_id": Uuid::new_v4().to_string(), "capabilities": caps})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{caps}: {body}");
        assert!(
            body.to_string().contains("unknown capability"),
            "{caps}: {body}"
        );
    }
    Ok(())
}
