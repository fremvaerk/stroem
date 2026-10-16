//! Templated refs (`docs/superpowers/specs/2026-10-15-templated-refs-design.md`):
//! deferred steps, their resolution at promotion, the ref memo, the lease,
//! privacy of the rendered value, `type: task` and webhook templated refs.

use crate::common::capture::capture;
use crate::common::pinned::*;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use stroem_db::{JobRefPinRepo, JobRepo, JobStepRepo, JobStepRow};
use stroem_server::config::JobDefaults;
use stroem_server::job_creator::{
    create_child_job_for_task_detailed, create_job_for_task_detailed,
};
use stroem_server::recovery::{readvance_deferred_steps, StalledSweepBounds};
use stroem_server::render_context::Snapshots;
use stroem_server::settlement::deferred::{resolve_deferred_steps_with, LeaseTiming};
use stroem_server::state::AppState;
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

// ─── Resolution at promotion (spec § 5.3) ─────────────────────────────────

/// release/2.3 one commit later: `hello` now echoes v2.
pub(crate) fn df_release_v2(fx: &PinnedFixture) -> String {
    fx.etl.commit(
        "release/2.3",
        "release/2.3",
        &[(
            "workflow.yaml",
            &df_etl_release().replace("echo v1", "echo v2"),
        )],
    )
}

/// The headline case: a ROOT step with `ref: "{{ input.branch }}"` is
/// resolved by `init`, inside the creating request — no recovery wait.
#[tokio::test]
async fn root_templated_ref_is_resolved_at_creation() -> Result<()> {
    let fx = df_fixture().await?;
    // Review Focus 1: a trailing newline (a pasted form value) is trimmed.
    for branch in ["release/2.3", "release/2.3\n"] {
        let (status, body) = execute_task(
            &fx.router,
            "etl",
            "tr-input",
            json!({"branch": branch}),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
        let run = df_step(&fx.pool, job_id, "run").await;
        assert_eq!(run.status, "ready", "{branch:?}");
        assert_eq!(run.action_type, "script");
        assert_eq!(run.required_ability, "script");
        assert_eq!(run.runner, "local");
        assert_eq!(run.action_ref.as_deref(), Some("release/2.3"));
        assert_eq!(
            run.action_revision.as_deref(),
            Some(fx.commits.etl_release.as_str())
        );
        assert_eq!(
            run.action_spec.as_ref().unwrap()["script"],
            json!("echo v1")
        );
        assert_eq!(run.ref_template.as_deref(), Some("{{ input.branch }}"));
    }
    // The claim returns the branch's action and commit.
    let worker = register_worker(&fx.router, &["script"]).await;
    let claimed = claim_once(&fx.router, &worker).await;
    assert_eq!(claimed["revision"], json!(fx.commits.etl_release));
    assert_eq!(claimed["action_spec"]["script"], json!("echo v1"));
    Ok(())
}

#[tokio::test]
async fn ref_read_from_an_earlier_steps_output() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-output", json!({})).await?;
    assert_eq!(
        df_step(&fx.pool, job_id, "run").await.action_type,
        "deferred"
    );
    let worker = register_worker(&fx.router, &["script"]).await;
    claim_and_complete(
        &fx,
        &worker,
        job_id,
        "pick",
        json!({"output": {"branch": "release/2.3"}}),
    )
    .await;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "ready");
    assert_eq!(run.action_type, "script");
    assert_eq!(run.action_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        run.action_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

/// § 5.4: each instance renders its own `each`; equal values share a commit.
#[tokio::test]
async fn for_each_instances_resolve_their_own_values_and_share_equal_ones() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-each", json!({})).await?;
    for (i, want) in [
        (0, &fx.commits.etl_release),
        (1, &fx.commits.etl_main),
        (2, &fx.commits.etl_release),
    ] {
        let s = df_step(&fx.pool, job_id, &format!("run[{i}]")).await;
        assert_eq!(s.action_type, "script", "run[{i}]");
        assert_eq!(
            s.action_revision.as_deref(),
            Some(want.as_str()),
            "run[{i}]"
        );
    }
    let placeholder = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(
        placeholder.action_type, "deferred",
        "the placeholder never runs"
    );
    assert_eq!(placeholder.status, "running");
    let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
    let etl: Vec<&str> = memo
        .iter()
        .filter(|e| e.workspace == "etl")
        .map(|e| e.git_ref.as_str())
        .collect();
    assert_eq!(etl, vec!["main", "release/2.3"]);
    Ok(())
}

