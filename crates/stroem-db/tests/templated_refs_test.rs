//! Templated refs (spec 2026-10-15) — DB layer: the deferred row, the ref
//! memo, the resolution lease and guarded writes, the recovery listing.

use crate::common::{create_job, setup_db};
use anyhow::Result;
use sqlx::PgPool;
use stroem_db::{
    DeferredWrite, JobRefPinRepo, JobRefPinRow, JobRepo, JobStepRepo, NewJobStep, WorkerRepo,
};
use uuid::Uuid;

/// A deferred `ready` step as job creation writes it (spec § 5.1).
pub(crate) fn deferred_step(job_id: Uuid, name: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "hello".to_string(),
        action_type: "deferred".to_string(),
        status: "ready".to_string(),
        required_ability: "deferred".to_string(),
        runner: "none".to_string(),
        action_workspace: Some("etl".to_string()),
        ref_template: Some("{{ input.branch }}".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn deferred_row_round_trips_with_its_template() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!(row.action_type, "deferred");
    assert_eq!(row.required_ability, "deferred");
    assert_eq!(row.runner, "none");
    assert_eq!(row.ref_template.as_deref(), Some("{{ input.branch }}"));
    assert!(row.action_spec.is_none() && row.action_ref.is_none());
    Ok(())
}

const C1: &str = "3f2a9c0e1b2c3d4e5f60718293a4b5c6d7e8f901";
const C2: &str = "0123456789abcdef0123456789abcdef01234567";

fn entry(ws: &str, r: &str, c: &str) -> JobRefPinRow {
    JobRefPinRow {
        workspace: ws.to_string(),
        git_ref: r.to_string(),
        commit: c.to_string(),
    }
}

#[tokio::test]
async fn job_ref_pin_first_writer_wins() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    let mut tx = pool.begin().await?;
    JobRefPinRepo::insert_many_tx(
        &mut *tx,
        job_id,
        &[
            entry("etl", "release/2.3", C1),
            entry("billing", "v4.1.0", C2),
        ],
    )
    .await?;
    tx.commit().await?;
    // A second writer of an existing key gets the FIRST commit back.
    let stored = JobRefPinRepo::insert_first_wins(&pool, job_id, "etl", "release/2.3", C2).await?;
    assert_eq!(stored, C1);
    // A new key is written and returned.
    let stored = JobRefPinRepo::insert_first_wins(&pool, job_id, "etl", "main", C2).await?;
    assert_eq!(stored, C2);
    // Keyed by the string as written: refs/heads/… is another key.
    assert_eq!(
        JobRefPinRepo::get(&pool, job_id, "etl", "refs/heads/release/2.3").await?,
        None
    );
    assert_eq!(
        JobRefPinRepo::get(&pool, job_id, "etl", "release/2.3")
            .await?
            .as_deref(),
        Some(C1)
    );
    let all = JobRefPinRepo::list_for_job(&pool, job_id).await?;
    assert_eq!(
        all,
        vec![
            entry("billing", "v4.1.0", C2),
            entry("etl", "main", C2),
            entry("etl", "release/2.3", C1),
        ]
    );
    // insert_many_tx keeps an existing key too.
    let mut tx = pool.begin().await?;
    JobRefPinRepo::insert_many_tx(&mut *tx, job_id, &[entry("etl", "release/2.3", C2)]).await?;
    tx.commit().await?;
    assert_eq!(
        JobRefPinRepo::get(&pool, job_id, "etl", "release/2.3")
            .await?
            .as_deref(),
        Some(C1)
    );
    // Deleting the job deletes its memo.
    JobRepo::delete(&pool, job_id).await?;
    assert!(JobRefPinRepo::list_for_job(&pool, job_id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn pin_keep_set_keeps_memo_commits_of_live_jobs_only() -> Result<()> {
    let pool = setup_db().await;
    let live = create_job(&pool, "etl", "t").await;
    let done = create_job(&pool, "etl", "t").await;
    sqlx::query("UPDATE job SET status = 'completed', completed_at = NOW() WHERE job_id = $1")
        .bind(done)
        .execute(&pool)
        .await?;
    JobRefPinRepo::insert_first_wins(&pool, live, "etl", "feature/x", C1).await?;
    JobRefPinRepo::insert_first_wins(&pool, done, "etl", "feature/y", C2).await?;
    let keep = JobRepo::pin_keep_set(&pool).await?;
    assert!(
        keep.contains(&("etl".to_string(), C1.to_string())),
        "{keep:?}"
    );
    assert!(
        !keep.contains(&("etl".to_string(), C2.to_string())),
        "{keep:?}"
    );
    Ok(())
}

async fn lease_owner(pool: &PgPool, job_id: Uuid, step: &str) -> Option<Uuid> {
    sqlx::query_scalar(
        "SELECT resolve_lease_owner FROM job_step WHERE job_id = $1 AND step_name = $2",
    )
    .bind(job_id)
    .bind(step)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn expire_lease(pool: &PgPool, job_id: Uuid, step: &str) {
    sqlx::query(
        "UPDATE job_step SET resolve_lease_until = NOW() - interval '1 second' \
         WHERE job_id = $1 AND step_name = $2",
    )
    .bind(job_id)
    .bind(step)
    .execute(pool)
    .await
    .unwrap();
}

/// What the resolution pass writes: a script action at etl@release/2.3.
fn resolved(job_id: Uuid, name: &str) -> NewJobStep {
    NewJobStep {
        job_id,
        step_name: name.to_string(),
        action_name: "hello".to_string(),
        action_type: "script".to_string(),
        action_spec: Some(serde_json::json!({"type": "script", "script": "echo v1"})),
        status: "ready".to_string(),
        required_ability: "script".to_string(),
        runner: "local".to_string(),
        max_retries: Some(2),
        retry_backoff_secs: Some(1),
        retry_strategy: Some("fixed".to_string()),
        action_workspace: Some("etl".to_string()),
        action_revision: Some(C1.to_string()),
        action_ref: Some("release/2.3".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn resolution_lease_is_exclusive_until_it_expires() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", a, 60.0).await?);
    assert!(!JobStepRepo::take_resolve_lease(&pool, job_id, "run", b, 60.0).await?);
    assert!(JobStepRepo::renew_resolve_lease(&pool, job_id, "run", a, 60.0).await?);
    assert!(!JobStepRepo::renew_resolve_lease(&pool, job_id, "run", b, 60.0).await?);
    expire_lease(&pool, job_id, "run").await;
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", b, 60.0).await?);
    assert_eq!(lease_owner(&pool, job_id, "run").await, Some(b));
    // The old holder can no longer renew: its lease is lost.
    assert!(!JobStepRepo::renew_resolve_lease(&pool, job_id, "run", a, 60.0).await?);
    Ok(())
}

#[tokio::test]
async fn resolve_deferred_tx_writes_once_and_only_for_the_lease_holder() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let (holder, other) = (Uuid::new_v4(), Uuid::new_v4());
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", holder, 60.0).await?);
    sqlx::query("UPDATE job_step SET ready_at = NOW() - interval '1 hour' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;

    // Token fence: not the holder → nothing.
    assert_eq!(
        JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", other, &resolved(job_id, "run"))
            .await?,
        DeferredWrite::NotApplied
    );
    let before = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>("SELECT NOW()")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", holder, &resolved(job_id, "run"))
            .await?,
        DeferredWrite::Applied
    );
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!(row.status, "ready");
    assert_eq!(row.action_type, "script");
    assert_eq!(row.required_ability, "script");
    assert_eq!(row.runner, "local");
    assert_eq!(row.action_ref.as_deref(), Some("release/2.3"));
    assert_eq!(row.action_revision.as_deref(), Some(C1));
    assert_eq!(row.max_retries, Some(2));
    assert_eq!(row.ref_template.as_deref(), Some("{{ input.branch }}"));
    assert_eq!(lease_owner(&pool, job_id, "run").await, None);
    let ready_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT ready_at FROM job_step WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await?;
    assert!(ready_at >= before, "ready_at restarts at resolution");
    // A second write (a lost race) changes nothing.
    assert_eq!(
        JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", holder, &resolved(job_id, "run"))
            .await?,
        DeferredWrite::NotApplied
    );
    Ok(())
}

#[tokio::test]
async fn deferred_writes_on_a_cancelled_job_write_nothing() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let token = Uuid::new_v4();
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", token, 60.0).await?);
    assert!(JobRepo::cancel(&pool, job_id).await?);
    assert_eq!(
        JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", token, &resolved(job_id, "run"))
            .await?,
        DeferredWrite::JobTerminal
    );
    assert_eq!(
        JobStepRepo::fail_deferred_tx(&pool, job_id, "run", token, "[ref] step 'run': x").await?,
        DeferredWrite::JobTerminal
    );
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!(
        (row.status.as_str(), row.action_type.as_str()),
        ("ready", "deferred")
    );
    Ok(())
}

#[tokio::test]
async fn fail_deferred_tx_never_overwrites_a_resolved_claimed_or_completed_row() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let token = Uuid::new_v4();
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", token, 60.0).await?);
    // Another resolver resolved it (simulated by the holder's own write).
    JobStepRepo::resolve_deferred_tx(&pool, job_id, "run", token, &resolved(job_id, "run")).await?;
    assert_eq!(
        JobStepRepo::fail_deferred_tx(&pool, job_id, "run", token, "late").await?,
        DeferredWrite::NotApplied
    );
    // Claimed by a worker.
    let worker = Uuid::new_v4();
    WorkerRepo::register(
        &pool,
        worker,
        "w-df",
        &["script".to_string()],
        &[],
        false,
        None,
    )
    .await?;
    let claimed =
        JobStepRepo::claim_ready_step(&pool, &["script".to_string()], &[], false, worker).await?;
    assert_eq!(claimed.map(|s| s.step_name).as_deref(), Some("run"));
    assert_eq!(
        JobStepRepo::fail_deferred_tx(&pool, job_id, "run", token, "late").await?,
        DeferredWrite::NotApplied
    );
    let row = JobStepRepo::get_step(&pool, job_id, "run").await?.unwrap();
    assert_eq!(row.status, "running");
    assert!(row.error_message.is_none());

    // The happy failure path: a deferred row, the holder → failed.
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "other")]).await?;
    let t2 = Uuid::new_v4();
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "other", t2, 60.0).await?);
    assert_eq!(
        JobStepRepo::fail_deferred_tx(
            &pool,
            job_id,
            "other",
            t2,
            "[ref] step 'other': the rendered ref is empty"
        )
        .await?,
        DeferredWrite::Applied
    );
    let row = JobStepRepo::get_step(&pool, job_id, "other")
        .await?
        .unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(
        row.error_message.as_deref(),
        Some("[ref] step 'other': the rendered ref is empty")
    );
    assert!(row.completed_at.is_some());
    Ok(())
}

