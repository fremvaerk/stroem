//! Spec 2026-10-02 (git refs) — DB layer: pin columns, partitioned state,
//! stats exclusion, release_claim, the expected_claim guard, the ACL scope.

mod common;

use anyhow::Result;
use common::{create_job, setup_db};
use sqlx::PgPool;
use stroem_db::{
    ClaimIdentity, ClosureBounds, FailOutcome, JobAclScope, JobPinCols, JobRepo, JobStepRepo,
    NewJobStep, RedactionClosure, ReleaseOutcome, TaskStateRepo, WorkerRepo, WorkspaceStateRepo,
};
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

// ─── Task 3: release_claim / expected_claim ───────────────────────────

/// Create one ready step and claim it; returns the claim identity.
/// Only ONE step may be ready when this runs (the claim picks at random).
async fn claim_one(pool: &PgPool, job_id: Uuid, name: &str) -> ClaimIdentity {
    JobStepRepo::create_steps(pool, &[script_step(job_id, name, "ready")])
        .await
        .unwrap();
    let worker_id = Uuid::new_v4();
    WorkerRepo::register(
        pool,
        worker_id,
        &format!("w-{name}-{worker_id}"),
        &["script".to_string()],
        &[],
        false,
        None,
    )
    .await
    .unwrap();
    let row = JobStepRepo::claim_ready_step(pool, &["script".to_string()], &[], false, worker_id)
        .await
        .unwrap()
        .expect("a ready step to claim");
    assert_eq!(row.step_name, name);
    ClaimIdentity {
        worker_id,
        started_at: row.started_at.expect("claim stamps started_at"),
    }
}

