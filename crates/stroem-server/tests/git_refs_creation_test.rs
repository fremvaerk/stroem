//! Git refs (`docs/superpowers/specs/2026-10-02-git-refs-design.md`), Part C:
//! ref resolution and stamping at creation, the role-scoped pre-check, and
//! pinned jobs through settlement, dispatch, hooks, retry and recovery.

mod common;
use common::pinned::*;

use anyhow::Result;
use serde_json::json;
use stroem_common::git_ref::GitRefError;
use stroem_db::{JobRepo, JobRow, JobStepRepo, JobStepRow};
use stroem_server::config::JobDefaults;
use stroem_server::job_creator::{
    create_child_job_for_task_detailed, create_job_for_task_detailed, create_job_for_task_pinned,
    CreationMode,
};
use stroem_server::refs::RefResolveError;
use stroem_server::workspace::pins::{PinError, PinLoadWithheld};
use uuid::Uuid;

/// An ordinary (unpinned) job of `task` in the live `etl` config.
async fn cr_create_etl(fx: &PinnedFixture, task: &str) -> Result<Uuid> {
    let cfg = fx.mgr().get_config("etl").await.expect("etl loaded");
    create_job_for_task_detailed(
        fx.mgr(),
        &fx.pool,
        &cfg,
        "etl",
        task,
        json!({}),
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

/// A top-level job of `task` pinned to etl@release/2.3 at fixture creation.
async fn cr_create_pinned_etl(fx: &PinnedFixture, task: &str) -> Result<Uuid> {
    let pinned = fx
        .mgr()
        .pins()
        .ensure("etl", &fx.commits.etl_release)
        .await?;
    create_job_for_task_pinned(
        fx.mgr(),
        &fx.pool,
        &pinned.config,
        "etl",
        task,
        json!({}),
        "trigger",
        None,
        &fx.commits.etl_release,
        "release/2.3",
        CreationMode::Normal,
        None,
        JobDefaults::default(),
    )
    .await
    .map(|c| c.job_id)
}

async fn cr_step(pool: &sqlx::PgPool, job_id: Uuid, name: &str) -> JobStepRow {
    JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.step_name == name)
        .unwrap_or_else(|| panic!("step {name} exists"))
}

async fn cr_jobs_where(pool: &sqlx::PgPool, sql: &str, id: Uuid) -> Vec<JobRow> {
    let ids: Vec<Uuid> = sqlx::query_scalar(sql)
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap();
    let mut out = Vec::new();
    for id in ids {
        out.push(JobRepo::get(pool, id).await.unwrap().unwrap());
    }
    out
}

/// release/2.3 one commit later: `nightly` gains step `c`.
fn cr_etl_release_v2() -> String {
    ETL_RELEASE.replace(
        "      # nightly-end\n",
        "      c:\n        action: hello\n        depends_on: [b]\n",
    )
}

/// `etl` main plus two tasks: one whose step `ref` is not a valid ref name,
/// one whose step refs branch `broken` (a commit whose config does not load).
fn cr_etl_main_with_bad_refs() -> String {
    format!(
        "{ETL_MAIN}  uses-bad-ref-syntax:\n    flow:\n      run:\n        action: hello\n        \
         ref: \" v1\"\n  uses-broken-branch:\n    flow:\n      run:\n        action: hello\n        \
         ref: broken\n"
    )
}

/// `etl` release/2.3 plus a task that names its own workspace explicitly,
/// without `ref`: `etl.hello` and a `type: task` action on `etl.nightly`.
fn cr_etl_release_self_qualified() -> String {
    format!(
        "{}  self-qualified:\n    flow:\n      run:\n        action: etl.hello\n      call:\n        \
         action: call-self\n",
        ETL_RELEASE.replace(
            "actions:\n  hello:\n",
            "actions:\n  call-self:\n    type: task\n    task: etl.nightly\n  hello:\n",
        )
    )
}

/// A commit whose config does not load: its connection renders an undefined
/// secret, so the loader itself fails (not a per-file warning).
const CR_BROKEN: &str = r#"
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

// ─── Task 9: stamping at creation ──────────────────────────────────────────

/// F1: the fixture refuses a commit the loader only half-loads. A
/// `properties:` wrapper under a connection type (`ConnectionTypeDef` is
/// transparent) makes the loader skip the whole file with just a warning.
#[tokio::test]
async fn fixture_rejects_a_commit_that_loads_with_warnings() -> Result<()> {
    let wrapped = ETL_RELEASE.replace(
        "  pg:\n    host:\n      type: string\n",
        "  pg:\n    properties:\n      host:\n        type: string\n",
    );
    assert_ne!(wrapped, ETL_RELEASE, "the splice point moved");
    let result = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_release: Some(wrapped),
        ..Default::default()
    })
    .await;
    match result {
        Ok(_) => panic!("the fixture accepted a commit that loads with warnings"),
        Err(err) => assert!(
            format!("{err:#}").contains("loads with warnings"),
            "{err:#}"
        ),
    }
    Ok(())
}

