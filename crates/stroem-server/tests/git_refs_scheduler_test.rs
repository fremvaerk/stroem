//! Scheduler triggers with a cross-workspace `task` and/or `ref:` (spec § 7.5).
//! These also cover `trigger_target::create_target_job`: the pinned branch
//! (`*_with_ref_*`) and the unpinned branch (`*_without_ref_*`).

use crate::common::pinned::*;

use stroem_db::{JobRepo, JobRow};
use uuid::Uuid;

/// `billing` main: `nightly` with no folder.
const SCHED_BILLING_MAIN: &str = r#"
actions:
  say:
    type: script
    script: echo billing-main
tasks:
  nightly:
    flow:
      run:
        action: say
"#;

/// The older `billing` commit that tag `v4.1.0` points at: `nightly` in folder `etl`.
const SCHED_BILLING_TAGGED: &str = r#"
actions:
  say:
    type: script
    script: echo billing-v4
tasks:
  nightly:
    folder: etl
    flow:
      run:
        action: say
"#;

/// `etl` at `release/2.3`: `local-task` in folder `ops`.
const SCHED_ETL_RELEASE: &str = r#"
actions:
  say:
    type: script
    script: echo etl-release
tasks:
  local-task:
    folder: ops
    flow:
      run:
        action: say
"#;

/// `etl` main: the triggers under test.
const SCHED_ETL_MAIN: &str = r#"
actions:
  say:
    type: script
    script: echo etl-main
tasks:
  local-task:
    flow:
      run:
        action: say
triggers:
  cross-live:
    type: scheduler
    cron: "0 2 * * *"
    task: billing.nightly
  cross-ref:
    type: scheduler
    cron: "0 2 * * *"
    task: billing.nightly
    ref: v4.1.0
    concurrency: skip
  own-ref:
    type: scheduler
    cron: "0 2 * * *"
    task: local-task
    ref: release/2.3
  missing-ref-cancel:
    type: scheduler
    cron: "0 2 * * *"
    task: billing.nightly
    ref: release/404
    concurrency: cancel_previous
  missing-ref-skip:
    type: scheduler
    cron: "0 2 * * *"
    task: billing.nightly
    ref: release/404
    concurrency: skip
"#;

async fn sched_fixture() -> anyhow::Result<PinnedFixture> {
    pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(SCHED_ETL_MAIN.to_string()),
        etl_release: Some(SCHED_ETL_RELEASE.to_string()),
        billing_main: Some(SCHED_BILLING_MAIN.to_string()),
        billing_tagged: Some(SCHED_BILLING_TAGGED.to_string()),
        ..Default::default()
    })
    .await
}

async fn sched_fire(fx: &PinnedFixture, trigger: &str) {
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        stroem_server::scheduler::fire_trigger_once(
            &fx.state,
            fx.mgr(),
            fx.mgr(),
            &format!("etl/{trigger}"),
        ),
    )
    .await
    .expect("fire_trigger_once timed out");
}

async fn sched_jobs(fx: &PinnedFixture, trigger: &str) -> Vec<JobRow> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT job_id FROM job WHERE source_type = 'trigger' AND source_id = $1 ORDER BY created_at",
    )
    .bind(format!("etl/{trigger}"))
    .fetch_all(&fx.pool)
    .await
    .unwrap();
    let mut rows = Vec::new();
    for id in ids {
        rows.push(JobRepo::get(&fx.pool, id).await.unwrap().unwrap());
    }
    rows
}