/// Success criterion 4: a static ref and a template rendering the same value
/// get ONE commit, even after the branch moves between creation and
/// promotion; so do two promotions of the same rendered value.
#[tokio::test]
async fn one_commit_per_ref_survives_a_branch_move() -> Result<()> {
    let fx = df_fixture().await?;
    let worker = register_worker(&fx.router, &["script"]).await;

    let job_id = df_create(&fx, "tr-memo", json!({})).await?;
    let v2 = df_release_v2(&fx);
    assert_ne!(v2, fx.commits.etl_release);
    claim_and_complete(&fx, &worker, job_id, "a", json!({"output": {}})).await;
    let b = df_step(&fx.pool, job_id, "b").await;
    assert_eq!(
        b.action_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    // A claim takes ANY ready step: finish job 1 so job 2's claims are exact.
    claim_and_complete(&fx, &worker, job_id, "b", json!({"output": {}})).await;

    let job2 = df_create(&fx, "tr-memo-two", json!({})).await?;
    claim_and_complete(&fx, &worker, job2, "x", json!({"output": {}})).await;
    let y = df_step(&fx.pool, job2, "y").await;
    assert_eq!(
        y.action_revision.as_deref(),
        Some(v2.as_str()),
        "y resolved at the tip"
    );
    let v3 = fx.etl.commit(
        "release/2.3",
        "release/2.3",
        &[(
            "workflow.yaml",
            &df_etl_release().replace("echo v1", "echo v3"),
        )],
    );
    claim_and_complete(&fx, &worker, job2, "y", json!({"output": {}})).await;
    claim_and_complete(&fx, &worker, job2, "z", json!({"output": {}})).await;
    let w = df_step(&fx.pool, job2, "w").await;
    assert_eq!(
        w.action_revision.as_deref(),
        Some(v2.as_str()),
        "w reuses y's commit, not {v3}"
    );
    Ok(())
}

/// Spec § 5.5: each resolution error fails ONLY that step with a fixed,
/// value-free sentence; the job settles; the execute call still returns the
/// job (D8: no pre-check); nothing failed reaches the memo.
#[tokio::test]
async fn resolution_failures_fail_the_step_with_a_value_free_message() -> Result<()> {
    let fx = df_fixture().await?;
    let cases: &[(&str, serde_json::Value, &str)] = &[
        ("tr-input", json!({"branch": "  "}), "[ref] step 'run': the rendered ref is empty"),
        (
            "tr-input",
            json!({"branch": "release/9.9"}),
            "[ref] step 'run': the rendered ref was not found in workspace 'etl'",
        ),
        (
            "tr-input",
            json!({"branch": "a..b"}),
            "[ref] step 'run': the rendered ref is not a valid branch, tag or full commit SHA",
        ),
        (
            "tr-missing-action",
            json!({}),
            "[ref] step 'run': workspace 'etl' has no such action or task at the rendered ref",
        ),
        (
            "tr-agent",
            json!({}),
            "[ref] step 'run': the action at the rendered ref is an agent action; agent actions cannot be referenced with `ref` yet",
        ),
        (
            "tr-billing",
            json!({"tag": "v9.9.9"}),
            "[ref] step 'run': the rendered ref was not found in workspace 'billing'",
        ),
    ];
    for (task, input, want) in cases {
        let (status, body) = execute_task(&fx.router, "etl", task, input.clone(), None).await;
        assert_eq!(status, StatusCode::OK, "{task} {input}: {body}");
        let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
        let run = df_step(&fx.pool, job_id, "run").await;
        assert_eq!(run.status, "failed", "{task} {input}");
        assert_eq!(run.error_message.as_deref(), Some(*want), "{task} {input}");
        assert_eq!(
            run.action_type, "deferred",
            "a failed resolution keeps the deferred row"
        );
        assert_eq!(
            JobRepo::get(&fx.pool, job_id).await?.unwrap().status,
            "failed"
        );
        let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
        assert!(
            memo.iter()
                .all(|e| e.git_ref != "release/9.9" && e.git_ref != "v9.9.9"),
            "{memo:?}"
        );
    }
    // The owner comes from the action name: `billing.export` at a tag.
    let job_id = df_create(&fx, "tr-billing", json!({"tag": "v4.1.0"})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_workspace.as_deref(), Some("billing"));
    assert_eq!(
        run.action_revision.as_deref(),
        Some(fx.commits.billing_tag.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn a_rendered_ref_holding_a_live_secret_is_refused() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(format!(
            "secrets:\n  deploy_token: tok-9f8e7d6c5b\n{}",
            df_etl_main()
        )),
        etl_release: Some(df_etl_release()),
        ..Default::default()
    })
    .await?;
    let job_id = df_create(&fx, "tr-input", json!({"branch": "feature-tok-9f8e7d6c5b"})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "failed");
    assert_eq!(
        run.error_message.as_deref(),
        Some("[ref] step 'run': the rendered ref contains a secret value")
    );
    let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
    assert!(memo.iter().all(|e| !e.git_ref.contains("tok-")), "{memo:?}");
    Ok(())
}

/// D7: a temporary git failure at promotion fails the step (no wait).
#[tokio::test]
async fn a_temporary_git_failure_fails_the_step() -> Result<()> {
    let fx = df_fixture().await?;
    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let (status, body) = execute_task(
        &replica.router,
        "etl",
        "tr-input",
        json!({"branch": "release/2.3"}),
        None,
    )
    .await;
    fx.etl.restore_remote();
    assert_eq!(status, StatusCode::OK, "{body}");
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "failed");
    assert_eq!(
        run.error_message.as_deref(),
        Some("[ref] step 'run': workspace 'etl' is not available at the rendered ref yet")
    );
    Ok(())
}

