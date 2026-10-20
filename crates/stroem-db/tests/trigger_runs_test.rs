//! Trigger last-run lookup (spec 2026-10-20 § 7.2): result shape and the
//! index-probe plan.

use crate::common::setup_db;
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use stroem_db::{TriggerRunKey, TriggerRunRepo};
use uuid::Uuid;

fn key(source_type: &str, source_id: &str) -> TriggerRunKey {
    TriggerRunKey {
        source_type: source_type.to_string(),
        source_id: source_id.to_string(),
    }
}

async fn insert_job(
    pool: &PgPool,
    source_type: &str,
    source_id: &str,
    task: &str,
    status: &str,
    created_at: DateTime<Utc>,
    input: Option<Value>,
) -> Uuid {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type, source_id, input, created_at) \
         VALUES ($1, 'etl', $2, 'distributed', $3, $4, $5, $6, $7)",
    )
    .bind(job_id)
    .bind(task)
    .bind(status)
    .bind(source_type)
    .bind(source_id)
    .bind(input)
    .bind(created_at)
    .execute(pool)
    .await
    .unwrap();
    job_id
}

#[tokio::test]
async fn last_runs_returns_one_row_per_key_in_order_with_nulls_for_never() -> Result<()> {
    let pool = setup_db().await;
    let now = Utc::now();
    insert_job(&pool, "trigger", "etl/a", "t", "completed", now, None).await;
    insert_job(&pool, "webhook", "etl/b", "t", "failed", now, None).await;
    let keys = [
        key("trigger", "etl/a"),
        key("webhook", "etl/b"),
        key("trigger", "etl/none"),
    ];
    let (_, rows) = TriggerRunRepo::last_runs(&pool, &keys).await?;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].source_id, "etl/a");
    assert_eq!(rows[0].run_status.as_deref(), Some("completed"));
    assert_eq!(rows[1].source_id, "etl/b");
    assert_eq!(rows[1].run_status.as_deref(), Some("failed"));
    let r = &rows[2];
    assert_eq!(r.source_id, "etl/none");
    assert!(r.run_job_id.is_none() && r.run_workspace.is_none() && r.run_task_name.is_none());
    assert!(r.run_task_folder.is_none() && r.run_git_ref.is_none() && r.run_status.is_none());
    assert!(r.run_created_at.is_none() && r.skipped_at.is_none());
    assert!(r.retry_job_id.is_none() && r.retry_status.is_none());
    Ok(())
}

#[tokio::test]
async fn last_run_skips_skipped_rows_but_last_fire_sees_them() -> Result<()> {
    let pool = setup_db().await;
    let t = Utc::now();
    let run = insert_job(
        &pool,
        "trigger",
        "etl/s",
        "t",
        "completed",
        t - Duration::hours(3),
        None,
    )
    .await;
    for h in [2, 1, 0] {
        insert_job(
            &pool,
            "trigger",
            "etl/s",
            "t",
            "skipped",
            t - Duration::hours(h),
            None,
        )
        .await;
    }
    let (_, rows) = TriggerRunRepo::last_runs(&pool, &[key("trigger", "etl/s")]).await?;
    let r = &rows[0];
    assert_eq!(r.run_job_id, Some(run));
    assert_eq!(r.run_status.as_deref(), Some("completed"));
    assert_eq!(
        r.run_created_at.unwrap().timestamp_micros(),
        (t - Duration::hours(3)).timestamp_micros()
    );
    assert_eq!(
        r.skipped_at.unwrap().timestamp_micros(),
        t.timestamp_micros()
    );
    Ok(())
}

#[tokio::test]
async fn skipped_only_trigger_returns_the_skipped_row() -> Result<()> {
    let pool = setup_db().await;
    let t = Utc::now();
    insert_job(
        &pool,
        "trigger",
        "etl/so",
        "t",
        "skipped",
        t - Duration::hours(2),
        None,
    )
    .await;
    let newest = insert_job(
        &pool,
        "trigger",
        "etl/so",
        "t",
        "skipped",
        t - Duration::hours(1),
        None,
    )
    .await;
    let (_, rows) = TriggerRunRepo::last_runs(&pool, &[key("trigger", "etl/so")]).await?;
    let r = &rows[0];
    assert!(r.run_job_id.is_none());
    assert_eq!(r.skipped_job_id, Some(newest));
    assert_eq!(r.skipped_workspace.as_deref(), Some("etl"));
    assert_eq!(r.skipped_task_name.as_deref(), Some("t"));
    assert!(r.skipped_task_folder.is_none() && r.skipped_git_ref.is_none());
    assert_eq!(
        r.skipped_at.unwrap().timestamp_micros(),
        (t - Duration::hours(1)).timestamp_micros()
    );
    Ok(())
}

#[tokio::test]
async fn event_source_last_run_is_the_newest_consumer_not_an_emitted_job() -> Result<()> {
    let pool = setup_db().await;
    let t = Utc::now();
    insert_job(
        &pool,
        "event_source",
        "etl/q",
        "consume",
        "running",
        t - Duration::hours(1),
        Some(json!({"_fingerprint": "f", "_event_source": {}})),
    )
    .await;
    for m in [30, 0] {
        insert_job(
            &pool,
            "event_source",
            "etl/q",
            "handle",
            "completed",
            t - Duration::minutes(m),
            Some(json!({"x": 1})),
        )
        .await;
    }
    let (_, rows) = TriggerRunRepo::last_runs(&pool, &[key("event_source", "etl/q")]).await?;
    assert_eq!(rows[0].run_task_name.as_deref(), Some("consume"));
    Ok(())
}