async fn step(pool: &PgPool, job_id: Uuid, name: &str) -> stroem_db::JobStepRow {
    JobStepRepo::get_step(pool, job_id, name)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn release_claim_puts_the_step_back_to_ready() -> Result<()> {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let claim = claim_one(&pool, job, "s").await;

    // `retry_at` and `ready_at` come from the DB clock: read it, not the host's.
    let db_before: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT NOW()").fetch_one(&pool).await?;
    let out = JobStepRepo::release_claim(&pool, job, "s", claim, chrono::Duration::seconds(10), 30)
        .await?;
    assert_eq!(out, ReleaseOutcome::Released);
    let db_after: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT NOW()").fetch_one(&pool).await?;

    let row = step(&pool, job, "s").await;
    assert_eq!(row.status, "ready");
    assert_eq!(row.worker_id, None);
    assert_eq!(row.started_at, None);
    assert_eq!(row.pin_releases, 1);
    assert_eq!(row.retry_attempt, 0, "a release is not a retry");
    assert_eq!(
        row.retry_history,
        serde_json::json!([]),
        "a release adds no retry history"
    );
    let retry_at = row.retry_at.expect("retry_at set");
    let ten = chrono::Duration::seconds(10);
    assert!(
        retry_at >= db_before + ten && retry_at <= db_after + ten,
        "retry_at {retry_at} not within DB now + 10 s [{db_before}, {db_after}]"
    );
    // The unmatched-step sweep measures from `ready_at`: reset at release, so
    // the time the step spent claimed is not counted.
    let ready_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT ready_at FROM job_step WHERE job_id = $1 AND step_name = 's'")
            .bind(job)
            .fetch_one(&pool)
            .await?;
    assert!(
        ready_at >= claim.started_at && ready_at >= db_before && ready_at <= db_after,
        "ready_at {ready_at}: claim at {}, release in [{db_before}, {db_after}]",
        claim.started_at
    );
    // Not claimable before retry_at.
    let w = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        w,
        &format!("w2-{w}"),
        &["script".to_string()],
        &[],
        false,
        None,
    )
    .await?;
    assert!(
        JobStepRepo::claim_ready_step(&pool, &["script".to_string()], &[], false, w)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn release_claim_on_a_cancelled_job_cancels_the_step_and_its_siblings() -> Result<()> {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let claim = claim_one(&pool, job, "s").await;
    // A sibling becomes ready after the claim, then the job is cancelled
    // between `JobRepo::cancel` and `cancel_pending_steps` (two calls).
    JobStepRepo::create_steps(&pool, &[script_step(job, "sib", "ready")]).await?;
    assert!(JobRepo::cancel(&pool, job).await?);

    let out = JobStepRepo::release_claim(&pool, job, "s", claim, chrono::Duration::seconds(10), 30)
        .await?;
    assert_eq!(out, ReleaseOutcome::Cancelled);
    let s = step(&pool, job, "s").await;
    assert_eq!(s.status, "cancelled");
    assert!(s.completed_at.is_some());
    assert_eq!(step(&pool, job, "sib").await.status, "cancelled");
    assert!(!JobStepRepo::has_live_steps(&pool, job).await?);
    Ok(())
}

#[tokio::test]
async fn release_claim_at_the_cap_writes_nothing() -> Result<()> {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let claim = claim_one(&pool, job, "s").await;
    sqlx::query("UPDATE job_step SET pin_releases = 30 WHERE job_id = $1 AND step_name = 's'")
        .bind(job)
        .execute(&pool)
        .await?;

    let out = JobStepRepo::release_claim(&pool, job, "s", claim, chrono::Duration::seconds(10), 30)
        .await?;
    assert_eq!(out, ReleaseOutcome::CapReached);
    let row = step(&pool, job, "s").await;
    assert_eq!(row.status, "running", "still this claim");
    assert_eq!(row.worker_id, Some(claim.worker_id));
    assert_eq!(row.pin_releases, 30);

    // 29 → one more release is allowed (30 releases in total).
    sqlx::query("UPDATE job_step SET pin_releases = 29 WHERE job_id = $1 AND step_name = 's'")
        .bind(job)
        .execute(&pool)
        .await?;
    let out = JobStepRepo::release_claim(&pool, job, "s", claim, chrono::Duration::seconds(10), 30)
        .await?;
    assert_eq!(out, ReleaseOutcome::Released);
    Ok(())
}

#[tokio::test]
async fn release_claim_with_a_stale_identity_is_not_applied() -> Result<()> {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let claim = claim_one(&pool, job, "s").await;
    let stale = ClaimIdentity {
        started_at: claim.started_at - chrono::Duration::seconds(1),
        ..claim
    };
    let out = JobStepRepo::release_claim(&pool, job, "s", stale, chrono::Duration::seconds(10), 30)
        .await?;
    assert_eq!(out, ReleaseOutcome::NotApplied);
    assert_eq!(step(&pool, job, "s").await.status, "running");

    // A step that already completed is not released either.
    sqlx::query("UPDATE job_step SET status = 'completed' WHERE job_id = $1 AND step_name = 's'")
        .bind(job)
        .execute(&pool)
        .await?;
    let out = JobStepRepo::release_claim(&pool, job, "s", claim, chrono::Duration::seconds(10), 30)
        .await?;
    assert_eq!(out, ReleaseOutcome::NotApplied);
    assert_eq!(step(&pool, job, "s").await.status, "completed");
    Ok(())
}

#[tokio::test]
async fn fail_or_retry_expected_claim_guards_released_and_reclaimed_steps() -> Result<()> {
    let pool = setup_db().await;
    let job = create_job(&pool, "ws", "t").await;
    let first = claim_one(&pool, job, "s").await;

    // Released: the step is `ready` with no worker — a failure decided on the
    // first claim must not apply.
    JobStepRepo::release_claim(&pool, job, "s", first, chrono::Duration::seconds(0), 30).await?;
    let out =
        JobStepRepo::fail_or_retry(&pool, job, "s", "timed out", &[], |_| 0, Some(first)).await?;
    assert_eq!(out, FailOutcome::NotApplied);
    assert_eq!(step(&pool, job, "s").await.status, "ready");

    // Reclaimed by another worker: still not the first claim.
    let w2 = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        w2,
        &format!("w2-{w2}"),
        &["script".to_string()],
        &[],
        false,
        None,
    )
    .await?;
    sqlx::query("UPDATE job_step SET retry_at = NULL WHERE job_id = $1 AND step_name = 's'")
        .bind(job)
        .execute(&pool)
        .await?;
    let row = JobStepRepo::claim_ready_step(&pool, &["script".to_string()], &[], false, w2)
        .await?
        .expect("released step is claimable once retry_at passed");
    let second = ClaimIdentity {
        worker_id: w2,
        started_at: row.started_at.unwrap(),
    };
    let out =
        JobStepRepo::fail_or_retry(&pool, job, "s", "timed out", &[], |_| 0, Some(first)).await?;
    assert_eq!(out, FailOutcome::NotApplied);
    assert_eq!(step(&pool, job, "s").await.status, "running");

    // The matching claim applies.
    let out = JobStepRepo::fail_or_retry(&pool, job, "s", "boom", &[], |_| 0, Some(second)).await?;
    assert!(matches!(out, FailOutcome::Failed { .. }), "{out:?}");
    // And `None` keeps today's behaviour (any status).
    let job2 = create_job(&pool, "ws", "t2").await;
    claim_one(&pool, job2, "s").await;
    let out = JobStepRepo::fail_or_retry(&pool, job2, "s", "boom", &[], |_| 0, None).await?;
    assert!(matches!(out, FailOutcome::Failed { .. }));
    Ok(())
}

/// A finished row keeps its `worker_id` and `started_at`, so the claim
/// identity alone still matches it. Every claim-decided failure pairs it
/// with `[Running]` (`Settlement::claimed_step_failed`): a step that
/// completed or was cancelled after the claim was observed is left alone.
#[tokio::test]
async fn fail_or_retry_running_and_claim_leave_a_finished_row_alone() -> Result<()> {
    use stroem_common::models::job::StepStatus;
    let pool = setup_db().await;

    let job = create_job(&pool, "ws", "t").await;
    let claim = claim_one(&pool, job, "s").await;
    JobStepRepo::mark_completed(&pool, job, "s", Some(serde_json::json!({"ok": true}))).await?;
    let done = step(&pool, job, "s").await;
    assert_eq!(
        (done.worker_id, done.started_at),
        (Some(claim.worker_id), Some(claim.started_at)),
        "the identity survives completion"
    );
    let out = JobStepRepo::fail_or_retry(
        &pool,
        job,
        "s",
        "Worker heartbeat timeout",
        &[StepStatus::Running],
        |_| 0,
        Some(claim),
    )
    .await?;
    assert_eq!(out, FailOutcome::NotApplied);
    let after = step(&pool, job, "s").await;
    assert_eq!(after.status, "completed");
    assert_eq!(after.error_message, None);
    assert_eq!(after.output, Some(serde_json::json!({"ok": true})));

    let job2 = create_job(&pool, "ws", "t2").await;
    let claim2 = claim_one(&pool, job2, "s").await;
    sqlx::query("UPDATE job_step SET status = 'cancelled' WHERE job_id = $1")
        .bind(job2)
        .execute(&pool)
        .await?;
    let out = JobStepRepo::fail_or_retry(
        &pool,
        job2,
        "s",
        "Step timed out",
        &[StepStatus::Running],
        |_| 0,
        Some(claim2),
    )
    .await?;
    assert_eq!(out, FailOutcome::NotApplied);
    assert_eq!(step(&pool, job2, "s").await.status, "cancelled");
    Ok(())
}

// ─── Task 4: ACL scope ────────────────────────────────────────────────

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

fn triples(v: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
    v.iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect()
}

#[tokio::test]
async fn acl_scope_authorises_pinned_jobs_by_their_own_folder() -> Result<()> {
    let pool = setup_db().await;
    // Created oldest → newest; the denied one is the newest.
    let plain = create_job(&pool, "a", "t").await;
    let allowed = create_pinned_job(&pool, "a", "t", "release/2.3", Some("etl")).await;
    let no_folder = create_pinned_job(&pool, "a", "t", "release/2.5", None).await;
    let denied = create_pinned_job(&pool, "a", "t", "release/2.4", Some("secret")).await;
    // Distinct created_at so "newest first" is deterministic.
    for (age_min, id) in [(4, plain), (3, allowed), (2, no_folder), (1, denied)] {
        sqlx::query(
            "UPDATE job SET created_at = NOW() - make_interval(mins => $2) WHERE job_id = $1",
        )
        .bind(id)
        .bind(age_min)
        .execute(&pool)
        .await?;
    }

    let scope = JobAclScope {
        live_pairs: pairs(&[("a", "t")]),
        pinned_triples: triples(&[("a", "t", "etl"), ("a", "t", "")]),
    };
    let ids: Vec<Uuid> = JobRepo::list_with_acl(&pool, &scope, None, None, None, 10, 0)
        .await?
        .into_iter()
        .map(|j| j.job_id)
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    for id in [plain, allowed, no_folder] {
        assert!(ids.contains(&id));
    }
    assert!(!ids.contains(&denied));
    assert_eq!(
        JobRepo::count_with_acl(&pool, &scope, None, None, None).await?,
        3
    );
    let counts = JobRepo::get_status_counts_with_acl(&pool, &scope).await?;
    assert_eq!(counts.get("pending"), Some(&3));

    // The predicate applies before LIMIT: the newest job is denied, so the
    // first page is the newest PERMITTED job.
    let first = JobRepo::list_with_acl(&pool, &scope, None, None, None, 1, 0).await?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].job_id, no_folder);

    // Live pairs never authorise a pinned job, even for the same task name.
    let live_only = JobAclScope {
        live_pairs: pairs(&[("a", "t")]),
        pinned_triples: vec![],
    };
    let ids: Vec<Uuid> = JobRepo::list_with_acl(&pool, &live_only, None, None, None, 10, 0)
        .await?
        .into_iter()
        .map(|j| j.job_id)
        .collect();
    assert_eq!(ids, vec![plain]);

    // Empty scope → nothing.
    let empty = JobAclScope::default();
    assert!(empty.is_empty());
    assert!(
        JobRepo::list_with_acl(&pool, &empty, None, None, None, 10, 0)
            .await?
            .is_empty()
    );
    assert_eq!(
        JobRepo::count_with_acl(&pool, &empty, None, None, None).await?,
        0
    );
    assert!(JobRepo::get_status_counts_with_acl(&pool, &empty)
        .await?
        .is_empty());

    // Filters still compose with the scope.
    assert_eq!(
        JobRepo::count_with_acl(&pool, &scope, Some("completed"), None, None).await?,
        0
    );
    assert_eq!(
        JobRepo::count_with_acl(&pool, &scope, None, Some("trigger"), None).await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn pinned_task_triples_lists_distinct_pinned_tasks() -> Result<()> {
    let pool = setup_db().await;
    create_job(&pool, "a", "t").await;
    create_pinned_job(&pool, "a", "t", "release/2.3", Some("etl")).await;
    create_pinned_job(&pool, "a", "t", "release/2.3", Some("etl")).await;
    create_pinned_job(&pool, "a", "t", "release/2.4", Some("secret")).await;
    create_pinned_job(&pool, "b", "u", "v1", None).await;

    let mut got = JobRepo::pinned_task_triples(&pool).await?;
    got.sort();
    assert_eq!(
        got,
        vec![
            ("a".to_string(), "t".to_string(), Some("etl".to_string())),
            ("a".to_string(), "t".to_string(), Some("secret".to_string())),
            ("b".to_string(), "u".to_string(), None),
        ]
    );
    Ok(())
}

// ─── Task 22 fix: the redaction closure (spec § 7.4) ──────────────────

const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";
const C3: &str = "3333333333333333333333333333333333333333";
const C4: &str = "4444444444444444444444444444444444444444";
const C5: &str = "5555555555555555555555555555555555555555";
const C6: &str = "6666666666666666666666666666666666666666";
const C7: &str = "7777777777777777777777777777777777777777";
const C8: &str = "8888888888888888888888888888888888888888";
const C9: &str = "9999999999999999999999999999999999999999";
const CF: &str = "ffffffffffffffffffffffffffffffffffffffff";

/// The server's bounds (`redaction::CLOSURE_BOUNDS`).
const BOUNDS: ClosureBounds = ClosureBounds {
    task_depth: 10,
    hook_hops: 3,
    restart_hops: 32,
    retry_hops: 33,
    max_jobs: 20_000,
};

/// Lineage of one seeded job.
#[derive(Default)]
struct Lineage<'a> {
    source_type: &'a str,
    source_job_id: Option<Uuid>,
    source_id: Option<String>,
    parent_job_id: Option<Uuid>,
    retry_of_job_id: Option<Uuid>,
    /// The job's own pin: (workspace, ref, commit).
    pin: Option<(&'a str, &'a str, &'a str)>,
}

async fn seed_closure_job(pool: &PgPool, l: Lineage<'_>) -> Uuid {
    let job_id = Uuid::new_v4();
    let (workspace, git_ref, revision) = match l.pin {
        Some((ws, r, c)) => (ws, Some(r), Some(c)),
        None => ("etl", None, None),
    };
    let source_type = if l.source_type.is_empty() {
        "api"
    } else {
        l.source_type
    };
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, status, source_type, source_job_id, \
                          source_id, parent_job_id, retry_of_job_id, git_ref, revision) \
         VALUES ($1, $2, 't', 'completed', $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(job_id)
    .bind(workspace)
    .bind(source_type)
    .bind(l.source_job_id)
    .bind(l.source_id)
    .bind(l.parent_job_id)
    .bind(l.retry_of_job_id)
    .bind(git_ref)
    .bind(revision)
    .execute(pool)
    .await
    .expect("seed closure job");
    job_id
}

/// A step with an action pin `(action_workspace, ref, commit)` and/or a task
/// pin `(task_workspace, ref, commit)`.
async fn seed_closure_step(
    pool: &PgPool,
    job_id: Uuid,
    name: &str,
    action: Option<(Option<&str>, &str, &str)>,
    task: Option<(&str, &str, &str)>,
) {
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, \
                               action_workspace, action_ref, action_revision, \
                               task_workspace, task_ref, task_revision) \
         VALUES ($1, $2, $2, 'script', 'completed', $3, $4, $5, $6, $7, $8)",
    )
    .bind(job_id)
    .bind(name)
    .bind(action.and_then(|a| a.0))
    .bind(action.map(|a| a.1))
    .bind(action.map(|a| a.2))
    .bind(task.map(|t| t.0))
    .bind(task.map(|t| t.1))
    .bind(task.map(|t| t.2))
    .execute(pool)
    .await
    .expect("seed closure step");
}