#[tokio::test]
async fn a_worker_with_every_ability_never_claims_a_deferred_step() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run")]).await?;
    let caps: Vec<String> = ["script", "docker", "kubernetes", "agent"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let worker = Uuid::new_v4();
    WorkerRepo::register(&pool, worker, "w-all", &caps, &[], false, None).await?;
    assert!(
        JobStepRepo::claim_ready_step(&pool, &caps, &[], false, worker)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn unmatched_sweep_and_stranded_listing_treat_deferred_rows_apart() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    let mut script = deferred_step(job_id, "plain");
    script.action_type = "script".to_string();
    script.required_ability = "script".to_string();
    script.runner = "local".to_string();
    script.ref_template = None;
    JobStepRepo::create_steps(&pool, &[deferred_step(job_id, "run"), script]).await?;
    sqlx::query("UPDATE job_step SET ready_at = NOW() - interval '1 hour' WHERE job_id = $1")
        .bind(job_id)
        .execute(&pool)
        .await?;

    // No worker at all: the script step is unmatched, the deferred one is not.
    let unmatched = JobStepRepo::get_unmatched_ready_steps(&pool, 30.0).await?;
    let names: Vec<&str> = unmatched
        .iter()
        .filter(|s| s.job_id == job_id)
        .map(|s| s.step_name.as_str())
        .collect();
    assert_eq!(names, vec!["plain"]);

    // Stranded: an old, unleased deferred row.
    let listed = JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 30.0, None, 100).await?;
    assert!(listed.contains(&job_id));
    // A live lease hides it.
    let token = Uuid::new_v4();
    assert!(JobStepRepo::take_resolve_lease(&pool, job_id, "run", token, 600.0).await?);
    assert!(
        !JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 30.0, None, 100)
            .await?
            .contains(&job_id)
    );
    // An expired one shows it again.
    expire_lease(&pool, job_id, "run").await;
    assert!(
        JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 30.0, None, 100)
            .await?
            .contains(&job_id)
    );
    // Too young: not listed.
    assert!(
        !JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 7200.0, None, 100)
            .await?
            .contains(&job_id)
    );
    // A terminal job: not listed.
    JobRepo::cancel(&pool, job_id).await?;
    assert!(
        !JobRepo::get_jobs_with_stranded_deferred_steps(&pool, 30.0, None, 100)
            .await?
            .contains(&job_id)
    );
    Ok(())
}

