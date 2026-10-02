//! Spec 2026-10-02 (git refs) — DB layer: pin columns, partitioned state,
//! stats exclusion, release_claim, the expected_claim guard, the ACL scope.

mod common;

use anyhow::Result;
use common::{create_job, setup_db};
use sqlx::PgPool;
use stroem_db::{JobPinCols, JobRepo, JobStepRepo, NewJobStep, TaskStateRepo, WorkspaceStateRepo};
use uuid::Uuid;

const SHA: &str = "3f2a9c0e1b2c3d4e5f60718293a4b5c6d7e8f901";
const SHA2: &str = "0123456789abcdef0123456789abcdef01234567";

/// Create a job pinned to `git_ref` with `folder`, via the real creation path.
async fn create_pinned_job(
    pool: &PgPool,
    workspace: &str,
    task: &str,
    git_ref: &str,
    folder: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    JobRepo::create_with_parent_tx_id(
        pool,
        id,
        workspace,
        task,
        "distributed",
        None,
        "trigger",
        Some("billing/nightly"),
        None,
        None,
        None,
        Some(SHA),
        None,
        None,
        None,
        None,
        Some(&JobPinCols {
            git_ref: git_ref.to_string(),
            task_folder: folder.map(str::to_string),
        }),
    )
    .await
    .expect("create pinned job");
    id
}

fn script_step(job_id: Uuid, name: &str, status: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "a".to_string(),
        action_type: "script".to_string(),
        status: status.to_string(),
        required_ability: "script".to_string(),
        runner: "local".to_string(),
        ..Default::default()
    }
}

// ─── Task 2: columns ──────────────────────────────────────────────────

#[tokio::test]
async fn job_pin_columns_round_trip() -> Result<()> {
    let pool = setup_db().await;
    let pinned = create_pinned_job(&pool, "billing", "nightly", "release/2.3", Some("etl")).await;
    let job = JobRepo::get(&pool, pinned).await?.unwrap();
    assert_eq!(job.git_ref.as_deref(), Some("release/2.3"));
    assert_eq!(job.task_folder.as_deref(), Some("etl"));
    assert_eq!(job.revision.as_deref(), Some(SHA));

    let plain = create_job(&pool, "billing", "nightly").await;
    let job = JobRepo::get(&pool, plain).await?.unwrap();
    assert_eq!(job.git_ref, None);
    assert_eq!(job.task_folder, None);
    Ok(())
}

#[tokio::test]
async fn skipped_job_carries_its_pin() -> Result<()> {
    let pool = setup_db().await;
    let id = JobRepo::create_skipped(
        &pool,
        "billing",
        "nightly",
        None,
        "trigger",
        Some("main/cron"),
        Some(SHA),
        Some(&JobPinCols {
            git_ref: "v4.1.0".into(),
            task_folder: None,
        }),
    )
    .await?;
    let job = JobRepo::get(&pool, id).await?.unwrap();
    assert_eq!(job.status, "skipped");
    assert_eq!(job.git_ref.as_deref(), Some("v4.1.0"));
    assert_eq!(job.task_folder, None);
    assert_eq!(job.revision.as_deref(), Some(SHA));
    Ok(())
}

#[tokio::test]
async fn step_pin_columns_round_trip() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "main-ws", "t").await;
    let mut step = script_step(job_id, "s", "pending");
    step.action_workspace = Some("billing".into());
    step.action_ref = Some("v4.1.0".into());
    step.action_revision = Some(SHA.into());
    step.task_workspace = Some("billing".into());
    step.task_ref = Some("release/2.3".into());
    step.task_revision = Some(SHA2.into());
    JobStepRepo::create_steps(&pool, &[step, script_step(job_id, "plain", "pending")]).await?;

    let rows = JobStepRepo::get_steps_for_job(&pool, job_id).await?;
    let plain = rows.iter().find(|r| r.step_name == "plain").unwrap();
    assert_eq!(plain.action_ref, None);
    assert_eq!(plain.task_ref, None);
    assert_eq!(plain.pin_releases, 0);
    let s = rows.iter().find(|r| r.step_name == "s").unwrap();
    assert_eq!(s.action_workspace.as_deref(), Some("billing"));
    assert_eq!(s.action_ref.as_deref(), Some("v4.1.0"));
    assert_eq!(s.action_revision.as_deref(), Some(SHA));
    assert_eq!(s.task_workspace.as_deref(), Some("billing"));
    assert_eq!(s.task_ref.as_deref(), Some("release/2.3"));
    assert_eq!(s.task_revision.as_deref(), Some(SHA2));
    assert_eq!(s.pin_releases, 0);
    Ok(())
}

// ─── Task 2: state partitions ─────────────────────────────────────────