/// The closure's pins as triples; `None` = [`RedactionClosure::Truncated`].
async fn closure(
    pool: &PgPool,
    job: Uuid,
    bounds: ClosureBounds,
) -> Option<Vec<(String, String, String)>> {
    match JobRepo::redaction_closure_pins(pool, job, bounds)
        .await
        .expect("closure pins")
    {
        RedactionClosure::Pins(rows) => Some(
            rows.into_iter()
                .map(|r| (r.workspace, r.git_ref, r.revision))
                .collect(),
        ),
        RedactionClosure::Truncated => None,
    }
}

/// The closure is the whole tree (root + all descendants) of every job in the
/// source lineage: the job, its parents up to the root, and the hook,
/// restart and task-retry sources it was made from. Nothing else: no hook or
/// restart OF a job, no re-run lineage, no unrelated job. Every bound fails
/// closed: an edge it refuses, or too many jobs, answers `Truncated`, and the
/// exact bound itself still passes.
#[tokio::test]
async fn redaction_closure_pins_cover_the_whole_tree_of_the_source_lineage_and_fail_closed_at_a_bound(
) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(240), async {
        let pool = setup_db().await;
        // S0 (unpinned) runs a step at etl@release/2.3. Its children: K,
        // pinned at billing@v4.1.0, and K2 with a task pin at etl@release/2.6.
        // K's child G runs a docs@main action.
        let s0 = seed_closure_job(&pool, Lineage::default()).await;
        seed_closure_step(&pool, s0, "s", Some((None, "release/2.3", C1)), None).await;
        let k = seed_closure_job(
            &pool,
            Lineage {
                source_type: "task",
                parent_job_id: Some(s0),
                pin: Some(("billing", "v4.1.0", C2)),
                ..Default::default()
            },
        )
        .await;
        let k2 = seed_closure_job(
            &pool,
            Lineage {
                source_type: "task",
                parent_job_id: Some(s0),
                ..Default::default()
            },
        )
        .await;
        seed_closure_step(&pool, k2, "t", None, Some(("etl", "release/2.6", C6))).await;
        let g = seed_closure_job(
            &pool,
            Lineage {
                source_type: "task",
                parent_job_id: Some(k),
                ..Default::default()
            },
        )
        .await;
        seed_closure_step(&pool, g, "g", Some((Some("docs"), "main", C3)), None).await;

        // H1: an unpinned hook of S0, whose `type: task` child HK runs at
        // etl@release/2.4; H2 is a hook of H1; HX a sibling hook of S0.
        let h1 = seed_closure_job(
            &pool,
            Lineage {
                source_type: "hook",
                source_job_id: Some(s0),
                source_id: Some(s0.to_string()),
                ..Default::default()
            },
        )
        .await;
        let hk = seed_closure_job(
            &pool,
            Lineage {
                source_type: "task",
                parent_job_id: Some(h1),
                ..Default::default()
            },
        )
        .await;
        seed_closure_step(&pool, hk, "h", Some((None, "release/2.4", C4)), None).await;
        let h2 = seed_closure_job(
            &pool,
            Lineage {
                source_type: "hook",
                source_job_id: Some(h1),
                source_id: Some(h1.to_string()),
                ..Default::default()
            },
        )
        .await;
        let hx = seed_closure_job(
            &pool,
            Lineage {
                source_type: "hook",
                source_job_id: Some(s0),
                pin: Some(("etl", "release/7", C7)),
                ..Default::default()
            },
        )
        .await;
        // A hook row written before migration 048: no source_job_id.
        let hpre = seed_closure_job(
            &pool,
            Lineage {
                source_type: "hook",
                source_id: Some(format!("{s0}/notify")),
                ..Default::default()
            },
        )
        .await;
        // R restarts S0 at etl@release/2.5; R2 restarts R (no pin of its own).
        let r = seed_closure_job(
            &pool,
            Lineage {
                source_type: "restart",
                source_job_id: Some(s0),
                pin: Some(("etl", "release/2.5", C5)),
                ..Default::default()
            },
        )
        .await;
        let r2 = seed_closure_job(
            &pool,
            Lineage {
                source_type: "restart",
                source_job_id: Some(r),
                ..Default::default()
            },
        )
        .await;
        // A re-run of S0 (input replay, not a copy) and an unrelated job.
        let rerun = seed_closure_job(
            &pool,
            Lineage {
                source_type: "rerun",
                source_job_id: Some(s0),
                pin: Some(("etl", "release/8", C8)),
                ..Default::default()
            },
        )
        .await;
        let unrelated = seed_closure_job(&pool, Lineage::default()).await;
        seed_closure_step(&pool, unrelated, "u", None, Some(("etl", "release/9", C9))).await;
        // RT: a task retry of the hook job H1 (it replays H1's input).
        let rt = seed_closure_job(
            &pool,
            Lineage {
                source_type: "retry",
                retry_of_job_id: Some(h1),
                ..Default::default()
            },
        )
        .await;
        // F: a two-level tree (F → FC), deep enough for `task_depth: 1`.
        let f = seed_closure_job(&pool, Lineage::default()).await;
        seed_closure_step(&pool, f, "f", Some((None, "release/f", CF)), None).await;
        let fc = seed_closure_job(
            &pool,
            Lineage {
                source_type: "task",
                parent_job_id: Some(f),
                ..Default::default()
            },
        )
        .await;

        let s0_tree = triples(&[
            ("billing", "v4.1.0", C2),
            ("docs", "main", C3),
            ("etl", "release/2.3", C1),
            ("etl", "release/2.6", C6),
        ]);
        let with = |extra: &[(&str, &str, &str)]| {
            let mut v = s0_tree.clone();
            v.extend(triples(extra));
            v.sort_by(|a, b| (&a.0, &a.2, &a.1).cmp(&(&b.0, &b.2, &b.1)));
            v
        };

        // Any job of S0's tree: the whole tree — root, ancestors, siblings.
        for job in [s0, k, k2, g] {
            assert_eq!(
                closure(&pool, job, BOUNDS).await,
                Some(s0_tree.clone()),
                "{job}"
            );
        }
        // A hook job and its whole tree: their tree plus the source's tree;
        // never a sibling hook of the same source (C7).
        let hook_tree = with(&[("etl", "release/2.4", C4)]);
        for job in [h1, hk, h2] {
            assert_eq!(
                closure(&pool, job, BOUNDS).await,
                Some(hook_tree.clone()),
                "{job}"
            );
        }
        // A task retry of H1 reaches H1, and through it S0.
        assert_eq!(closure(&pool, rt, BOUNDS).await, Some(hook_tree.clone()));
        // A sibling hook: its own pin and the source's tree, not H1's child.
        assert_eq!(
            closure(&pool, hx, BOUNDS).await,
            Some(with(&[("etl", "release/7", C7)]))
        );
        // A pre-048 hook row reaches its source through `source_id`.
        assert_eq!(closure(&pool, hpre, BOUNDS).await, Some(s0_tree.clone()));
        // A restart (and a restart of a restart) covers its source's tree.
        let restart_tree = with(&[("etl", "release/2.5", C5)]);
        assert_eq!(closure(&pool, r, BOUNDS).await, Some(restart_tree.clone()));
        assert_eq!(closure(&pool, r2, BOUNDS).await, Some(restart_tree.clone()));
        // A re-run is not a copy: only its own pin.
        assert_eq!(
            closure(&pool, rerun, BOUNDS).await,
            Some(triples(&[("etl", "release/8", C8)]))
        );

        // Bounds fail closed. A hop cap: the exact cap passes (H1 → S0 is
        // one hook hop, S0 has no source), one more is refused (H2 → H1 →
        // S0).
        let one_hook = ClosureBounds {
            hook_hops: 1,
            ..BOUNDS
        };
        assert_eq!(closure(&pool, h1, one_hook).await, Some(hook_tree.clone()));
        assert_eq!(closure(&pool, h2, one_hook).await, None);
        let one_restart = ClosureBounds {
            restart_hops: 1,
            ..BOUNDS
        };
        assert_eq!(
            closure(&pool, r, one_restart).await,
            Some(restart_tree.clone())
        );
        assert_eq!(closure(&pool, r2, one_restart).await, None);
        let no_retry = ClosureBounds {
            retry_hops: 0,
            ..BOUNDS
        };
        assert_eq!(closure(&pool, rt, no_retry).await, None);
        assert_eq!(closure(&pool, h1, no_retry).await, Some(hook_tree.clone()));

        // The depth bound: F's two levels fit one level exactly, from either
        // end. S0's three levels do not, from any of its jobs: a parent or a
        // child at the limit is a refused edge.
        let one_level = ClosureBounds {
            task_depth: 1,
            ..BOUNDS
        };
        let f_tree = Some(triples(&[("etl", "release/f", CF)]));
        assert_eq!(closure(&pool, f, one_level).await, f_tree);
        assert_eq!(closure(&pool, fc, one_level).await, f_tree);
        for job in [s0, k, k2, g] {
            assert_eq!(closure(&pool, job, one_level).await, None, "{job}");
        }

        // The node cap: S0's tree holds 4 jobs.
        let cap = |max_jobs| ClosureBounds { max_jobs, ..BOUNDS };
        assert_eq!(closure(&pool, s0, cap(4)).await, Some(s0_tree.clone()));
        assert_eq!(closure(&pool, s0, cap(3)).await, None);
        Ok::<(), anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("test timed out"))?
}