/// After resolution, a `type: task` step is dispatched in the same `init`,
/// its child pinned to the commit the template selected.
#[tokio::test]
async fn a_resolved_task_action_dispatches_a_pinned_child() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-task", json!({})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_type, "task");
    assert_eq!(run.status, "running", "dispatched after the pass");
    assert_eq!(run.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        run.task_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    let child: Uuid = sqlx::query_scalar("SELECT job_id FROM job WHERE parent_job_id = $1")
        .bind(job_id)
        .fetch_one(&fx.pool)
        .await?;
    let child = JobRepo::get(&fx.pool, child).await?.unwrap();
    assert_eq!(child.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        child.revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn a_resolved_approval_step_suspends() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-gate", json!({})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_type, "approval");
    assert_eq!(run.status, "suspended");
    Ok(())
}

/// An agent task-tool child whose only root step fails resolution at `init`
/// is born terminal (spec § 5.3 consequences).
#[tokio::test]
async fn an_agent_tool_child_whose_root_fails_resolution_is_born_terminal() -> Result<()> {
    let fx = df_fixture().await?;
    let parent = df_create(&fx, "tr-later", json!({})).await?;
    let cfg = fx.mgr().get_config("etl").await.expect("etl loaded");
    let created = create_child_job_for_task_detailed(
        fx.mgr(),
        &fx.pool,
        &cfg,
        "etl",
        "tr-input",
        json!({"branch": "release/9.9"}),
        "agent_tool",
        None,
        parent,
        "first",
        Some(&fx.commits.etl_main),
        JobDefaults::default(),
        None,
    )
    .await?;
    assert!(fx
        .state
        .settlement()
        .agent_child_created(created)
        .await
        .is_err());
    Ok(())
}

/// Review Focus 2: an object item renders JSON text (or a render error);
/// either way the instance fails and its message carries none of it.
#[tokio::test]
async fn for_each_over_objects_fails_each_instance_without_echoing_it() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-each-objects", json!({})).await?;
    let inst = df_step(&fx.pool, job_id, "run[0]").await;
    assert_eq!(inst.status, "failed");
    let msg = inst.error_message.unwrap();
    assert!(
        msg == "[ref] step 'run[0]': the rendered ref is not a valid branch, tag or full commit SHA"
            || msg.starts_with("[ref] step 'run[0]': the ref template could not be rendered"),
        "{msg}"
    );
    assert!(!msg.contains("release/2.3") && !msg.contains('{'), "{msg}");
    Ok(())
}

/// Review Focus 3: the flow step has no `retry:`; the resolved action's own
/// retry fills the row.
#[tokio::test]
async fn a_resolved_action_brings_its_own_retry() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-retry", json!({})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_type, "script");
    assert_eq!(run.max_retries, Some(2));
    assert_eq!(run.retry_backoff_secs, Some(1));
    assert_eq!(run.retry_strategy.as_deref(), Some("fixed"));
    Ok(())
}

// ─── Webhook trigger ref (spec § 6) ───────────────────────────────────────

const DF_HOOK: &str = r#"
triggers:
  on-push:
    type: webhook
    name: push-hook
    task: nightly
    ref: "{{ input.body.ref }}"
"#;

async fn df_hook_fixture(main_prefix: &str) -> Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(format!("{main_prefix}{}{DF_HOOK}", df_etl_main())),
        etl_release: Some(df_etl_release()),
        ..Default::default()
    })
    .await
}

