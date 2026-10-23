//! A templated `ref:` that renders empty (after trim) means "no ref"
//! (`docs/superpowers/specs/2026-10-15-templated-refs-design.md` D6, option
//! A): the flow step, the `type: task` dispatch or the webhook job runs as if
//! the `ref:` key were absent.

use crate::common::pinned::*;
use crate::git_refs_deferred_test::{df_create, df_etl_main, df_etl_release, df_step};

use anyhow::Result;
use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{JobRefPinRepo, JobRepo, JobRow, JobStepRow};
use stroem_server::config::JobDefaults;
use stroem_server::render_context::Snapshots;
use stroem_server::settlement::dispatch::handle_task_steps;
use uuid::Uuid;

/// `etl` main: `type: task` actions whose templated ref reads the step input,
/// on tasks the default branch has.
const DE_MAIN_ACTIONS: &str = "\nactions:\n  call-child:\n    type: task\n    task: tr-child\n  call-child-at:\n    type: task\n    task: tr-child\n    ref: \"{{ input.branch }}\"\n  call-self-at:\n    type: task\n    task: tr-self-empty\n    ref: \"{{ input.branch }}\"\n";

/// Appended to `df_etl_main()`'s `tasks:` section (its last key), then a
/// webhook trigger.
const DE_MAIN_TASKS: &str = r#"
  tr-child:
    flow:
      a:
        action: hello
  tr-task-empty:
    flow:
      call:
        action: call-child-at
        input:
          branch: ""
  tr-self-empty:
    flow:
      call:
        action: call-self-at
        input:
          branch: "  "
  tr-task-empty-later:
    flow:
      first:
        action: hello
      call:
        action: call-child-at
        depends_on: [first]
        input:
          branch: ""
  tr-task-plain-later:
    flow:
      first:
        action: hello
      call:
        action: call-child
        depends_on: [first]
  tr-each-mixed:
    flow:
      run:
        action: hello
        ref: "{{ each.item }}"
        for_each: ["", "release/2.3"]
triggers:
  on-push-default:
    type: webhook
    name: push-default
    task: tr-child
    ref: "{{ input.body.ref | default(value='') }}"
"#;

/// `etl` release/2.3: a `type: task` action with a templated ref.
const DE_RELEASE_ACTIONS: &str = "\nactions:\n  call-nightly-tpl:\n    type: task\n    task: nightly\n    ref: \"{{ input.branch }}\"\n";

/// Appended to `df_etl_release()`'s `tasks:` section (its last key): the
/// consumer's spelling, `default(value='')`, with no `branch` input.
const DE_RELEASE_TASKS: &str = r#"
  tr-empty-root:
    flow:
      run:
        action: hello
        ref: "{{ input.branch | default(value='') }}"
  tr-empty-later:
    flow:
      first:
        action: hello
      later:
        action: hello
        ref: "{{ input.branch | default(value='') }}"
        depends_on: [first]
  tr-task-empty-pinned:
    flow:
      call:
        action: call-nightly-tpl
        input:
          branch: ""
  tr-task-empty-pinned-later:
    flow:
      first:
        action: hello
      call:
        action: call-nightly-tpl
        depends_on: [first]
        input:
          branch: ""
"#;

async fn de_fixture() -> Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(format!(
            "{}{DE_MAIN_TASKS}",
            df_etl_main().replacen("\nactions:\n", DE_MAIN_ACTIONS, 1)
        )),
        etl_release: Some(format!(
            "{}{DE_RELEASE_TASKS}",
            df_etl_release().replacen("\nactions:\n", DE_RELEASE_ACTIONS, 1)
        )),
        ..Default::default()
    })
    .await
}

