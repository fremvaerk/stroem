//! Templated refs (`docs/superpowers/specs/2026-10-15-templated-refs-design.md`):
//! deferred steps, their resolution at promotion, the ref memo, the lease,
//! privacy of the rendered value, `type: task` and webhook templated refs.

use crate::common::capture::capture;
use crate::common::pinned::*;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use stroem_db::{JobRefPinRepo, JobStepRepo, JobStepRow};
use stroem_server::config::JobDefaults;
use stroem_server::job_creator::create_job_for_task_detailed;
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

// ─── Fixture ───────────────────────────────────────────────────────────────

/// `etl` main gains a `type: task` action whose own `ref:` is a template.
const DF_MAIN_ACTIONS: &str = "actions:\n  call-nightly-at:\n    type: task\n    task: nightly\n    ref: \"{{ input.branch }}\"\n  hello:\n";

/// Templated-ref tasks, appended to `ETL_MAIN`'s `tasks:` section (its last key).
const DF_TASKS: &str = r#"
  tr-input:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      run:
        action: hello
        ref: "{{ input.branch }}"
  tr-later:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      first:
        action: hello
      later:
        action: hello
        ref: "{{ input.branch }}"
        depends_on: [first]
  tr-output:
    flow:
      pick:
        action: hello
      run:
        action: hello
        ref: "{{ pick.output.branch }}"
        depends_on: [pick]
  tr-each:
    flow:
      run:
        action: hello
        ref: "{{ each.item }}"
        for_each: ["release/2.3", "main", "release/2.3"]
  tr-each-objects:
    flow:
      run:
        action: hello
        ref: "{{ each.item }}"
        for_each: [{"b": "release/2.3"}]
  tr-memo:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      a:
        action: hello
        ref: release/2.3
      b:
        action: hello
        ref: "{{ input.branch }}"
        depends_on: [a]
  tr-memo-two:
    flow:
      x:
        action: hello
      y:
        action: hello
        ref: "{{ 'release/' ~ '2.3' }}"
        depends_on: [x]
      z:
        action: hello
        depends_on: [y]
      w:
        action: hello
        ref: "release/{{ '2.3' }}"
        depends_on: [z]
  tr-task:
    flow:
      run:
        action: call-local
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-gate:
    flow:
      run:
        action: gate
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-agent:
    flow:
      run:
        action: ask
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-missing-action:
    flow:
      run:
        action: nope
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-retry:
    flow:
      run:
        action: flaky
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-billing:
    flow:
      run:
        action: billing.export
        ref: "{{ input.tag }}"
  tr-docs:
    flow:
      run:
        action: docs.render
        ref: "{{ input.r }}"
  tr-bad-syntax:
    flow:
      run:
        action: hello
        ref: "{{ input.branch "
  tr-chain:
    input:
      branch:
        type: string
        default: release/2.3
    flow:
      a:
        action: hello
        ref: "{{ input.branch }}"
      b:
        action: hello
        depends_on: [a]
  tr-task-action:
    flow:
      call:
        action: call-nightly-at
        input:
          branch: release/2.3
  tr-task-action-later:
    flow:
      first:
        action: hello
      call:
        action: call-nightly-at
        depends_on: [first]
        input:
          branch: release/2.3
"#;

/// `etl` main with the templated-ref actions and tasks.
pub(crate) fn df_etl_main() -> String {
    format!(
        "{}{DF_TASKS}",
        ETL_MAIN.replace("actions:\n  hello:\n", DF_MAIN_ACTIONS)
    )
}

/// `etl` release/2.3 plus `flaky`, an action with its own `retry:`
/// (Review Focus 3 and 4).
pub(crate) fn df_etl_release() -> String {
    ETL_RELEASE.replace(
        "actions:\n  hello:\n",
        "actions:\n  flaky:\n    type: script\n    runner: local\n    script: exit 1\n    retry:\n      max_attempts: 3\n      delay: 1s\n  hello:\n",
    )
}

pub(crate) async fn df_fixture() -> Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(df_etl_main()),
        etl_release: Some(df_etl_release()),
        ..Default::default()
    })
    .await
}

