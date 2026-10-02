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
    create_job_for_task_detailed, create_job_for_task_pinned, CreationMode,
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

#[allow(dead_code)] // the Part C settlement tests use it
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
#[allow(dead_code)] // the Part C settlement tests use it
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