async fn df_webhook_jobs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE source_type = 'webhook'")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn webhook_templated_ref_creates_a_pinned_job() -> Result<()> {
    let fx = df_hook_fixture("").await?;
    let (status, body) = api_req(
        &fx.router,
        "POST",
        "/hooks/push-hook",
        None,
        Some(json!({"ref": "release/2.3"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let job = JobRepo::get(&fx.pool, job_id).await?.unwrap();
    assert_eq!(job.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        job.revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    assert_eq!(job.task_folder.as_deref(), Some("nightlies"));
    Ok(())
}

#[tokio::test]
async fn webhook_templated_ref_errors_are_value_free_400s_and_create_nothing() -> Result<()> {
    let fx = df_hook_fixture("secrets:\n  hook_token: hk-77aa66bb99\n").await?;
    let cases = [
        (json!({"ref": "  "}), "the rendered ref is empty"),
        (
            json!({"ref": "release/9.9"}),
            "the rendered ref was not found in workspace 'etl'",
        ),
        (
            json!({"ref": "main"}),
            "workspace 'etl' has no such action or task at the rendered ref",
        ),
        (
            json!({"ref": "x-hk-77aa66bb99"}),
            "the rendered ref contains a secret value",
        ),
        (json!({}), "the ref template could not be rendered"),
    ];
    for (body_in, want) in cases {
        let (status, body) = api_req(
            &fx.router,
            "POST",
            "/hooks/push-hook",
            None,
            Some(body_in.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body_in}: {body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.starts_with(want), "{body_in}: {error}");
    }
    assert_eq!(df_webhook_jobs(&fx.pool).await, 0);
    Ok(())
}

/// A temporary git failure is a 500, and neither the response nor the server
/// log carries the rendered value.
#[tokio::test]
async fn webhook_unavailable_is_a_500_and_the_log_carries_no_value() -> Result<()> {
    const BRANCH: &str = "zz-hook-4242";
    let fx = df_hook_fixture("").await?;
    fx.etl.commit(
        BRANCH,
        "release/2.3",
        &[("workflow.yaml", &df_etl_release())],
    );
    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    let (captured, guard) = capture();
    let (status, body) = api_req(
        &replica.router,
        "POST",
        "/hooks/push-hook",
        None,
        Some(json!({"ref": BRANCH})),
    )
    .await;
    drop(guard);
    fx.etl.restore_remote();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(!body.to_string().contains(BRANCH), "{body}");
    let text = captured.text();
    assert!(
        text.contains("is not available at the rendered ref yet"),
        "{text}"
    );
    assert!(!text.contains(BRANCH), "{text}");
    assert_eq!(df_webhook_jobs(&fx.pool).await, 0);
    Ok(())
}

// ─── API (spec § 9) ───────────────────────────────────────────────────────

/// Job detail returns the template and the deferred kind of an unresolved
/// step; `ref_template` is content, not an identifier key (redacted as such).
#[tokio::test]
async fn job_detail_shows_the_template_of_a_deferred_step() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-later", json!({})).await?;
    let (status, detail) = api_req(
        &fx.router,
        "GET",
        &format!("/api/jobs/{job_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let later = detail["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["step_name"] == "later")
        .expect("later")
        .clone();
    assert_eq!(later["action_type"], json!("deferred"));
    assert_eq!(later["ref_template"], json!("{{ input.branch }}"));
    assert!(!stroem_server::redaction::STEP_IDENTIFIER_KEYS.contains(&"ref_template"));
    Ok(())
}

// ─── Resolution fixes (Task 10 fix round 1) ───────────────────────────────

/// Extra tasks appended to `df_etl_main()`'s `tasks:` section (its last key).
const DF_EXTRA_TASKS: &str = r#"
  tr-dotted:
    flow:
      run:
        action: etl.common-ish.thing
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-dotted-ws:
    flow:
      run:
        action: etl.billing.local
        ref: "{{ input.branch | default(value='release/2.3') }}"
  tr-each-seq-fail:
    flow:
      run:
        action: hello
        ref: "{{ each.item }}"
        for_each: ["a..b", "a..c", "a..d"]
        sequential: true
        continue_on_failure: true
"#;

/// Dotted local action keys of `etl` release/2.3: `common-ish` is not a
/// workspace; `billing` is a configured one.
const DF_DOTTED_ACTIONS: &str = "\nactions:\n  \"common-ish.thing\":\n    type: script\n    runner: local\n    script: echo dotted\n  \"billing.local\":\n    type: script\n    runner: local\n    script: echo shadow\n";

/// [`df_fixture`] plus [`DF_EXTRA_TASKS`] in main and [`DF_DOTTED_ACTIONS`] in
/// release/2.3. A variant, so the other tests' configs do not change.
async fn df_extra_fixture() -> Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(format!("{}{DF_EXTRA_TASKS}", df_etl_main())),
        etl_release: Some(df_etl_release().replacen("\nactions:\n", DF_DOTTED_ACTIONS, 1)),
        ..Default::default()
    })
    .await
}

/// A self-qualified dotted action (`etl.x.y` written in `etl`) keeps its owner
/// and its dotted local name at resolution, also when the local name's prefix
/// is another configured workspace (`billing`).
#[tokio::test]
async fn a_self_qualified_dotted_action_resolves_at_a_templated_ref() -> Result<()> {
    let fx = df_extra_fixture().await?;
    for (task, local, script) in [
        ("tr-dotted", "common-ish.thing", "echo dotted"),
        ("tr-dotted-ws", "billing.local", "echo shadow"),
    ] {
        let job_id = df_create(&fx, task, json!({})).await?;
        let run = df_step(&fx.pool, job_id, "run").await;
        assert_eq!(run.status, "ready", "{task}: {:?}", run.error_message);
        assert_eq!(run.action_type, "script", "{task}");
        assert_eq!(run.action_workspace.as_deref(), Some("etl"), "{task}");
        assert_eq!(run.action_name, local, "{task}");
        assert_eq!(run.action_ref.as_deref(), Some("release/2.3"), "{task}");
        assert_eq!(
            run.action_revision.as_deref(),
            Some(fx.commits.etl_release.as_str()),
            "{task}"
        );
        assert_eq!(run.action_spec.as_ref().unwrap()["script"], json!(script));
    }
    Ok(())
}

/// Spec § 5.3: the pass is bounded by the number of steps in the JOB, not in
/// the flow. A sequential loop with `continue_on_failure` whose instances all
/// fail resolution needs one pass per instance; all fail inside `init`.
#[tokio::test]
async fn every_sequential_instance_that_fails_resolution_fails_in_one_advance() -> Result<()> {
    let fx = df_extra_fixture().await?;
    let job_id = df_create(&fx, "tr-each-seq-fail", json!({})).await?;
    for i in 0..3 {
        let inst = df_step(&fx.pool, job_id, &format!("run[{i}]")).await;
        assert_eq!(inst.status, "failed", "run[{i}]");
        assert_eq!(
            inst.error_message,
            Some(format!(
                "[ref] step 'run[{i}]': the rendered ref is not a valid branch, tag or full commit SHA"
            ))
        );
    }
    Ok(())
}

// ─── Lease, concurrency, cancel (spec § 5.3 steps 0 and 7, D12) ───────────

/// A `tr-later` job whose `later` step is `ready` but NOT resolved — what a
/// crash between the cascade and the pass leaves (§ 5.8). No advance runs.
pub(crate) async fn df_promoted_unresolved(fx: &PinnedFixture, branch: &str) -> Result<Uuid> {
    let job_id = df_create(fx, "tr-later", json!({"branch": branch})).await?;
    sqlx::query(
        "UPDATE job_step SET status = 'completed', output = $2, completed_at = NOW() \
         WHERE job_id = $1 AND step_name = 'first'",
    )
    .bind(job_id)
    .bind(json!({}))
    .execute(&fx.pool)
    .await?;
    sqlx::query(
        "UPDATE job_step SET status = 'ready', ready_at = NOW() \
         WHERE job_id = $1 AND step_name = 'later'",
    )
    .bind(job_id)
    .execute(&fx.pool)
    .await?;
    sqlx::query("UPDATE job SET status = 'running' WHERE job_id = $1")
        .bind(job_id)
        .execute(&fx.pool)
        .await?;
    Ok(job_id)
}

/// Run the pass for a `tr-later` job on `state`'s replica.
pub(crate) async fn df_resolve_on(
    state: &AppState,
    job_id: Uuid,
    timing: LeaseTiming,
) -> Result<()> {
    let cfg = state
        .workspaces
        .get_config("etl")
        .await
        .expect("etl loaded");
    let task = cfg.tasks.get("tr-later").expect("tr-later").clone();
    resolve_deferred_steps_with(
        &state.workspaces,
        &state.pool,
        &cfg,
        "etl",
        job_id,
        &task,
        JobDefaults::default(),
        &Snapshots::default(),
        timing,
    )
    .await
}

pub(crate) async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition not reached within 10 s");
}

async fn df_lease_live(pool: &PgPool, job_id: Uuid) -> bool {
    sqlx::query_scalar(
        "SELECT COALESCE(resolve_lease_until > NOW(), false) FROM job_step \
         WHERE job_id = $1 AND step_name = 'later'",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

const SHORT: LeaseTiming = LeaseTiming {
    lease: Duration::from_secs(2),
    renew: Duration::from_millis(300),
};

/// Two concurrent resolvers: exactly one takes the lease and resolves; the
/// other skips the step; one commit.
#[tokio::test]
async fn two_concurrent_resolvers_resolve_once() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_promoted_unresolved(&fx, "release/2.3").await?;
    let replica = fx.second_replica().await?;
    let hold = replica.state.workspaces.pins().hold_loads_for_test();
    let state = replica.state.clone();
    let first =
        tokio::spawn(async move { df_resolve_on(&state, job_id, LeaseTiming::default()).await });
    wait_until(|| hold.entered() > 0).await;
    df_resolve_on(&fx.state, job_id, LeaseTiming::default()).await?;
    assert_eq!(
        df_step(&fx.pool, job_id, "later").await.action_type,
        "deferred"
    );
    drop(hold);
    first.await??;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.action_type, "script");
    assert_eq!(
        later.action_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

/// A resolve held past the lease length keeps its lease by renewal, and the
/// Phase 4.6 listing does not offer the step to a second resolver.
#[tokio::test]
async fn a_held_resolve_keeps_its_lease_by_renewal() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_promoted_unresolved(&fx, "release/2.3").await?;
    sqlx::query("UPDATE job_step SET ready_at = NOW() - interval '1 hour' WHERE job_id = $1")
        .bind(job_id)
        .execute(&fx.pool)
        .await?;
    let replica = fx.second_replica().await?;
    let hold = replica.state.workspaces.pins().hold_loads_for_test();
    let state = replica.state.clone();
    let resolver = tokio::spawn(async move { df_resolve_on(&state, job_id, SHORT).await });
    wait_until(|| hold.entered() > 0).await;
    tokio::time::sleep(Duration::from_secs(4)).await; // twice the lease
    assert!(
        df_lease_live(&fx.pool, job_id).await,
        "renewal kept the lease"
    );
    assert!(
        !JobRepo::get_jobs_with_stranded_deferred_steps(&fx.pool, 30.0, None, 100)
            .await?
            .contains(&job_id)
    );
    drop(hold);
    resolver.await??;
    assert_eq!(
        df_step(&fx.pool, job_id, "later").await.action_type,
        "script"
    );
    Ok(())
}

/// The token fence: a resolver whose lease was taken over writes nothing,
/// neither the resolved row nor a failure.
#[tokio::test]
async fn a_resolver_whose_lease_was_taken_over_cannot_write() -> Result<()> {
    let fx = df_fixture().await?;
    fx.etl
        .commit("zz-broken", "main", &[("workflow.yaml", DF_BROKEN)]);
    for branch in ["release/2.3", "zz-broken"] {
        let job_id = df_promoted_unresolved(&fx, branch).await?;
        let replica = fx.second_replica().await?;
        let hold = replica.state.workspaces.pins().hold_loads_for_test();
        let state = replica.state.clone();
        let resolver =
            tokio::spawn(
                async move { df_resolve_on(&state, job_id, LeaseTiming::default()).await },
            );
        wait_until(|| hold.entered() > 0).await;
        sqlx::query(
            "UPDATE job_step SET resolve_lease_owner = $2, \
                                 resolve_lease_until = NOW() + interval '1 hour' \
             WHERE job_id = $1 AND step_name = 'later'",
        )
        .bind(job_id)
        .bind(Uuid::new_v4())
        .execute(&fx.pool)
        .await?;
        drop(hold);
        resolver.await??;
        let later = df_step(&fx.pool, job_id, "later").await;
        assert_eq!(
            (later.status.as_str(), later.action_type.as_str()),
            ("ready", "deferred"),
            "{branch}"
        );
        assert!(later.error_message.is_none(), "{branch}");
    }
    Ok(())
}

/// The lease guard: dropping the resolver future (a cancelled request during
/// `init`) stops renewal, so the lease expires.
#[tokio::test]
async fn dropping_the_resolver_stops_renewal() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_promoted_unresolved(&fx, "release/2.3").await?;
    let replica = fx.second_replica().await?;
    let hold = replica.state.workspaces.pins().hold_loads_for_test();
    let state = replica.state.clone();
    let resolver = tokio::spawn(async move { df_resolve_on(&state, job_id, SHORT).await });
    wait_until(|| hold.entered() > 0).await;
    resolver.abort();
    let _ = resolver.await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !df_lease_live(&fx.pool, job_id).await,
        "no renewal after the drop"
    );
    drop(hold);
    Ok(())
}