/// The row of a resolved step that names no pin: what an un-ref'd step of the
/// job's own config gets.
fn assert_unrefd(step: &JobStepRow, script: &str) {
    let name = &step.step_name;
    assert_eq!(step.status, "ready", "{name}: {:?}", step.error_message);
    assert_eq!(step.action_type, "script", "{name}");
    assert_eq!(step.required_ability, "script", "{name}");
    assert_eq!(step.action_name, "hello", "{name}");
    assert_eq!(step.action_workspace, None, "{name}");
    assert_eq!(step.action_ref, None, "{name}");
    assert_eq!(step.action_revision, None, "{name}");
    assert_eq!(
        step.action_spec.as_ref().unwrap()["script"],
        json!(script),
        "{name}"
    );
}

async fn de_child_of(pool: &PgPool, parent: Uuid) -> JobRow {
    let id: Uuid = sqlx::query_scalar("SELECT job_id FROM job WHERE parent_job_id = $1")
        .bind(parent)
        .fetch_one(pool)
        .await
        .unwrap();
    JobRepo::get(pool, id).await.unwrap().unwrap()
}

/// A root step of a live job, resolved by `init`: the default branch's action,
/// the job's revision, no pin columns and no ref memo row. Whitespace-only is
/// empty too.
#[tokio::test]
async fn an_empty_ref_runs_a_root_step_of_a_live_job_unrefd() -> Result<()> {
    let fx = de_fixture().await?;
    let worker = register_worker(&fx.router, &["script"]).await;
    for branch in ["", "  \n\t"] {
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
        assert_unrefd(&run, "echo main");
        assert_eq!(run.ref_template.as_deref(), Some("{{ input.branch }}"));
        assert!(JobRefPinRepo::list_for_job(&fx.pool, job_id)
            .await?
            .is_empty());

        let claimed = claim_once(&fx.router, &worker).await;
        assert_eq!(claimed["job_id"], json!(job_id.to_string()), "{claimed}");
        assert_eq!(claimed["revision"], json!(fx.commits.etl_main));
        assert_eq!(claimed["action_spec"]["script"], json!("echo main"));
    }
    Ok(())
}

/// A later step of a live job, resolved when its dependency completes.
#[tokio::test]
async fn an_empty_ref_runs_a_later_step_of_a_live_job_unrefd() -> Result<()> {
    let fx = de_fixture().await?;
    let job_id = df_create(&fx, "tr-later", json!({"branch": ""})).await?;
    assert_eq!(
        df_step(&fx.pool, job_id, "later").await.action_type,
        "deferred"
    );
    let worker = register_worker(&fx.router, &["script"]).await;
    claim_and_complete(&fx, &worker, job_id, "first", json!({"output": {}})).await;
    assert_unrefd(&df_step(&fx.pool, job_id, "later").await, "echo main");
    assert!(JobRefPinRepo::list_for_job(&fx.pool, job_id)
        .await?
        .is_empty());
    let claimed = claim_once(&fx.router, &worker).await;
    assert_eq!(claimed["step_name"], json!("later"), "{claimed}");
    assert_eq!(claimed["revision"], json!(fx.commits.etl_main));
    Ok(())
}

/// `ws.action` with an empty ref: the cross-workspace rule, `ws`'s live
/// config and revision.
#[tokio::test]
async fn an_empty_ref_on_a_cross_workspace_action_runs_it_live() -> Result<()> {
    let fx = de_fixture().await?;
    let job_id = df_create(&fx, "tr-billing", json!({"tag": ""})).await?;
    let run = df_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.status, "ready", "{:?}", run.error_message);
    assert_eq!(run.action_type, "script");
    assert_eq!(run.action_workspace.as_deref(), Some("billing"));
    assert_eq!(run.action_name, "export");
    assert_eq!(run.action_ref, None);
    assert_eq!(
        run.action_revision.as_deref(),
        Some(fx.commits.billing_main.as_str())
    );
    assert_eq!(
        run.action_spec.as_ref().unwrap()["script"],
        json!("echo export-main")
    );
    assert!(JobRefPinRepo::list_for_job(&fx.pool, job_id)
        .await?
        .is_empty());
    Ok(())
}

