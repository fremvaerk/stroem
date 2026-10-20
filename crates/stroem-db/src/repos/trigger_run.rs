use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// A trigger identity as stored on its jobs: `source_type` is `trigger`,
/// `webhook` or `event_source`; `source_id` is `"{workspace}/{trigger}"`.
#[derive(Debug, Clone)]
pub struct TriggerRunKey {
    pub source_type: String,
    pub source_id: String,
}

/// One trigger's last run, last skipped fire and retry result.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TriggerRunRow {
    pub db_now: DateTime<Utc>,
    pub source_type: String,
    pub source_id: String,
    pub run_job_id: Option<Uuid>,
    pub run_workspace: Option<String>,
    pub run_task_name: Option<String>,
    pub run_task_folder: Option<String>,
    pub run_git_ref: Option<String>,
    pub run_status: Option<String>,
    pub run_created_at: Option<DateTime<Utc>>,
    /// Newest `skipped` fire (scheduler only).
    pub skipped_at: Option<DateTime<Utc>>,
    pub retry_job_id: Option<Uuid>,
    pub retry_status: Option<String>,
}

pub struct TriggerRunRepo;

impl TriggerRunRepo {
    /// Every `WHERE` repeats the predicate of the partial index it probes
    /// (`idx_job_trigger_runs`, `idx_job_trigger_skipped`, `idx_job_retry_of`)
    /// literally, so the planner can prove it (migration 053).
    pub const LAST_RUNS_SQL: &'static str = "\
SELECT NOW() AS db_now, k.source_type, k.source_id, \
       r.job_id AS run_job_id, r.workspace AS run_workspace, r.task_name AS run_task_name, \
       r.task_folder AS run_task_folder, r.git_ref AS run_git_ref, r.status AS run_status, \
       r.created_at AS run_created_at, \
       s.created_at AS skipped_at, t.job_id AS retry_job_id, t.status AS retry_status \
FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS k(source_type, source_id, ord) \
LEFT JOIN LATERAL ( \
    SELECT j.job_id, j.workspace, j.task_name, j.task_folder, j.git_ref, j.status, j.created_at \
    FROM job j \
    WHERE j.source_type = k.source_type AND j.source_id = k.source_id \
      AND ((j.source_type IN ('trigger', 'webhook') AND j.status <> 'skipped') \
           OR (j.source_type = 'event_source' AND j.input ? '_fingerprint')) \
    ORDER BY j.created_at DESC LIMIT 1) r ON true \
LEFT JOIN LATERAL ( \
    SELECT j.created_at FROM job j \
    WHERE k.source_type = 'trigger' \
      AND j.source_type = 'trigger' AND j.source_id = k.source_id AND j.status = 'skipped' \
    ORDER BY j.created_at DESC LIMIT 1) s ON true \
LEFT JOIN LATERAL ( \
    SELECT j.job_id, j.status FROM job j \
    WHERE j.retry_of_job_id = r.job_id \
    ORDER BY j.retry_attempt DESC LIMIT 1) t ON true \
ORDER BY k.ord";

    /// One row per key, in input order, with the DB's `NOW()`.
    #[tracing::instrument(skip_all, fields(keys = keys.len()))]
    pub async fn last_runs(
        pool: &PgPool,
        keys: &[TriggerRunKey],
    ) -> Result<(DateTime<Utc>, Vec<TriggerRunRow>)> {
        if keys.is_empty() {
            let (now,): (DateTime<Utc>,) = sqlx::query_as("SELECT NOW()")
                .fetch_one(pool)
                .await
                .context("Failed to read the database clock")?;
            return Ok((now, Vec::new()));
        }
        let types: Vec<&str> = keys.iter().map(|k| k.source_type.as_str()).collect();
        let ids: Vec<&str> = keys.iter().map(|k| k.source_id.as_str()).collect();
        let rows: Vec<TriggerRunRow> = sqlx::query_as(Self::LAST_RUNS_SQL)
            .bind(types)
            .bind(ids)
            .fetch_all(pool)
            .await
            .context("Failed to look up trigger last runs")?;
        let now = rows.first().map(|r| r.db_now).unwrap_or_else(Utc::now);
        Ok((now, rows))
    }
}