/// A cancellation that commits while a resolution runs wins: the resolution
/// (success or failure) writes nothing and the step ends `cancelled`.
#[tokio::test]
async fn a_cancel_during_resolution_wins() -> Result<()> {
    let fx = df_fixture().await?;
    fx.etl
        .commit("zz-broken", "main", &[("workflow.yaml", DF_BROKEN)]);
    for branch in ["release/2.3", "zz-broken"] {
        let job_id = df_promoted_unresolved(&fx, branch).await?;
        let replica = fx.second_replica().await?;
        let hold = replica.state.workspaces.pins().hold_loads_for_test();
        let state = replica.state.clone();
        let resolver =
            tokio::spawn(
                async move { df_resolve_on(&state, job_id, LeaseTiming::default()).await },
            );
        wait_until(|| hold.entered() > 0).await;
        fx.state.settlement().cancel(job_id).await?;
        drop(hold);
        resolver.await??;
        let later = df_step(&fx.pool, job_id, "later").await;
        assert_eq!(later.status, "cancelled", "{branch}");
        assert_eq!(
            later.action_type, "deferred",
            "{branch}: nothing written after the cancel"
        );
    }
    Ok(())
}

// ─── Privacy of the rendered value (spec § 5.5, § 10.3) ───────────────────

/// A BRANCH value equal to a secret that exists only in the commit it selects
/// is refused after that commit's config loads; nothing is written; no
/// captured span, log line or error text carries the value.
#[tokio::test]
async fn a_value_equal_to_a_secret_of_the_selected_commit_is_refused_privately() -> Result<()> {
    const SNEAKY: &str = "zz-sekrit-5150";
    let fx = df_fixture().await?;
    fx.etl.commit(
        SNEAKY,
        "release/2.3",
        &[(
            "workflow.yaml",
            &format!("secrets:\n  hidden: {SNEAKY}\n{}", df_etl_release()),
        )],
    );
    let job_id = df_promoted_unresolved(&fx, SNEAKY).await?;
    let (captured, _guard) = capture();
    fx.state.settlement().advance(job_id).await?;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.status, "failed");
    assert_eq!(
        later.error_message.as_deref(),
        Some("[ref] step 'later': the rendered ref contains a secret value")
    );
    assert!(JobRefPinRepo::list_for_job(&fx.pool, job_id)
        .await?
        .is_empty());
    let text = captured.text();
    assert!(!text.contains(SNEAKY), "{text}");
    Ok(())
}