/// F2: the live `etl`/`billing` entries are real clones of `main`: `reload`
/// picks up a new `main` commit, and the live path is a git checkout. `docs`
/// stays in memory: configured, not git.
#[tokio::test]
async fn fixture_live_git_workspaces_reload_new_main_commits() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    assert_eq!(
        fx.mgr().get_revision("etl").as_deref(),
        Some(fx.commits.etl_main.as_str())
    );
    assert!(fx.mgr().get_path("etl").unwrap().join(".git").is_dir());

    let next = fx.etl.commit(
        "main",
        "main",
        &[(
            "workflow.yaml",
            &ETL_MAIN.replace("echo main", "echo main-2"),
        )],
    );
    fx.mgr().reload("etl").await?;
    assert_eq!(fx.mgr().get_revision("etl").as_deref(), Some(next.as_str()));
    let cfg = fx.mgr().get_config("etl").await.expect("etl loaded");
    assert_eq!(
        serde_json::to_value(&cfg.actions["hello"])?["script"],
        json!("echo main-2")
    );

    assert!(fx.mgr().get_config("docs").await.is_some());
    assert!(!fx.mgr().pins().is_git("docs"));
    Ok(())
}

#[tokio::test]
async fn own_workspace_ref_stamps_action_pin() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_etl(&fx, "uses-release-import").await?;
    let run = cr_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_workspace.as_deref(), Some("etl"));
    assert_eq!(run.action_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        run.action_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    assert_eq!(run.action_name, "import");
    assert_eq!(run.action_spec.unwrap()["script"], json!("echo import-v1"));
    assert!(run.task_workspace.is_none() && run.task_ref.is_none());
    Ok(())
}

#[tokio::test]
async fn cross_workspace_tag_ref_stamps_owner_pin() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_etl(&fx, "uses-billing-tag").await?;
    let run = cr_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_workspace.as_deref(), Some("billing"));
    assert_eq!(run.action_ref.as_deref(), Some("v4.1.0"));
    assert_eq!(
        run.action_revision.as_deref(),
        Some(fx.commits.billing_tag.as_str())
    );
    assert_ne!(fx.commits.billing_tag, fx.commits.billing_main);
    assert_eq!(run.action_spec.unwrap()["script"], json!("echo export-v4"));
    Ok(())
}

#[tokio::test]
async fn type_task_action_with_ref_stamps_task_pin() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_etl(&fx, "manifest").await?;

    let call = cr_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.task_workspace.as_deref(), Some("etl"));
    assert_eq!(call.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        call.task_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    // The action itself (`call-nightly`) is local to main: no action pin.
    assert!(call.action_ref.is_none() && call.action_workspace.is_none());

    let later = cr_step(&fx.pool, job_id, "later").await;
    assert_eq!(
        later.action_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );

    let first = cr_step(&fx.pool, job_id, "first").await;
    assert!(first.action_ref.is_none() && first.task_ref.is_none());
    Ok(())
}

#[tokio::test]
async fn pinned_job_row_carries_ref_commit_and_folder() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "nightly").await?;
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
async fn local_task_action_in_pinned_job_inherits_pin() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "wrapper").await?;
    let call = cr_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.task_workspace.as_deref(), Some("etl"));
    assert_eq!(call.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        call.task_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn live_foreign_task_action_in_pinned_job_is_not_stamped() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "wrapper-foreign").await?;
    let call = cr_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.action_workspace.as_deref(), Some("billing"));
    assert!(call.action_ref.is_none());
    assert!(
        call.task_workspace.is_none() && call.task_ref.is_none() && call.task_revision.is_none()
    );
    Ok(())
}