#[tokio::test]
async fn task_state_is_partitioned_by_git_ref() -> Result<()> {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    TaskStateRepo::insert(&pool, "ws", "t", job, "k-null", 1, false, None).await?;
    TaskStateRepo::insert_for_ref(
        &pool,
        "ws",
        "t",
        Some("release/2.3"),
        job,
        "k-23",
        1,
        false,
        None,
    )
    .await?;

    let null = TaskStateRepo::get_latest(&pool, "ws", "t").await?.unwrap();
    assert_eq!(
        null.storage_key, "k-null",
        "the old API reads the NULL partition"
    );
    assert_eq!(null.git_ref, None);
    let r23 = TaskStateRepo::get_latest_for_ref(&pool, "ws", "t", Some("release/2.3"))
        .await?
        .unwrap();
    assert_eq!(r23.storage_key, "k-23");
    assert_eq!(r23.git_ref.as_deref(), Some("release/2.3"));
    assert!(
        TaskStateRepo::get_latest_for_ref(&pool, "ws", "t", Some("release/2.4"))
            .await?
            .is_none()
    );

    let all = TaskStateRepo::list(&pool, "ws", "t").await?;
    assert_eq!(all.len(), 2, "list returns every partition");
    Ok(())
}

#[tokio::test]
async fn task_state_prune_is_per_partition() -> Result<()> {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    for k in ["n1", "n2"] {
        TaskStateRepo::insert(&pool, "ws", "t", job, k, 1, false, None).await?;
    }
    for k in ["r1", "r2"] {
        TaskStateRepo::insert_for_ref(&pool, "ws", "t", Some("v1"), job, k, 1, false, None).await?;
    }
    let mut tx = pool.begin().await?;
    let (_, pruned) = TaskStateRepo::insert_and_prune_for_ref(
        &mut tx,
        "ws",
        "t",
        Some("v1"),
        job,
        "r3",
        1,
        false,
        None,
        1,
        None,
    )
    .await?;
    tx.commit().await?;
    let mut pruned = pruned;
    pruned.sort();
    assert_eq!(pruned, vec!["r1".to_string(), "r2".to_string()]);

    let pruned_null = TaskStateRepo::prune_for_ref(&pool, "ws", "t", None, 1).await?;
    assert_eq!(pruned_null.len(), 1);
    assert!(pruned_null[0].starts_with('n'));

    let all = TaskStateRepo::list(&pool, "ws", "t").await?;
    let v1: Vec<_> = all
        .iter()
        .filter(|r| r.git_ref.as_deref() == Some("v1"))
        .collect();
    let null: Vec<_> = all.iter().filter(|r| r.git_ref.is_none()).collect();
    assert_eq!(v1.len(), 1);
    assert_eq!(v1[0].storage_key, "r3");
    assert_eq!(null.len(), 1);
    Ok(())
}

#[tokio::test]
async fn workspace_state_is_partitioned_by_git_ref() -> Result<()> {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    WorkspaceStateRepo::insert(&pool, "ws", "t", job, "g-null", 1, false, None).await?;
    WorkspaceStateRepo::insert_for_ref(&pool, "ws", Some("v1"), "t", job, "g-v1", 1, false, None)
        .await?;
    assert_eq!(
        WorkspaceStateRepo::get_latest(&pool, "ws")
            .await?
            .unwrap()
            .storage_key,
        "g-null"
    );
    let v1 = WorkspaceStateRepo::get_latest_for_ref(&pool, "ws", Some("v1"))
        .await?
        .unwrap();
    assert_eq!(v1.storage_key, "g-v1");
    assert_eq!(v1.git_ref.as_deref(), Some("v1"));
    assert_eq!(
        WorkspaceStateRepo::prune_for_ref(&pool, "ws", Some("v1"), 0).await?,
        vec!["g-v1".to_string()]
    );
    assert_eq!(WorkspaceStateRepo::list(&pool, "ws").await?.len(), 1);
    Ok(())
}

// ─── Task 2: stats exclude pinned jobs (spec § 7.8) ───────────────────

#[tokio::test]
async fn duration_stats_exclude_pinned_jobs() -> Result<()> {
    let pool = setup_db().await;
    let plain = create_job(&pool, "ws", "t").await;
    let pinned = create_pinned_job(&pool, "ws", "t", "release/2.3", None).await;
    for job in [plain, pinned] {
        sqlx::query(
            "UPDATE job SET status = 'completed', started_at = NOW() - INTERVAL '5 seconds', \
             completed_at = NOW() WHERE job_id = $1",
        )
        .bind(job)
        .execute(&pool)
        .await?;
        JobStepRepo::create_steps(&pool, &[script_step(job, "s", "pending")]).await?;
        sqlx::query(
            "UPDATE job_step SET status = 'completed', started_at = NOW() - INTERVAL '4 seconds', \
             completed_at = NOW() WHERE job_id = $1",
        )
        .bind(job)
        .execute(&pool)
        .await?;
    }

    let stats = JobRepo::get_task_duration_stats(&pool, "ws", "t", 50).await?;
    assert_eq!(stats.sample_size, 1);
    let recent = JobRepo::get_recent_durations(&pool, "ws", "t", 50).await?;
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].job_id, plain);
    let steps = JobStepRepo::get_step_duration_stats_for_task(&pool, "ws", "t", 50).await?;
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].sample_size, 1);
    Ok(())
}