/// A load failure of a rendered ref's commit is labelled `ws (sha)` only.
#[tokio::test]
async fn a_rendered_refs_load_failure_names_the_commit_only() -> Result<()> {
    const BRANCH: &str = "zz-sentinel-9001";
    let fx = df_fixture().await?;
    let broken = fx
        .etl
        .commit(BRANCH, "main", &[("workflow.yaml", DF_BROKEN)]);
    let job_id = df_promoted_unresolved(&fx, BRANCH).await?;
    let (captured, _guard) = capture();
    fx.state.settlement().advance(job_id).await?;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(
        later.error_message.as_deref(),
        Some("[ref] step 'later': workspace 'etl' at the rendered ref cannot be loaded: its configuration does not load")
    );
    let text = captured.text();
    assert!(
        text.contains(&format!("[pin] etl ({})", &broken[..7])),
        "{text}"
    );
    assert!(!text.contains(BRANCH), "{text}");
    Ok(())
}

/// A full-SHA value is the accepted residual of § 10.3: it resolves, the
/// resolve span never names it, and only `commit` fields carry it.
#[tokio::test]
async fn a_full_sha_ref_resolves_and_only_commit_fields_name_it() -> Result<()> {
    let fx = df_fixture().await?;
    let sha = fx.commits.etl_release.clone();
    let job_id = df_promoted_unresolved(&fx, &sha).await?;
    let (captured, _guard) = capture();
    fx.state.settlement().advance(job_id).await?;
    let later = df_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.action_revision.as_deref(), Some(sha.as_str()));
    // `latest_snapshots` records its own `git_ref=None` (the job's pin, never
    // a rendered value); any other `git_ref` span field would carry the value.
    assert!(
        captured
            .span_fields("git_ref")
            .iter()
            .all(|f| f == "git_ref=None"),
        "{}",
        captured.text()
    );
    for line in captured.text().lines().filter(|l| l.contains(&sha)) {
        assert!(
            line.contains("commit="),
            "only commit fields may name the SHA: {line}"
        );
    }
    Ok(())
}

// ─── After resolution (spec § 5.7) ────────────────────────────────────────