/// F36 (spec § 4.3): inside a pinned job, a name that spells out the job's
/// own workspace (`etl.hello`, `task: etl.nightly`) and has no `ref` resolves
/// to that workspace, so it inherits the job pin — it is read from the pinned
/// config, never the live one. Live `main` has a different `hello` and no
/// `nightly` at all.
#[tokio::test]
async fn self_qualified_names_in_pinned_job_inherit_the_job_pin() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_release: Some(cr_etl_release_self_qualified()),
        ..Default::default()
    })
    .await?;
    let job_id = cr_create_pinned_etl(&fx, "self-qualified").await?;

    let run = cr_step(&fx.pool, job_id, "run").await;
    assert_eq!(run.action_name, "hello");
    assert_eq!(run.action_spec.unwrap()["script"], json!("echo v1"));
    assert_eq!(run.action_workspace.as_deref(), Some("etl"));
    assert_eq!(run.action_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        run.action_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );

    let call = cr_step(&fx.pool, job_id, "call").await;
    assert_eq!(call.task_workspace.as_deref(), Some("etl"));
    assert_eq!(call.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        call.task_revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn ref_to_agent_action_is_rejected() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let err = cr_create_etl(&fx, "uses-release-agent").await.unwrap_err();
    assert_eq!(
        err.downcast_ref::<RefResolveError>(),
        Some(&RefResolveError::AgentAction("ask".to_string()))
    );
    Ok(())
}

#[tokio::test]
async fn ref_to_missing_action_names_the_ref() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let err = cr_create_etl(&fx, "uses-missing-action").await.unwrap_err();
    // Outermost message keeps "has no action" so classify_execute_error → 400.
    assert!(err.to_string().contains("has no action 'nope'"), "{err:#}");
    assert!(err.to_string().contains("at ref 'release/2.3'"), "{err:#}");
    Ok(())
}

#[tokio::test]
async fn ref_to_missing_branch_is_ref_not_found() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let err = cr_create_etl(&fx, "uses-missing-branch").await.unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<PinError>(),
            Some(PinError::RefNotFound { .. })
        ),
        "{err:#}"
    );
    Ok(())
}

#[tokio::test]
async fn ref_on_folder_workspace_is_not_git() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let err = cr_create_etl(&fx, "uses-docs-ref").await.unwrap_err();
    assert_eq!(
        err.downcast_ref::<RefResolveError>(),
        Some(&RefResolveError::NotGit("docs".to_string()))
    );
    Ok(())
}

/// F43: a `ref:` is parsed before it is resolved, so a malformed one is a
/// syntax error ("invalid ref name"), not a ref that was not found.
#[tokio::test]
async fn malformed_ref_is_an_invalid_ref_name() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(cr_etl_main_with_bad_refs()),
        ..Default::default()
    })
    .await?;
    let err = cr_create_etl(&fx, "uses-bad-ref-syntax").await.unwrap_err();
    assert_eq!(
        err.downcast_ref::<GitRefError>(),
        Some(&GitRefError::InvalidName(" v1".to_string())),
        "{err:#}"
    );
    assert!(err.to_string().contains("invalid ref name"), "{err:#}");

    let (status, body) =
        execute_task(&fx.router, "etl", "uses-bad-ref-syntax", json!({}), None).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("invalid ref name"),
        "{body}"
    );
    Ok(())
}

/// T6 review #9: a `PinLoadFailed` message is the raw loader chain and can
/// quote secret values, so neither the creation error nor the HTTP body
/// carries it — only the fixed sentence.
#[tokio::test]
async fn pin_load_failure_shows_only_the_fixed_sentence() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(cr_etl_main_with_bad_refs()),
        ..Default::default()
    })
    .await?;
    let broken = fx
        .etl
        .commit("broken", "main", &[("workflow.yaml", CR_BROKEN)]);
    let sentence = format!(
        "[pin] etl@broken ({}) cannot be loaded: its configuration does not load",
        &broken[..7]
    );

    let err = cr_create_etl(&fx, "uses-broken-branch").await.unwrap_err();
    assert!(err.downcast_ref::<PinLoadWithheld>().is_some(), "{err:#}");
    assert_eq!(format!("{err:#}"), sentence);

    let (status, body) =
        execute_task(&fx.router, "etl", "uses-broken-branch", json!({}), None).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!(sentence));
    Ok(())
}