#[tokio::test]
async fn stamp_task_pin_only_stamps_a_ready_unstamped_step() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    let mut task_step = deferred_step(job_id, "call");
    task_step.action_type = "task".to_string();
    task_step.required_ability = String::new();
    task_step.ref_template = None;
    JobStepRepo::create_steps(&pool, &[task_step]).await?;
    assert!(JobStepRepo::stamp_task_pin(&pool, job_id, "call", "etl", "release/2.3", C1).await?);
    assert!(!JobStepRepo::stamp_task_pin(&pool, job_id, "call", "etl", "main", C2).await?);
    let row = JobStepRepo::get_step(&pool, job_id, "call").await?.unwrap();
    assert_eq!(
        (
            row.task_workspace.as_deref(),
            row.task_ref.as_deref(),
            row.task_revision.as_deref()
        ),
        (Some("etl"), Some("release/2.3"), Some(C1))
    );
    Ok(())
}

/// A ready `type: task` step, unstamped.
async fn ready_task_step(pool: &PgPool, job_id: Uuid, name: &str) -> Result<()> {
    let mut task_step = deferred_step(job_id, name);
    task_step.action_type = "task".to_string();
    task_step.required_ability = String::new();
    task_step.ref_template = None;
    JobStepRepo::create_steps(pool, &[task_step]).await?;
    Ok(())
}

