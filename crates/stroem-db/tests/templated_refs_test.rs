//! Templated refs (spec 2026-10-15) — DB layer: the deferred row, the ref
//! memo, the resolution lease and guarded writes, the recovery listing.

use crate::common::{create_job, setup_db};
use anyhow::Result;
use stroem_db::{JobRefPinRepo, JobRefPinRow, JobRepo, JobStepRepo, NewJobStep};
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