/// The execute API answers 400 for an author mistake in a ref (spec § 8).
#[tokio::test]
async fn execute_api_classifies_ref_errors_as_bad_request() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    for task in [
        "uses-release-agent",
        "uses-missing-action",
        "uses-missing-branch",
        "uses-docs-ref",
    ] {
        let (status, body) = execute_task(&fx.router, "etl", task, json!({}), None).await;
        assert_eq!(
            status,
            axum::http::StatusCode::BAD_REQUEST,
            "{task}: {body}"
        );
    }
    Ok(())
}

/// Review Focus 2: a cold replica whose pin cannot load (remote down) answers
/// 500 (`PinUnavailable`) and creates NO job row — pins are resolved and
/// ensured inside the step loop, before the creation transaction opens.
#[tokio::test]
async fn cold_replica_with_remote_down_answers_500_without_a_job_row() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let replica = fx.second_replica().await?;
    fx.etl.break_remote();

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job")
        .fetch_one(&fx.pool)
        .await?;
    let (status, body) = execute_task(
        &replica.router,
        "etl",
        "uses-release-import",
        json!({}),
        None,
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "{body}"
    );
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job")
        .fetch_one(&fx.pool)
        .await?;
    assert_eq!(
        before, after,
        "a pin failure must not leave a job row behind"
    );

    fx.etl.restore_remote();
    Ok(())
}

// ─── Task 10: role-scoped pre-check ────────────────────────────────────────

#[tokio::test]
async fn ref_step_literal_precheck_runs_against_owner_commit() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    // `release-db` exists only at release/2.3 — main has no connections at all.
    cr_create_etl(&fx, "uses-release-db").await?;

    let err = cr_create_etl(&fx, "uses-release-bad-db").await.unwrap_err();
    assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
    Ok(())
}

/// `etl` release/2.3 plus two tasks whose LOCAL `query` step names a
/// connection literally: `release-db` exists only at this commit (live main
/// has no connections), `nope-db` nowhere.
fn cr_etl_release_with_db_tasks() -> String {
    format!(
        "{ETL_RELEASE}  pinned-db:\n    flow:\n      run:\n        action: query\n        input:\n          \
         db: release-db\n  pinned-bad-db:\n    flow:\n      run:\n        action: query\n        \
         input:\n          db: nope-db\n"
    )
}

/// A pinned job's own literals are pre-checked against the job's commit, not
/// skipped and not checked against live main.
#[tokio::test]
async fn pinned_job_literal_precheck_runs_against_job_commit() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_release: Some(cr_etl_release_with_db_tasks()),
        ..Default::default()
    })
    .await?;
    cr_create_pinned_etl(&fx, "pinned-db").await?;

    let err = cr_create_pinned_etl(&fx, "pinned-bad-db")
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
    Ok(())
}

// ─── Task 11: settlement, dispatch, hooks, retry, agent tools, recovery ────

/// Spec § 4.4 / D5: a branch moving mid-job never splits the job across two
/// commits. The child task and a later ref'd step keep the creation-time commit.
#[tokio::test]
async fn branch_move_mid_job_keeps_original_commit() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let c1 = fx.commits.etl_release.clone();
    let job_id = cr_create_etl(&fx, "manifest").await?;

    // release/2.3 moves to C2, whose `nightly` has a third step.
    let c2 = fx.etl.commit(
        "release/2.3",
        "main",
        &[("workflow.yaml", &cr_etl_release_v2())],
    );
    assert_eq!(
        fx.mgr().pins().resolve("etl", "release/2.3").await?.commit,
        c2
    );

    JobStepRepo::mark_completed(&fx.pool, job_id, "first", None).await?;
    fx.state.settlement().advance(job_id).await?;

    let children = JobRepo::list_children(&fx.pool, job_id).await?;
    assert_eq!(children.len(), 1, "{children:?}");
    let child = &children[0];
    assert_eq!(child.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(child.revision.as_deref(), Some(c1.as_str()));
    assert_eq!(child.task_folder.as_deref(), Some("nightlies"));
    let mut names: Vec<String> = JobStepRepo::get_steps_for_job(&fx.pool, child.job_id)
        .await?
        .into_iter()
        .map(|s| s.step_name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["a", "b"], "the child runs C1's flow, not C2's");

    let later = cr_step(&fx.pool, job_id, "later").await;
    assert_eq!(later.action_revision.as_deref(), Some(c1.as_str()));
    Ok(())
}