/// Review Focus 4: a resolved step that fails retries in place on the same
/// commit; it is never deferred again.
#[tokio::test]
async fn a_resolved_step_retries_on_its_commit_without_re_resolution() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_create(&fx, "tr-retry", json!({})).await?;
    let worker = register_worker(&fx.router, &["script"]).await;
    let claimed = claim_once(&fx.router, &worker).await;
    assert_eq!(claimed["step_name"], json!("run"), "{claimed}");
    let (st, body) = worker_req(
        &fx.router,
        "POST",
        &format!("/worker/jobs/{job_id}/steps/run/complete"),
        Some(json!({"exit_code": 1, "error": "boom"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "ready", "retried in place");
    assert_eq!(run.retry_attempt, 1);
    assert_eq!(run.action_type, "script");
    assert_eq!(
        run.action_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    assert!(JobStepRepo::get_ready_deferred_steps(&fx.pool, job_id)
        .await?
        .is_empty());
    tokio::time::sleep(Duration::from_millis(1200)).await; // the action's 1 s delay
    let again = claim_once(&fx.router, &worker).await;
    assert_eq!(again["step_name"], json!("run"), "{again}");
    assert_eq!(again["revision"], json!(fx.commits.etl_release));
    Ok(())
}

/// Review Focus 5 (spec § 5.7): restart from the templated step resolves it
/// again (a newer commit); a carried templated row stays terminal and keeps
/// its deferred kind — the pass never touches it.
#[tokio::test]
async fn restart_re_resolves_a_templated_step_and_leaves_a_carried_one() -> Result<()> {
    let fx = df_fixture().await?;
    let worker = register_worker(&fx.router, &["script"]).await;
    let source = df_create(&fx, "tr-chain", json!({})).await?;
    claim_and_complete(&fx, &worker, source, "a", json!({"output": {}})).await;
    claim_and_complete(&fx, &worker, source, "b", json!({"output": {}})).await;
    assert_eq!(
        JobRepo::get(&fx.pool, source).await?.unwrap().status,
        "completed"
    );
    let v2 = df_release_v2(&fx);

    let (st, body) = api_req(
        &fx.router,
        "POST",
        &format!("/api/jobs/{source}/restart"),
        None,
        Some(json!({"from_step": "b"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let carried: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let a = df_step(&fx.pool, carried, "a").await;
    assert_eq!(
        (a.status.as_str(), a.action_type.as_str()),
        ("completed", "deferred")
    );
    assert!(a.carried_over);
    assert!(a.action_ref.is_none());

    let (st, body) = api_req(
        &fx.router,
        "POST",
        &format!("/api/jobs/{source}/restart"),
        None,
        Some(json!({"from_step": "a"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let rerun: Uuid = body["job_id"].as_str().unwrap().parse()?;
    let a = df_step(&fx.pool, rerun, "a").await;
    assert_eq!(a.action_type, "script");
    assert_eq!(a.action_revision.as_deref(), Some(v2.as_str()));
    Ok(())
}

// ─── Recovery Phase 4.6 (spec § 5.8) ──────────────────────────────────────

/// Age `later` past the unmatched timeout; with `lease_secs`, give it a lease
/// of another (dead or live) resolver ending that many seconds from now.
async fn df_age_and_lease(pool: &PgPool, job_id: Uuid, lease_secs: Option<f64>) {
    sqlx::query("UPDATE job_step SET ready_at = NOW() - interval '1 hour' WHERE job_id = $1 AND step_name = 'later'")
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
    if let Some(secs) = lease_secs {
        sqlx::query(
            "UPDATE job_step SET resolve_lease_owner = $3, \
                                 resolve_lease_until = NOW() + make_interval(secs => $2::double precision) \
             WHERE job_id = $1 AND step_name = 'later'",
        )
        .bind(job_id)
        .bind(secs)
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn phase_4_6_resolves_stranded_and_expired_but_not_leased_steps() -> Result<()> {
    let fx = df_fixture().await?;
    let stranded = df_promoted_unresolved(&fx, "release/2.3").await?;
    let expired = df_promoted_unresolved(&fx, "release/2.3").await?;
    let leased = df_promoted_unresolved(&fx, "release/2.3").await?;
    df_age_and_lease(&fx.pool, stranded, None).await;
    df_age_and_lease(&fx.pool, expired, Some(-1.0)).await;
    df_age_and_lease(&fx.pool, leased, Some(3600.0)).await;

    readvance_deferred_steps(&fx.state, StalledSweepBounds::default()).await;

    assert_eq!(
        df_step(&fx.pool, stranded, "later").await.action_type,
        "script"
    );
    assert_eq!(
        df_step(&fx.pool, expired, "later").await.action_type,
        "script"
    );
    assert_eq!(
        df_step(&fx.pool, leased, "later").await.action_type,
        "deferred"
    );
    Ok(())
}

#[tokio::test]
async fn phase_4_6_is_bounded_and_resumes_on_the_next_sweep() -> Result<()> {
    let fx = df_fixture().await?;
    let one = df_promoted_unresolved(&fx, "release/2.3").await?;
    let two = df_promoted_unresolved(&fx, "release/2.3").await?;
    df_age_and_lease(&fx.pool, one, None).await;
    df_age_and_lease(&fx.pool, two, None).await;
    let bounds = StalledSweepBounds {
        max_jobs: 10,
        budget: Duration::ZERO,
    };
    readvance_deferred_steps(&fx.state, bounds).await;
    let resolved =
        |rows: [JobStepRow; 2]| rows.iter().filter(|r| r.action_type == "script").count();
    let after_one = resolved([
        df_step(&fx.pool, one, "later").await,
        df_step(&fx.pool, two, "later").await,
    ]);
    assert_eq!(after_one, 1, "the budget stops after the first job");
    readvance_deferred_steps(&fx.state, bounds).await;
    let after_two = resolved([
        df_step(&fx.pool, one, "later").await,
        df_step(&fx.pool, two, "later").await,
    ]);
    assert_eq!(after_two, 2, "the next sweep resumes after the cursor");
    Ok(())
}

/// A resolver dropped mid-resolve (Task 11) leaves a leased row; once the
/// lease expires, Phase 4.6 resolves it.
#[tokio::test]
async fn phase_4_6_resolves_the_row_of_a_dropped_resolver_after_expiry() -> Result<()> {
    let fx = df_fixture().await?;
    let job_id = df_promoted_unresolved(&fx, "release/2.3").await?;
    df_age_and_lease(&fx.pool, job_id, None).await;
    let replica = fx.second_replica().await?;
    let hold = replica.state.workspaces.pins().hold_loads_for_test();
    let state = replica.state.clone();
    let resolver = tokio::spawn(async move { df_resolve_on(&state, job_id, SHORT).await });
    wait_until(|| hold.entered() > 0).await;
    resolver.abort();
    let _ = resolver.await;
    drop(hold);
    tokio::time::sleep(Duration::from_secs(3)).await;
    readvance_deferred_steps(&fx.state, StalledSweepBounds::default()).await;
    assert_eq!(
        df_step(&fx.pool, job_id, "later").await.action_type,
        "script"
    );
    Ok(())
}

// ─── A `type: task` action's templated ref (spec § 5.6) ───────────────────

/// main: actions whose own `ref:` is a template, and tasks that call them.
fn df_task_action_main() -> String {
    format!(
        "{}{}",
        df_etl_main().replace(
            "actions:\n",
            "actions:\n  call-json-at:\n    type: task\n    task: takes-json\n    ref: \"{{ input.branch }}\"\n  call-default-at:\n    type: task\n    task: nightly\n    ref: \"{{ input.branch }}\"\n    input:\n      branch:\n        type: string\n        default: release/2.3\n",
        ),
        r#"
  tr-json:
    input:
      payload:
        type: json
        default: {"a": 1}
    flow:
      call:
        action: call-json-at
        input:
          branch: release/2.3
          cfg: "{{ input.payload }}"
  tr-default:
    flow:
      call:
        action: call-default-at
  tr-nested-bad:
    flow:
      call:
        action: call-nightly-at
        input:
          branch: release/2.3
          conn:
            host: "{{ bad"
  tr-nested-raw:
    flow:
      call:
        action: call-nightly-at
        input:
          branch: release/2.3
          conn:
            host: "{% raw %}{{ bad{% endraw %}"
  tr-cross:
    flow:
      call:
        action: billing.call-at
"#
    )
}

/// release/2.3 plus `takes-json` (a `json` input) and `self-call` (a task
/// that calls itself through a templated ref) at the end of `tasks:`.
fn df_task_action_release() -> String {
    format!(
        "{}  takes-json:\n    input:\n      cfg:\n        type: json\n    flow:\n      s:\n        action: hello\n  self-call:\n    flow:\n      call:\n        action: call-self-at\n        input:\n          branch: release/2.3\n",
        df_etl_release().replace(
            "actions:\n",
            "actions:\n  call-self-at:\n    type: task\n    task: self-call\n    ref: \"{{ input.branch }}\"\n",
        )
    )
}

async fn df_task_action_fixture() -> Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(df_task_action_main()),
        etl_release: Some(df_task_action_release()),
        billing_main: Some(BILLING_MAIN.replace(
            "actions:\n",
            "actions:\n  call-at:\n    type: task\n    task: nightly\n    ref: \"{{ input.nope }}\"\n",
        )),
        ..Default::default()
    })
    .await
}

async fn df_child_of(pool: &PgPool, parent: Uuid) -> stroem_db::JobRow {
    let id: Uuid = sqlx::query_scalar("SELECT job_id FROM job WHERE parent_job_id = $1")
        .bind(parent)
        .fetch_one(pool)
        .await
        .unwrap();
    JobRepo::get(pool, id).await.unwrap().unwrap()
}

#[tokio::test]
async fn templated_task_action_ref_resolves_at_dispatch_and_stamps_the_pin() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-task-action", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "running", "{:?}", call.error_message);
    assert_eq!(call.task_workspace.as_deref(), Some("etl"));
    assert_eq!(call.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        call.task_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    let child = df_child_of(&fx.pool, job_id).await;
    assert_eq!(child.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        child.revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    let memo = JobRefPinRepo::list_for_job(&fx.pool, job_id).await?;
    assert!(memo.iter().any(|e| e.git_ref == "release/2.3"), "{memo:?}");
    Ok(())
}

/// The ref reads the caller's step input as strings; the child's `json`
/// field is still a native value (T's schema decides the real render).
#[tokio::test]
async fn the_ref_input_is_schema_free_and_the_child_keeps_native_json() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-json", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "running", "{:?}", call.error_message);
    let child = df_child_of(&fx.pool, job_id).await;
    assert_eq!(child.input.unwrap()["cfg"], json!({"a": 1}));
    Ok(())
}

/// Action defaults are not visible to the ref (spec § 5.6 consequences).
#[tokio::test]
async fn action_defaults_are_not_in_the_ref_context() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-default", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "failed");
    let msg = call.error_message.unwrap();
    assert!(
        msg.starts_with("[ref] step 'call': the ref template could not be rendered"),
        "{msg}"
    );
    Ok(())
}

/// A nested leaf that does not render fails the step, naming the field only;
/// the `{% raw %}` escape lets it run (spec § 5.6 known limitation).
#[tokio::test]
async fn a_nested_render_error_fails_the_step_and_raw_escapes_it() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-nested-bad", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "failed");
    let msg = call.error_message.unwrap();
    assert!(msg.contains("input field 'conn'"), "{msg}");
    assert!(!msg.contains("{{ bad"), "{msg}");
    let job_id = df_create(&fx, "tr-nested-raw", json!({})).await?;
    assert_eq!(df_step(&fx.pool, job_id, "call").await.status, "running");
    Ok(())
}

/// The template is the ACTION OWNER's: its render error is withheld when the
/// owner (billing) is not the job's workspace (etl).
#[tokio::test]
async fn an_owner_side_ref_render_error_is_withheld_across_workspaces() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = df_create(&fx, "tr-cross", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "failed");
    assert_eq!(
        call.error_message.as_deref(),
        Some(
            "Failed to prepare input for task step 'call': rendering in workspace 'billing' failed \
             (details withheld from this job; see the server log or validate the owner workspace)"
        )
    );
    Ok(())
}

/// A templated task ref that renders the job's own task at the job's own pin
/// is a self-reference, refused like a static one at creation; no child job
/// is created (recursion would otherwise stop only at `MAX_TASK_DEPTH`).
#[tokio::test]
async fn a_templated_task_ref_to_the_jobs_own_task_and_pin_is_a_self_reference() -> Result<()> {
    let fx = df_task_action_fixture().await?;
    let job_id = fx.create_pinned_etl_job("self-call").await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "failed");
    assert_eq!(
        call.error_message.as_deref(),
        Some(
            "[ref] step 'call': task 'self-call' is a self-reference to 'etl/self-call' (invalid)"
        )
    );
    let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE parent_job_id = $1")
        .bind(job_id)
        .fetch_one(&fx.pool)
        .await?;
    assert_eq!(children, 0);
    Ok(())
}