/// A job of `task` in the live `etl` config, created like the execute API does.
pub(crate) async fn df_create(
    fx: &PinnedFixture,
    task: &str,
    input: serde_json::Value,
) -> Result<Uuid> {
    let cfg = fx.mgr().get_config("etl").await.expect("etl loaded");
    create_job_for_task_detailed(
        fx.mgr(),
        &fx.pool,
        &cfg,
        "etl",
        task,
        input,
        "api",
        None,
        Some(&fx.commits.etl_main),
        None,
        None,
        JobDefaults::default(),
    )
    .await
    .map(|c| c.job_id)
}

pub(crate) async fn df_step(pool: &PgPool, job_id: Uuid, name: &str) -> JobStepRow {
    JobStepRepo::get_step(pool, job_id, name)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("step {name} exists"))
}

async fn df_job_count(pool: &PgPool, task: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE task_name = $1")
        .bind(task)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ─── Creation (spec § 5.1, § 5.2, § 7) ─────────────────────────────────────

#[tokio::test]
async fn templated_step_ref_creates_a_deferred_row() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-later", json!({})).await?;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.status, "pending");
    assert_eq!(later.action_type, "deferred");
    assert_eq!(later.required_ability, "deferred");
    assert_eq!(later.runner, "none");
    assert_eq!(later.required_tags, json!([]));
    assert_eq!(later.action_workspace.as_deref(), Some("etl"));
    assert_eq!(later.action_name, "hello");
    assert_eq!(later.ref_template.as_deref(), Some("{{ input.branch }}"));
    assert!(
        later.action_spec.is_none()
            && later.action_ref.is_none()
            && later.action_revision.is_none()
    );
    let first = df_step(&fx.pool, job_id, "first").await;
    assert_eq!(first.action_type, "script");
    assert!(first.ref_template.is_none());
    Ok(())
}

#[tokio::test]
async fn creation_persists_the_ref_memo() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-memo", json!({})).await?;
    let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
    assert!(
        memo.iter().any(|e| e.workspace == "etl"
            && e.git_ref == "release/2.3"
            && e.commit == fx.commits.etl_release),
        "{memo:?}"
    );
    // A pinned job's own pin seeds its memo.
    let pinned = fx.create_pinned_etl_job("nightly").await?;
    let memo = JobRefPinRepo::list_for_job(&fx.pool, pinned).await?;
    assert!(
        memo.iter().any(|e| e.workspace == "etl"
            && e.git_ref == "release/2.3"
            && e.commit == fx.commits.etl_release),
        "{memo:?}"
    );
    Ok(())
}

#[tokio::test]
async fn templated_ref_syntax_error_is_a_400_at_creation() -> Result<()> {
    let fx = df_fixture().await?;
    let (status, body) = execute_task(&fx.router, "etl", "tr-bad-syntax", json!({}), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("invalid `ref` template"),
        "{body}"
    );
    assert_eq!(df_job_count(&fx.pool, "tr-bad-syntax").await, 0);
    Ok(())
}

/// Contract deviation 4 (spec § 4.2 rule 4): the owner comes from the action
/// name, so a non-git owner is known — and refused — at creation.
#[tokio::test]
async fn templated_ref_on_a_non_git_owner_is_a_400_at_creation() -> Result<()> {
    let fx = df_fixture().await?;
    let (status, body) =
        execute_task(&fx.router, "etl", "tr-docs", json!({"r": "main"}), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("is not a git workspace"),
        "{body}"
    );
    assert_eq!(df_job_count(&fx.pool, "tr-docs").await, 0);
    Ok(())
}

/// § 5.6 at creation: a `type: task` action whose own `ref:` is a template is
/// an ordinary task step with no task pin yet; the template is on the row
/// inside `action_spec`.
#[tokio::test]
async fn templated_task_action_ref_leaves_the_task_pin_empty_at_creation() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-task-action-later", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "pending");
    assert_eq!(call.action_type, "task");
    assert!(
        call.task_workspace.is_none() && call.task_ref.is_none() && call.task_revision.is_none()
    );
    assert_eq!(
        call.action_spec.unwrap()["ref"],
        json!("{{ input.branch }}")
    );
    assert!(call.ref_template.is_none());
    Ok(())
}