#[tokio::test]
async fn pinned_local_task_action_dispatches_pinned_child() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    // `wrapper`'s root step dispatches during creation (dispatch::init).
    let job_id = cr_create_pinned_etl(&fx, "wrapper").await?;
    let children = JobRepo::list_children(&fx.pool, job_id).await?;
    assert_eq!(children.len(), 1, "{children:?}");
    assert_eq!(children[0].task_name, "nightly");
    assert_eq!(children[0].git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        children[0].revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn live_foreign_task_action_in_pinned_job_dispatches_live_child() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "wrapper-foreign").await?;
    let children = JobRepo::list_children(&fx.pool, job_id).await?;
    assert_eq!(children.len(), 1, "{children:?}");
    assert_eq!(children[0].workspace, "billing");
    assert!(children[0].git_ref.is_none());
    assert_eq!(
        children[0].revision.as_deref(),
        Some(fx.commits.billing_main.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn for_each_instances_keep_the_action_pin() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_etl(&fx, "fan").await?;
    for name in ["each[0]", "each[1]"] {
        let s = cr_step(&fx.pool, job_id, name).await;
        assert_eq!(s.action_ref.as_deref(), Some("release/2.3"), "{name}");
        assert_eq!(
            s.action_revision.as_deref(),
            Some(fx.commits.etl_release.as_str()),
            "{name}"
        );
        assert_eq!(s.action_workspace.as_deref(), Some("etl"), "{name}");
    }
    Ok(())
}

#[tokio::test]
async fn hook_job_of_pinned_job_inherits_pin() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "failing").await?;

    JobStepRepo::mark_failed(&fx.pool, job_id, "only", "boom").await?;
    fx.state.settlement().advance(job_id).await?;

    let hooks = cr_jobs_where(
        &fx.pool,
        "SELECT job_id FROM job WHERE source_job_id = $1 AND source_type = 'hook'",
        job_id,
    )
    .await;
    assert_eq!(hooks.len(), 1, "{hooks:?}");
    assert_eq!(hooks[0].git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        hooks[0].revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn hook_with_ref_or_ref_task_action_is_not_fired() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    for task in ["ref-hook", "task-ref-hook"] {
        let job_id = cr_create_pinned_etl(&fx, task).await?;
        JobStepRepo::mark_failed(&fx.pool, job_id, "only", "boom").await?;
        fx.state.settlement().advance(job_id).await?;
        let hooks = cr_jobs_where(
            &fx.pool,
            "SELECT job_id FROM job WHERE source_job_id = $1 AND source_type = 'hook'",
            job_id,
        )
        .await;
        assert!(
            hooks.is_empty(),
            "{task}: a ref'd hook must not fire: {hooks:?}"
        );
        let log = job_log_text(&fx.pool, &fx.state, job_id).await;
        assert!(log.contains("not supported on hooks yet"), "{task}: {log}");
    }
    Ok(())
}

