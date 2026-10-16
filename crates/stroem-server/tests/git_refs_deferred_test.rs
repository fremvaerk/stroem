//! Templated refs (`docs/superpowers/specs/2026-10-15-templated-refs-design.md`):
//! deferred steps, their resolution at promotion, the ref memo, the lease,
//! privacy of the rendered value, `type: task` and webhook templated refs.

use crate::common::capture::capture;
use crate::common::pinned::*;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;
use stroem_server::workspace::pins::PinLoadWithheld;
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

/// A commit whose config does not load: its connection renders an undefined
/// secret, so the loader itself fails.
pub(crate) const DF_BROKEN: &str = r#"
secrets:
  token: s3cr3t-token-value
connections:
  db:
    host: "{{ secret.token }}-{{ secret.nope }}"
actions:
  hello:
    type: script
    runner: local
    script: echo broken
"#;

/// Spec § 5.5 change 1: `PinStore::resolve` no longer records the ref.
#[tokio::test]
async fn pin_store_resolve_records_no_git_ref_span_field() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let (captured, _guard) = capture();
    fx.mgr().pins().resolve("etl", "release/2.3").await?;
    assert!(
        captured.span_fields("git_ref").is_empty(),
        "{}",
        captured.text()
    );
    assert!(
        !captured.span_fields("workspace").is_empty(),
        "{}",
        captured.text()
    );
    Ok(())
}

/// Spec § 5.5 change 2: the private load labels a withheld load by commit only.
#[tokio::test]
async fn private_pin_load_labels_a_withheld_load_by_commit_only() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let broken = fx
        .etl
        .commit("zz-sentinel-71c3", "main", &[("workflow.yaml", DF_BROKEN)]);
    let (captured, _guard) = capture();
    let err = fx
        .mgr()
        .pinned_config_private("etl", &broken)
        .await
        .map(|_| ())
        .expect_err("the broken commit does not load");
    assert!(err.downcast_ref::<PinLoadWithheld>().is_some(), "{err:#}");
    assert_eq!(
        format!("{err:#}"),
        format!(
            "[pin] etl ({}) cannot be loaded: its configuration does not load",
            &broken[..7]
        )
    );
    let text = captured.text();
    assert!(
        text.contains(&format!("[pin] etl ({})", &broken[..7])),
        "{text}"
    );
    assert!(!text.contains("zz-sentinel-71c3"), "{text}");
    assert!(!text.contains("s3cr3t-token-value"), "{text}");
    Ok(())
}