async fn step_status(pool: &PgPool, job_id: Uuid, name: &str) -> String {
    JobStepRepo::get_step(pool, job_id, name)
        .await
        .unwrap()
        .unwrap()
        .status
}

/// The dispatch claim: one winner, and only for the task stamp the dispatcher
/// resolved with — an unstamped claim loses to a stamp written after it
/// rendered an empty ref, and a stamped claim needs that exact stamp.
#[tokio::test]
async fn claim_task_dispatch_has_one_winner_for_the_stamp_it_resolved_with() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    ready_task_step(&pool, job_id, "plain").await?;
    ready_task_step(&pool, job_id, "pinned").await?;

    // Unstamped: the first claim wins, the second loses.
    assert!(
        !JobStepRepo::claim_task_dispatch(&pool, job_id, "plain", Some(("etl", "main", C1)))
            .await?
    );
    assert!(JobStepRepo::claim_task_dispatch(&pool, job_id, "plain", None).await?);
    assert!(!JobStepRepo::claim_task_dispatch(&pool, job_id, "plain", None).await?);
    let row = JobStepRepo::get_step(&pool, job_id, "plain")
        .await?
        .unwrap();
    assert_eq!(row.status, "running");
    assert!(row.started_at.is_some());

    // Another dispatcher stamped a pin: an unstamped claim and a claim for
    // another stamp lose; the stamp's own claim wins once.
    assert!(JobStepRepo::stamp_task_pin(&pool, job_id, "pinned", "etl", "release/2.3", C1).await?);
    assert!(!JobStepRepo::claim_task_dispatch(&pool, job_id, "pinned", None).await?);
    assert!(
        !JobStepRepo::claim_task_dispatch(
            &pool,
            job_id,
            "pinned",
            Some(("etl", "release/2.3", C2))
        )
        .await?
    );
    assert!(
        !JobStepRepo::claim_task_dispatch(
            &pool,
            job_id,
            "pinned",
            Some(("billing", "release/2.3", C1))
        )
        .await?
    );
    assert_eq!(step_status(&pool, job_id, "pinned").await, "ready");
    assert!(
        JobStepRepo::claim_task_dispatch(&pool, job_id, "pinned", Some(("etl", "release/2.3", C1)))
            .await?
    );
    assert!(
        !JobStepRepo::claim_task_dispatch(
            &pool,
            job_id,
            "pinned",
            Some(("etl", "release/2.3", C1))
        )
        .await?
    );
    // A claimed step can no longer be stamped.
    assert!(!JobStepRepo::stamp_task_pin(&pool, job_id, "plain", "etl", "main", C2).await?);
    Ok(())
}