/// An active (pending) job attributed to `trigger`, as a previous fire leaves.
async fn sched_active_job(fx: &PinnedFixture, trigger: &str) -> Uuid {
    JobRepo::create(
        &fx.pool,
        "billing",
        "nightly",
        "distributed",
        None,
        "trigger",
        Some(&format!("etl/{trigger}")),
        None,
        None,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn cross_workspace_trigger_without_ref_runs_owner_live() -> anyhow::Result<()> {
    let fx = sched_fixture().await?;
    sched_fire(&fx, "cross-live").await;
    let jobs = sched_jobs(&fx, "cross-live").await;
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.workspace, "billing");
    assert_eq!(job.task_name, "nightly");
    assert_eq!(job.git_ref, None);
    assert_eq!(job.task_folder, None);
    assert_eq!(
        job.revision.as_deref(),
        Some(fx.commits.billing_main.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn cross_workspace_trigger_with_ref_creates_a_pinned_job() -> anyhow::Result<()> {
    let fx = sched_fixture().await?;
    sched_fire(&fx, "cross-ref").await;
    let jobs = sched_jobs(&fx, "cross-ref").await;
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.workspace, "billing");
    assert_eq!(job.task_name, "nightly");
    assert_eq!(job.git_ref.as_deref(), Some("v4.1.0"));
    assert_eq!(
        job.revision.as_deref(),
        Some(fx.commits.billing_tag.as_str())
    );
    assert_eq!(job.task_folder.as_deref(), Some("etl"));
    Ok(())
}

#[tokio::test]
async fn own_workspace_trigger_with_ref_creates_a_pinned_job() -> anyhow::Result<()> {
    let fx = sched_fixture().await?;
    sched_fire(&fx, "own-ref").await;
    let jobs = sched_jobs(&fx, "own-ref").await;
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.workspace, "etl");
    assert_eq!(job.task_name, "local-task");
    assert_eq!(job.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(
        job.revision.as_deref(),
        Some(fx.commits.etl_release.as_str())
    );
    assert_eq!(job.task_folder.as_deref(), Some("ops"));
    Ok(())
}

#[tokio::test]
async fn trigger_with_missing_ref_is_missed_without_side_effects() -> anyhow::Result<()> {
    let fx = sched_fixture().await?;
    for trigger in ["missing-ref-cancel", "missing-ref-skip"] {
        let running = sched_active_job(&fx, trigger).await;
        sched_fire(&fx, trigger).await;
        let job = JobRepo::get(&fx.pool, running).await?.unwrap();
        assert_ne!(
            job.status, "cancelled",
            "{trigger}: previous run was cancelled"
        );
        assert_eq!(
            sched_jobs(&fx, trigger).await.len(),
            1,
            "{trigger}: a fire that cannot resolve its ref must leave no row (no job, no skip row)"
        );
    }
    Ok(())
}

#[tokio::test]
async fn skipped_fire_is_recorded_for_the_resolved_target() -> anyhow::Result<()> {
    let fx = sched_fixture().await?;
    sched_active_job(&fx, "cross-ref").await;
    sched_fire(&fx, "cross-ref").await;
    let jobs = sched_jobs(&fx, "cross-ref").await;
    let skipped: Vec<_> = jobs.iter().filter(|j| j.status == "skipped").collect();
    assert_eq!(skipped.len(), 1, "{jobs:?}");
    let row = skipped[0];
    assert_eq!(row.workspace, "billing");
    assert_eq!(row.task_name, "nightly");
    assert_eq!(row.git_ref.as_deref(), Some("v4.1.0"));
    assert_eq!(
        row.revision.as_deref(),
        Some(fx.commits.billing_tag.as_str())
    );
    assert_eq!(row.task_folder.as_deref(), Some("etl"));
    Ok(())
}

/// `etl` main with a trigger whose task (an approval root) exists only at
/// the default fixture's `release/2.3`.
const SCHED_APPROVAL_MAIN: &str = r#"
actions:
  hello:
    type: script
    script: echo main
tasks:
  placeholder:
    flow:
      run:
        action: hello
triggers:
  approval-ref:
    type: scheduler
    cron: "0 2 * * *"
    task: approval-root
    ref: release/2.3
"#;

/// F45: the target's root step is an approval that exists only at the ref;
/// its initial `on_suspended` hook comes from the pinned config.
#[tokio::test]
async fn ref_target_with_approval_root_fires_on_suspended_from_the_pinned_config(
) -> anyhow::Result<()> {
    let fx = pinned_workspace_fixture(PinnedFixtureOpts {
        etl_main: Some(SCHED_APPROVAL_MAIN.to_string()),
        ..Default::default()
    })
    .await?;
    sched_fire(&fx, "approval-ref").await;
    let jobs = sched_jobs(&fx, "approval-ref").await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].git_ref.as_deref(), Some("release/2.3"));
    let step_status: String =
        sqlx::query_scalar("SELECT status FROM job_step WHERE job_id = $1 AND step_name = 'wait'")
            .bind(jobs[0].job_id)
            .fetch_one(&fx.pool)
            .await?;
    assert_eq!(step_status, "suspended");
    let hooks: Vec<Uuid> = sqlx::query_scalar(
        "SELECT job_id FROM job WHERE source_job_id = $1 AND source_type = 'hook'",
    )
    .bind(jobs[0].job_id)
    .fetch_all(&fx.pool)
    .await?;
    assert_eq!(hooks.len(), 1, "{hooks:?}");
    let hook = JobRepo::get(&fx.pool, hooks[0]).await?.unwrap();
    assert_eq!(hook.git_ref.as_deref(), Some("release/2.3"));
    Ok(())
}