/// The redaction short-circuit's probe: true as soon as ANY job is pinned or
/// any step carries an action or task pin, false otherwise.
#[tokio::test]
async fn any_pinned_rows_sees_a_pinned_job_or_a_pinned_step() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(240), async {
        let pool = setup_db().await;
        let any = || JobRepo::any_pinned_rows(&pool);
        assert!(!any().await?, "empty database");
        let plain = seed_closure_job(&pool, Lineage::default()).await;
        seed_closure_step(&pool, plain, "s", None, None).await;
        assert!(!any().await?, "no pin anywhere");

        let remove = |job: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query("DELETE FROM job_step WHERE job_id = $1")
                    .bind(job)
                    .execute(&pool)
                    .await?;
                sqlx::query("DELETE FROM job WHERE job_id = $1")
                    .bind(job)
                    .execute(&pool)
                    .await?;
                anyhow::Ok(())
            }
        };

        let action = seed_closure_job(&pool, Lineage::default()).await;
        seed_closure_step(&pool, action, "a", Some((None, "release/2.3", C1)), None).await;
        assert!(any().await?, "a step's action pin");
        remove(action).await?;
        assert!(!any().await?);

        let task = seed_closure_job(&pool, Lineage::default()).await;
        seed_closure_step(&pool, task, "t", None, Some(("etl", "release/2.3", C1))).await;
        assert!(any().await?, "a step's task pin");
        remove(task).await?;
        assert!(!any().await?);

        seed_closure_job(
            &pool,
            Lineage {
                pin: Some(("etl", "release/2.3", C1)),
                ..Default::default()
            },
        )
        .await;
        assert!(any().await?, "a pinned job");
        Ok::<(), anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("test timed out"))?
}