#[tokio::test]
async fn task_retry_of_pinned_job_keeps_pin() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "flaky").await?;

    JobStepRepo::mark_failed(&fx.pool, job_id, "only", "boom").await?;
    fx.state.settlement().advance(job_id).await?;

    let retries = cr_jobs_where(
        &fx.pool,
        "SELECT job_id FROM job WHERE retry_of_job_id = $1",
        job_id,
    )
    .await;
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert_eq!(retries[0].source_type, "retry");
    assert_eq!(retries[0].git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        retries[0].revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

/// `approval-root` exists only at release/2.3: firing the initial `on_suspended`
/// hooks from the caller's (live main) config would find no task at all.
#[tokio::test]
async fn initial_suspended_hooks_use_the_jobs_pinned_config() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "approval-root").await?;
    assert_eq!(cr_step(&fx.pool, job_id, "wait").await.status, "suspended");

    stroem_server::settlement::dispatch::fire_initial_suspended_hooks(&fx.state, job_id).await;

    let hooks = cr_jobs_where(
        &fx.pool,
        "SELECT job_id FROM job WHERE source_job_id = $1 AND source_type = 'hook'",
        job_id,
    )
    .await;
    assert_eq!(hooks.len(), 1, "{hooks:?}");
    assert_eq!(hooks[0].git_ref.as_deref(), Some("release/2.3"));
    Ok(())
}

/// `sub` exists only at release/2.3: the tool child must come from the job's
/// pinned config and carry the pin.
#[tokio::test]
async fn agent_task_tool_child_uses_pinned_config() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "agent-wrap").await?;

    let (status, body) = worker_req(
        &fx.router,
        "POST",
        &format!("/worker/jobs/{job_id}/steps/think/task-tool"),
        Some(json!({"task_name": "sub", "input": {}})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");

    let children = JobRepo::get_child_jobs(&fx.pool, job_id).await?;
    assert_eq!(children.len(), 1, "{children:?}");
    assert_eq!(children[0].git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        children[0].revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    Ok(())
}

/// Spec § 4.6: an agent task tool carrying `ref` is refused at call time —
/// serde would otherwise run the default branch. The YAML is overridden for
/// this test only (the workspace loader does not validate, so it loads).
#[tokio::test]
async fn agent_task_tool_with_ref_is_rejected() -> Result<()> {
    let release = ETL_RELEASE
        .replace(
            "actions:\n  hello:\n",
            "actions:\n  ask-ref:\n    type: agent\n    provider: anthropic\n    model: claude-sonnet-5\n    prompt: hi\n    tools:\n      - task: sub\n        ref: release/2.3\n  hello:\n",
        )
        .replace(
            "tasks:\n  nightly:\n",
            "tasks:\n  agent-ref-wrap:\n    flow:\n      think:\n        action: ask-ref\n  nightly:\n",
        );
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_release: Some(release),
        ..Default::default()
    })
    .await?;
    let job_id = cr_create_pinned_etl(&fx, "agent-ref-wrap").await?;

    let (status, body) = worker_req(
        &fx.router,
        "POST",
        &format!("/worker/jobs/{job_id}/steps/think/task-tool"),
        Some(json!({"task_name": "sub", "input": {}})),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string()
            .contains("not supported on agent task tools yet"),
        "{body}"
    );
    assert!(JobRepo::get_child_jobs(&fx.pool, job_id).await?.is_empty());
    Ok(())
}

/// R7: a step completion that lands on a replica whose pin cannot load (cold
/// store + remote down) leaves the job `running` with nothing live; one
/// recovery tick after the remote returns re-advances it.
#[tokio::test]
async fn stalled_pinned_job_is_readvanced_by_recovery() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let job_id = cr_create_pinned_etl(&fx, "nightly").await?;
    // Step `a` ran on some worker and completed; the job is running. (F11: no
    // `mark_running` — its worker id is an FK; `mark_completed` needs none.)
    JobRepo::mark_running_if_pending_server(&fx.pool, job_id).await?;
    JobStepRepo::mark_completed(&fx.pool, job_id, "a", None).await?;

    // The completion is advanced on a cold replica while the remote is down.
    let replica = fx.second_replica().await?;
    fx.etl.break_remote();
    replica.state.settlement().advance(job_id).await?;
    assert_eq!(
        JobRepo::get(&fx.pool, job_id).await?.unwrap().status,
        "running"
    );
    assert_eq!(cr_step(&fx.pool, job_id, "b").await.status, "pending");
    assert_eq!(
        JobRepo::get_stalled_pinned_jobs(&fx.pool).await?,
        vec![job_id]
    );
    let log = job_log_text(&fx.pool, &replica.state, job_id).await;
    assert!(log.contains("[pin] etl@release/2.3 ("), "{log}");
    assert!(log.contains("not available yet"), "{log}");

    // The remote returns: one recovery tick on that replica re-advances the job.
    fx.etl.restore_remote();
    stroem_server::recovery::sweep_once(&replica.state).await?;
    assert_eq!(cr_step(&fx.pool, job_id, "b").await.status, "ready");
    assert!(JobRepo::get_stalled_pinned_jobs(&fx.pool).await?.is_empty());

    JobStepRepo::mark_completed(&fx.pool, job_id, "b", None).await?;
    replica.state.settlement().advance(job_id).await?;
    assert_eq!(
        JobRepo::get(&fx.pool, job_id).await?.unwrap().status,
        "completed"
    );
    Ok(())
}