/// A failure guarded on a status writes only while the step has it: a
/// dispatcher that lost the claim cannot fail the winner's `running` step.
#[tokio::test]
async fn mark_failed_if_writes_only_the_expected_status() -> Result<()> {
    use stroem_common::models::job::StepStatus;
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    ready_task_step(&pool, job_id, "call").await?;
    assert!(JobStepRepo::claim_task_dispatch(&pool, job_id, "call", None).await?);
    assert!(!JobStepRepo::mark_failed_if(&pool, job_id, "call", "stale", StepStatus::Ready).await?);
    let row = JobStepRepo::get_step(&pool, job_id, "call").await?.unwrap();
    assert_eq!(row.status, "running");
    assert_eq!(row.error_message, None);
    assert!(JobStepRepo::mark_failed_if(&pool, job_id, "call", "boom", StepStatus::Running).await?);
    let row = JobStepRepo::get_step(&pool, job_id, "call").await?.unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_message.as_deref(), Some("boom"));
    // A ready step fails from `ready`, and then a claim loses.
    ready_task_step(&pool, job_id, "early").await?;
    assert!(JobStepRepo::mark_failed_if(&pool, job_id, "early", "bad", StepStatus::Ready).await?);
    assert!(!JobStepRepo::claim_task_dispatch(&pool, job_id, "early", None).await?);
    assert_eq!(step_status(&pool, job_id, "early").await, "failed");
    Ok(())
}

/// Spec § 8, contract deviation 1: the claim and the unmatched sweep keep
/// using a partial `status = 'ready'` index after the sweep gains its
/// `<> 'deferred'` conjunct. The WHERE clauses are copies of the two
/// queries (`claim_ready_step`, `get_unmatched_ready_steps`) with literal
/// values — keep them in sync. The sweep copy omits the `NOT EXISTS` worker
/// anti-join, which does not change the `job_step` access path.
/// `enable_seqscan = off` because a test table is
/// small enough that the planner would scan it whatever the indexes are.
#[tokio::test]
async fn claim_and_unmatched_sweep_still_use_a_ready_partial_index() -> Result<()> {
    let pool = setup_db().await;
    let job_id = create_job(&pool, "etl", "t").await;
    sqlx::query(
        "INSERT INTO job_step (job_id, step_name, action_name, action_type, status, \
                               required_ability, runner, ready_at) \
         SELECT $1, 's' || g, 'a', 'script', \
                CASE WHEN g % 100 = 0 THEN 'ready' ELSE 'completed' END, 'script', 'local', NOW() \
         FROM generate_series(1, 5000) g",
    )
    .bind(job_id)
    .execute(&pool)
    .await?;
    sqlx::query("ANALYZE job_step").execute(&pool).await?;

    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await?;
    let claim: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN SELECT job_id, step_name FROM job_step \
         WHERE status = 'ready' \
           AND '[\"script\"]'::jsonb @> to_jsonb(required_ability) \
           AND required_tags::jsonb <@ '[]'::jsonb \
           AND (NOT false OR '[]'::jsonb <@ required_tags::jsonb) \
           AND action_type NOT IN ('task', 'approval', 'event_source') \
           AND (retry_at IS NULL OR retry_at <= NOW()) \
         ORDER BY random() FOR UPDATE SKIP LOCKED LIMIT 1",
    )
    .fetch_all(&mut *tx)
    .await?;
    let sweep: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN SELECT js.job_id, js.step_name FROM job_step js \
         WHERE js.status = 'ready' \
           AND js.action_type NOT IN ('task', 'approval', 'event_source') \
           AND js.action_type <> 'deferred' \
           AND js.ready_at < NOW() - make_interval(secs => 30::double precision)",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (name, plan) in [("claim", claim), ("sweep", sweep)] {
        let text: String = plan.into_iter().map(|r| r.0).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("Seq Scan on job_step"), "{name}: {text}");
        assert!(
            text.contains("idx_job_step_ready_"),
            "{name} must use a partial `status = 'ready'` index: {text}"
        );
    }
    tx.rollback().await?;
    Ok(())
}
