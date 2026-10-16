//! Templated refs (`docs/superpowers/specs/2026-10-15-templated-refs-design.md`):
//! deferred steps, their resolution at promotion, the ref memo, the lease,
//! privacy of the rendered value, `type: task` and webhook templated refs.

use crate::common::capture::capture;
use crate::common::pinned::*;

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use stroem_db::{JobRefPinRepo, JobRepo, JobStepRepo, JobStepRow};
use stroem_server::config::JobDefaults;
use stroem_server::job_creator::{
    create_child_job_for_task_detailed, create_job_for_task_detailed,
};
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