/// F4 (spec § 7.3): a PERMANENT pin error at advance settles the job `failed`
/// with one `[pin] … cannot be loaded` line, so the stalled-job phase can
/// never loop on it. A `PinLoadFailed` shows only the fixed sentence: its
/// loader chain can quote secret values (T6 review #9).
#[tokio::test]
async fn permanent_pin_error_at_advance_fails_the_job_once() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let broken = fx
        .etl
        .commit("broken", "main", &[("workflow.yaml", CR_BROKEN)]);
    let gone = "0123456789abcdef0123456789abcdef01234567";

    let mut cases = Vec::new();
    for (commit, line) in [
        (
            gone.to_string(),
            format!(
                "[pin] etl@release/2.3 (0123456) cannot be loaded: commit {gone} not found in \
                 workspace 'etl'"
            ),
        ),
        (
            broken.clone(),
            format!(
                "[pin] etl@release/2.3 ({}) cannot be loaded: its configuration does not load",
                &broken[..7]
            ),
        ),
    ] {
        let job_id = cr_create_pinned_etl(&fx, "nightly").await?;
        JobRepo::mark_running_if_pending_server(&fx.pool, job_id).await?;
        JobStepRepo::mark_completed(&fx.pool, job_id, "a", None).await?;
        // The job's commit is gone (force-push + gc) or no longer loads.
        sqlx::query("UPDATE job SET revision = $1 WHERE job_id = $2")
            .bind(&commit)
            .bind(job_id)
            .execute(&fx.pool)
            .await?;
        fx.state.settlement().advance(job_id).await?;

        assert_eq!(
            JobRepo::get(&fx.pool, job_id).await?.unwrap().status,
            "failed"
        );
        assert_eq!(
            cr_step(&fx.pool, job_id, "b").await.status,
            "cancelled",
            "nothing of a failed job may still start"
        );
        cases.push((job_id, line));
    }

    assert!(JobRepo::get_stalled_pinned_jobs(&fx.pool).await?.is_empty());
    stroem_server::recovery::sweep_once(&fx.state).await?;
    for (job_id, line) in cases {
        let log = job_log_text(&fx.pool, &fx.state, job_id).await;
        assert_eq!(log.matches("[pin]").count(), 1, "{log}");
        assert!(log.contains(&line), "{line}\n---\n{log}");
        assert!(!log.contains("s3cr3t"), "{log}");
    }
    Ok(())
}

/// F4: a pinned CHILD whose pin can never load is settled `failed` and still
/// takes its terminal claim, so the failure reaches the parent step — the
/// parent must not wait on it forever.
#[tokio::test]
async fn permanent_pin_error_in_child_fails_the_parent_step() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let parent = cr_create_pinned_etl(&fx, "wrapper").await?;
    let children = JobRepo::list_children(&fx.pool, parent).await?;
    assert_eq!(children.len(), 1, "{children:?}");
    let child = children[0].job_id;

    JobRepo::mark_running_if_pending_server(&fx.pool, child).await?;
    JobStepRepo::mark_completed(&fx.pool, child, "a", None).await?;
    sqlx::query("UPDATE job SET revision = $1 WHERE job_id = $2")
        .bind("0123456789abcdef0123456789abcdef01234567")
        .bind(child)
        .execute(&fx.pool)
        .await?;
    fx.state.settlement().advance(child).await?;

    assert_eq!(
        JobRepo::get(&fx.pool, child).await?.unwrap().status,
        "failed"
    );
    let claimed: bool =
        sqlx::query_scalar("SELECT metrics_recorded_at IS NOT NULL FROM job WHERE job_id = $1")
            .bind(child)
            .fetch_one(&fx.pool)
            .await?;
    assert!(claimed, "the terminal claim was taken");
    assert_eq!(cr_step(&fx.pool, parent, "call").await.status, "failed");
    assert_eq!(
        JobRepo::get(&fx.pool, parent).await?.unwrap().status,
        "failed"
    );
    Ok(())
}