/// In a pinned job an empty ref inherits the job's pin, like an un-ref'd step:
/// the action of the pinned config, no pin columns on the row, and no memo row
/// besides the job's own pin — for a root step and for a later one.
#[tokio::test]
async fn an_empty_ref_in_a_pinned_job_inherits_the_jobs_pin() -> Result<()> {
    let fx = de_fixture().await?;
    let worker = register_worker(&fx.router, &["script"]).await;
    let only_the_job_pin = |rows: Vec<stroem_db::JobRefPinRow>| {
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (
                rows[0].workspace.as_str(),
                rows[0].git_ref.as_str(),
                rows[0].commit.as_str()
            ),
            ("etl", "release/2.3", fx.commits.etl_release.as_str())
        );
    };

    let root = fx.create_pinned_etl_job("tr-empty-root").await?;
    assert_unrefd(&df_step(&fx.pool, root, "run").await, "echo v1");
    only_the_job_pin(JobRefPinRepo::list_for_job(&fx.pool, root).await?);
    let claimed = claim_once(&fx.router, &worker).await;
    assert_eq!(claimed["job_id"], json!(root.to_string()), "{claimed}");
    assert_eq!(claimed["revision"], json!(fx.commits.etl_release));
    assert_eq!(claimed["action_spec"]["script"], json!("echo v1"));

    let later = fx.create_pinned_etl_job("tr-empty-later").await?;
    assert_eq!(
        df_step(&fx.pool, later, "later").await.action_type,
        "deferred"
    );
    claim_and_complete(&fx, &worker, later, "first", json!({"output": {}})).await;
    assert_unrefd(&df_step(&fx.pool, later, "later").await, "echo v1");
    only_the_job_pin(JobRefPinRepo::list_for_job(&fx.pool, later).await?);
    let claimed = claim_once(&fx.router, &worker).await;
    assert_eq!(claimed["step_name"], json!("later"), "{claimed}");
    assert_eq!(claimed["revision"], json!(fx.commits.etl_release));
    Ok(())
}