#[tokio::test]
async fn event_source_legacy_consumer_row_counts() -> Result<()> {
    let pool = setup_db().await;
    insert_job(
        &pool,
        "event_source",
        "etl/legacy",
        "_event_source:feed",
        "running",
        Utc::now(),
        Some(json!({"_fingerprint": "f"})),
    )
    .await;
    let (_, rows) = TriggerRunRepo::last_runs(&pool, &[key("event_source", "etl/legacy")]).await?;
    assert_eq!(rows[0].run_task_name.as_deref(), Some("_event_source:feed"));
    Ok(())
}

#[tokio::test]
async fn event_source_consumer_counts_whatever_its_task_name() -> Result<()> {
    let pool = setup_db().await;
    let t = Utc::now();
    for (task, h) in [("a", 2), ("c", 1)] {
        insert_job(
            &pool,
            "event_source",
            "etl/renamed",
            task,
            "completed",
            t - Duration::hours(h),
            Some(json!({"_fingerprint": "f"})),
        )
        .await;
    }
    let (_, rows) = TriggerRunRepo::last_runs(&pool, &[key("event_source", "etl/renamed")]).await?;
    assert_eq!(rows[0].run_task_name.as_deref(), Some("c"));
    Ok(())
}

#[tokio::test]
async fn retry_chain_result_is_the_highest_attempt() -> Result<()> {
    let pool = setup_db().await;
    let t = Utc::now();
    let run = insert_job(
        &pool,
        "trigger",
        "etl/r",
        "t",
        "failed",
        t - Duration::minutes(10),
        None,
    )
    .await;
    let mut last = Uuid::nil();
    for (attempt, status) in [(1, "failed"), (2, "completed")] {
        last = insert_job(
            &pool,
            "retry",
            "etl/r",
            "t",
            status,
            t - Duration::minutes(10 - attempt),
            None,
        )
        .await;
        sqlx::query("UPDATE job SET retry_of_job_id = $2, retry_attempt = $3 WHERE job_id = $1")
            .bind(last)
            .bind(run)
            .bind(attempt as i32)
            .execute(&pool)
            .await?;
    }
    let (_, rows) = TriggerRunRepo::last_runs(&pool, &[key("trigger", "etl/r")]).await?;
    assert_eq!(rows[0].run_job_id, Some(run));
    assert_eq!(rows[0].retry_status.as_deref(), Some("completed"));
    assert_eq!(rows[0].retry_job_id, Some(last));
    Ok(())
}

#[tokio::test]
async fn last_runs_reports_db_now() -> Result<()> {
    let pool = setup_db().await;
    let (now, rows) = TriggerRunRepo::last_runs(&pool, &[]).await?;
    assert!(rows.is_empty());
    let (db_now,): (DateTime<Utc>,) = sqlx::query_as("SELECT NOW()").fetch_one(&pool).await?;
    assert!((db_now - now).num_seconds().abs() < 5);
    let (now2, rows) = TriggerRunRepo::last_runs(&pool, &[key("trigger", "etl/x")]).await?;
    assert_eq!(rows.len(), 1);
    assert!((db_now - now2).num_seconds().abs() < 5);
    Ok(())
}

#[tokio::test]
async fn last_run_probes_use_the_partial_indexes() -> Result<()> {
    let pool = setup_db().await;
    let t = Utc::now();
    let run = insert_job(
        &pool,
        "trigger",
        "etl/plan",
        "t",
        "failed",
        t - Duration::days(1),
        None,
    )
    .await;
    insert_job(
        &pool,
        "event_source",
        "etl/planq",
        "consume",
        "running",
        t - Duration::days(1),
        Some(json!({"_fingerprint": "f"})),
    )
    .await;
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type, source_id, created_at) \
         SELECT md5(random()::text || g::text || clock_timestamp()::text)::uuid, 'etl', 't', 'distributed', 'skipped', 'trigger', 'etl/plan', NOW() - make_interval(secs => g) \
         FROM generate_series(1, 3000) g",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type, source_id, input, created_at) \
         SELECT md5(random()::text || g::text || clock_timestamp()::text)::uuid, 'etl', 'handle', 'distributed', 'completed', 'event_source', 'etl/planq', '{\"x\":1}'::jsonb, NOW() - make_interval(secs => g) \
         FROM generate_series(1, 3000) g",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO job (job_id, workspace, task_name, mode, status, source_type, source_id, created_at, retry_of_job_id, retry_attempt) \
         SELECT md5(random()::text || g::text || clock_timestamp()::text)::uuid, 'etl', 't', 'distributed', 'failed', 'retry', 'etl/plan', NOW(), $1, g \
         FROM generate_series(1, 20) g",
    )
    .bind(run)
    .execute(&pool)
    .await?;
    sqlx::query("ANALYZE job").execute(&pool).await?;

    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await?;
    // AssertSqlSafe: EXPLAIN prefix on the repo's own constant query text; no user input.
    let plan: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "EXPLAIN {}",
        TriggerRunRepo::LAST_RUNS_SQL
    )))
    .bind(vec!["trigger".to_string(), "event_source".to_string()])
    .bind(vec!["etl/plan".to_string(), "etl/planq".to_string()])
    .fetch_all(&mut *tx)
    .await?;
    let text: String = plan.into_iter().map(|r| r.0).collect::<Vec<_>>().join("\n");
    for idx in [
        "idx_job_trigger_runs",
        "idx_job_trigger_skipped",
        "idx_job_retry_of",
    ] {
        assert!(text.contains(idx), "plan must use {idx}: {text}");
    }
    assert!(!text.contains("Seq Scan on job"), "{text}");
    tx.rollback().await?;
    Ok(())
}