/// `etl` main plus a task whose `type: task` step is itself `ref`'d: the step
/// carries an ACTION pin (and inherits it as its task pin).
fn cr_etl_main_with_pinned_call() -> String {
    format!(
        "{ETL_MAIN}  pinned-call:\n    flow:\n      first:\n        action: hello\n      call:\n        \
         action: call-local\n        ref: release/2.3\n        depends_on: [first]\n"
    )
}

/// Dispatch loads a step's stamped pins (spec § 7.3). A pin that cannot load
/// fails the step with the `[pin]` text kept (F37) — and a `PinLoadFailed`
/// only ever as the fixed sentence, never the loader chain (T6 review #9).
#[tokio::test]
async fn dispatch_fails_a_task_step_whose_pin_cannot_load() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(cr_etl_main_with_pinned_call()),
        ..Default::default()
    })
    .await?;
    let broken = fx
        .etl
        .commit("broken", "main", &[("workflow.yaml", CR_BROKEN)]);
    let gone = "0123456789abcdef0123456789abcdef01234567";

    // The action owner's pin (`action_revision`) is gone.
    let job_a = cr_create_etl(&fx, "pinned-call").await?;
    sqlx::query(
        "UPDATE job_step SET action_revision = $1 WHERE job_id = $2 AND step_name = 'call'",
    )
    .bind(gone)
    .bind(job_a)
    .execute(&fx.pool)
    .await?;
    // The task pin (`task_revision`) does not load.
    let job_t = cr_create_etl(&fx, "manifest").await?;
    sqlx::query("UPDATE job_step SET task_revision = $1 WHERE job_id = $2 AND step_name = 'call'")
        .bind(&broken)
        .bind(job_t)
        .execute(&fx.pool)
        .await?;

    for (job_id, expected) in [
        (
            job_a,
            format!(
                "[pin] etl@release/2.3 (0123456) cannot be loaded: commit {gone} not found in \
                 workspace 'etl' (owner of action 'call-local')"
            ),
        ),
        (
            job_t,
            format!(
                "[pin] etl@release/2.3 ({}) cannot be loaded: its configuration does not load",
                &broken[..7]
            ),
        ),
    ] {
        JobStepRepo::mark_completed(&fx.pool, job_id, "first", None).await?;
        fx.state.settlement().advance(job_id).await?;
        let call = cr_step(&fx.pool, job_id, "call").await;
        assert_eq!(call.status, "failed");
        let err = call.error_message.unwrap_or_default();
        assert!(err.contains(&expected), "{expected}\n---\n{err}");
        assert!(!err.contains("s3cr3t"), "{err}");
        assert!(JobRepo::list_children(&fx.pool, job_id).await?.is_empty());
    }
    Ok(())
}

/// T9 review: a pin needs both halves. A child asked to carry a `ref` with no
/// commit is refused before any row is written.
#[tokio::test]
async fn child_with_ref_but_no_revision_is_refused() -> Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts::default()).await?;
    let pinned = fx
        .mgr()
        .pins()
        .ensure("etl", &fx.commits.etl_release)
        .await?;
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job")
        .fetch_one(&fx.pool)
        .await?;
    let err = create_child_job_for_task_detailed(
        fx.mgr(),
        &fx.pool,
        &pinned.config,
        "etl",
        "nightly",
        json!({}),
        "agent_tool",
        None,
        Uuid::new_v4(),
        "think",
        None,
        JobDefaults::default(),
        Some("release/2.3"),
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("without its commit"), "{err:#}");
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM job")
        .fetch_one(&fx.pool)
        .await?;
    assert_eq!(before, after);
    Ok(())
}