/// Each `for_each` instance renders its own value: an empty one runs
/// un-ref'd, a non-empty one at its ref.
#[tokio::test]
async fn an_empty_ref_applies_per_for_each_instance() -> Result<()> {
    let fx = de_fixture().await?;
    let job_id = df_create(&fx, "tr-each-mixed", json!({})).await?;
    let first = df_step(&fx.pool, job_id, "run[0]").await;
    assert_eq!(first.status, "ready", "{:?}", first.error_message);
    assert_eq!(first.action_type, "script");
    assert_eq!(first.action_workspace, None);
    assert_eq!(first.action_ref, None);
    assert_eq!(
        first.action_spec.as_ref().unwrap()["script"],
        json!("echo main")
    );
    let second = df_step(&fx.pool, job_id, "run[1]").await;
    assert_eq!(second.status, "ready", "{:?}", second.error_message);
    assert_eq!(second.action_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        second.action_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

/// A `type: task` action whose ref renders empty in a live job: the task is
/// resolved live, no task pin is stamped, and the child is unpinned at the
/// parent's revision.
#[tokio::test]
async fn an_empty_task_action_ref_dispatches_an_unpinned_child_in_a_live_job() -> Result<()> {
    let fx = de_fixture().await?;
    let job_id = df_create(&fx, "tr-task-empty", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.action_type, "task");
    assert_eq!(call.status, "running", "{:?}", call.error_message);
    assert_eq!(call.task_workspace, None);
    assert_eq!(call.task_ref, None);
    assert_eq!(call.task_revision, None);
    let child = de_child_of(&fx.pool, job_id).await;
    assert_eq!(child.task_name, "tr-child");
    assert_eq!(child.workspace, "etl");
    assert_eq!(child.git_ref, None);
    assert_eq!(
        child.revision.as_deref(),
        Some(fx.commits.etl_main.as_str())
    );
    Ok(())
}

/// In a pinned job the task inherits the job's pin, as an un-ref'd
/// `type: task` action's does: the same stamp as `wrapper` (whose
/// `call-local` has no `ref:`), and a child pinned to the same commit.
#[tokio::test]
async fn an_empty_task_action_ref_inherits_the_pin_of_a_pinned_job() -> Result<()> {
    let fx = de_fixture().await?;
    let job_id = fx.create_pinned_etl_job("tr-task-empty-pinned").await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "running", "{:?}", call.error_message);
    let unrefd = fx.create_pinned_etl_job("wrapper").await?;
    let unrefd_call = df_step(&fx.pool, unrefd, "call").await;
    assert_eq!(unrefd_call.status, "running");
    assert_eq!(
        (
            call.task_workspace.as_deref(),
            call.task_ref.as_deref(),
            call.task_revision.as_deref()
        ),
        (
            unrefd_call.task_workspace.as_deref(),
            unrefd_call.task_ref.as_deref(),
            unrefd_call.task_revision.as_deref()
        )
    );
    assert_eq!(call.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        call.task_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    let child = de_child_of(&fx.pool, job_id).await;
    assert_eq!(child.task_name, "nightly");
    assert_eq!(child.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        child.revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

/// The static self-reference rule applies to an empty ref: the same task at
/// the job's own (absent) pin fails the step and creates no child.
#[tokio::test]
async fn an_empty_task_action_ref_to_the_jobs_own_task_is_a_self_reference() -> Result<()> {
    let fx = de_fixture().await?;
    let job_id = df_create(&fx, "tr-self-empty", json!({})).await?;
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "failed");
    assert_eq!(
        call.error_message.as_deref(),
        Some("[ref] step 'call': task 'tr-self-empty' is a self-reference to 'etl/tr-self-empty' (invalid)")
    );
    let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE parent_job_id = $1")
        .bind(job_id)
        .fetch_one(&fx.pool)
        .await?;
    assert_eq!(children, 0);
    assert_eq!(
        JobRepo::get(&fx.pool, job_id).await?.unwrap().status,
        "failed"
    );
    Ok(())
}

/// A webhook whose ref renders empty creates the job an un-ref'd trigger
/// would: unpinned, at the live revision.
#[tokio::test]
async fn an_empty_webhook_ref_creates_an_unpinned_job() -> Result<()> {
    let fx = de_fixture().await?;
    for body_in in [json!({}), json!({"ref": " "})] {
        let (status, body) = api_req(
            &fx.router,
            "POST",
            "/hooks/push-default",
            None,
            Some(body_in.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body_in}: {body}");
        let job_id: Uuid = body["job_id"].as_str().unwrap().parse()?;
        let job = JobRepo::get(&fx.pool, job_id).await?.unwrap();
        assert_eq!(job.task_name, "tr-child");
        assert_eq!(job.git_ref, None, "{body_in}");
        assert_eq!(job.task_folder, None, "{body_in}");
        assert_eq!(job.revision.as_deref(), Some(fx.commits.etl_main.as_str()));
        assert!(JobRefPinRepo::list_for_job(&fx.pool, job_id)
            .await?
            .is_empty());
    }
    Ok(())
}

// ─── The dispatch claim ────────────────────────────────────────────────────

/// `first` completed and `call` promoted to `ready` behind the cascade's back,
/// so no dispatcher has seen `call` yet.
async fn de_ready_undispatched(pool: &PgPool, job_id: Uuid) {
    for (step, status) in [("first", "completed"), ("call", "ready")] {
        sqlx::query("UPDATE job_step SET status = $3 WHERE job_id = $1 AND step_name = $2")
            .bind(job_id)
            .bind(step)
            .bind(status)
            .execute(pool)
            .await
            .unwrap();
    }
}

/// `n` dispatch passes over `job_id` at the same time, as concurrent
/// `advance` calls (two replicas, a completion and a recovery tick) run them.
async fn de_dispatch_concurrently(
    fx: &PinnedFixture,
    cfg: &WorkspaceConfig,
    job_id: Uuid,
    n: usize,
) -> Result<()> {
    let job = JobRepo::get(&fx.pool, job_id).await?.unwrap();
    let task = cfg.tasks.get(&job.task_name).expect("task in config");
    let snapshots = Snapshots::default();
    let passes = (0..n).map(|_| {
        handle_task_steps(
            fx.mgr(),
            &fx.pool,
            cfg,
            "etl",
            job_id,
            task,
            JobDefaults::default(),
            &snapshots,
        )
    });
    for result in futures_util::future::join_all(passes).await {
        result?;
    }
    Ok(())
}

async fn de_children(pool: &PgPool, parent: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM job WHERE parent_job_id = $1")
        .bind(parent)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Concurrent dispatchers of one ready `type: task` step create ONE child:
/// the dispatch claim has one winner, and the losers write nothing — for an
/// empty templated ref, for a task without `ref:`, and for an empty ref that
/// inherits the pin of a pinned job (stamped by the first dispatcher).
#[tokio::test]
async fn concurrent_dispatchers_create_one_child() -> Result<()> {
    let fx = de_fixture().await?;
    let live = fx.mgr().get_config("etl").await.expect("etl loaded");
    let pinned = fx
        .mgr()
        .pins()
        .ensure("etl", &fx.commits.etl_release)
        .await?
        .config
        .clone();
    for (task, is_pinned) in [
        ("tr-task-empty-later", false),
        ("tr-task-plain-later", false),
        ("tr-task-empty-pinned-later", true),
    ] {
        let (job_id, cfg) = if is_pinned {
            (fx.create_pinned_etl_job(task).await?, pinned.as_ref())
        } else {
            (df_create(&fx, task, json!({})).await?, live.as_ref())
        };
        de_ready_undispatched(&fx.pool, job_id).await;
        de_dispatch_concurrently(&fx, cfg, job_id, 4).await?;
        let call = df_step(&fx.pool, job_id, "call").await;
        assert_eq!(call.status, "running", "{task}: {:?}", call.error_message);
        assert_eq!(call.error_message, None, "{task}");
        assert_eq!(de_children(&fx.pool, job_id).await, 1, "{task}");
        let child = de_child_of(&fx.pool, job_id).await;
        if is_pinned {
            assert_eq!(call.task_ref.as_deref(), Some("release/2.3"), "{task}");
            assert_eq!(child.git_ref.as_deref(), Some("release/2.3"), "{task}");
        } else {
            assert_eq!(call.task_ref, None, "{task}");
            assert_eq!(child.git_ref, None, "{task}");
        }
    }
    Ok(())
}

/// A dispatcher that resolved an empty ref loses the claim once another one
/// has stamped a pin on the step, and cannot fail it either: it creates no
/// child and leaves the stamp.
#[tokio::test]
async fn a_dispatcher_without_a_pin_loses_to_a_stamp() -> Result<()> {
    let fx = de_fixture().await?;
    let job_id = df_create(&fx, "tr-task-empty-later", json!({})).await?;
    de_ready_undispatched(&fx.pool, job_id).await;
    // What a concurrent dispatcher that rendered a branch writes first.
    assert!(
        stroem_db::JobStepRepo::stamp_task_pin(
            &fx.pool,
            job_id,
            "call",
            "etl",
            "release/2.3",
            &fx.commits.etl_release,
        )
        .await?
    );
    assert!(
        !stroem_db::JobStepRepo::claim_task_dispatch(&fx.pool, job_id, "call", None).await?,
        "the empty-ref dispatcher's claim"
    );
    assert!(
        !stroem_db::JobStepRepo::fail_task_dispatch(
            &fx.pool,
            job_id,
            "call",
            "a stale failure",
            stroem_common::models::job::StepStatus::Ready,
            None,
        )
        .await?,
        "the empty-ref dispatcher's failure"
    );
    let call = df_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.status, "ready");
    assert_eq!(call.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(de_children(&fx.pool, job_id).await, 0);
    Ok(())
}
