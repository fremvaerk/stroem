use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

const WORKER_COLUMNS: &str =
    "worker_id, name, tags, last_heartbeat, registered_at, status, version";

/// Worker row from database
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkerRow {
    pub worker_id: Uuid,
    pub name: String,
    pub tags: JsonValue,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub registered_at: DateTime<Utc>,
    pub status: String,
    pub version: Option<String>,
}

/// Repository for worker operations
pub struct WorkerRepo;

impl WorkerRepo {
    /// Register a new worker
    pub async fn register(
        pool: &PgPool,
        worker_id: Uuid,
        name: &str,
        tags: &[String],
        version: Option<&str>,
    ) -> Result<()> {
        let tags_json = serde_json::to_value(tags).context("Failed to serialize tags")?;

        sqlx::query(
            r#"
            INSERT INTO worker (worker_id, name, tags, last_heartbeat, version)
            VALUES ($1, $2, $3, NOW(), $4)
            "#,
        )
        .bind(worker_id)
        .bind(name)
        .bind(tags_json)
        .bind(version)
        .execute(pool)
        .await
        .context("Failed to register worker")?;

        Ok(())
    }

    /// Update worker heartbeat timestamp and ensure status is active
    pub async fn heartbeat(pool: &PgPool, worker_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE worker
            SET last_heartbeat = NOW(), status = 'active'
            WHERE worker_id = $1
            "#,
        )
        .bind(worker_id)
        .execute(pool)
        .await
        .context("Failed to update worker heartbeat")?;

        Ok(())
    }

    /// Mark workers as inactive if their heartbeat is older than the threshold.
    /// Returns the IDs of newly-inactivated workers.
    pub async fn mark_stale_inactive(
        pool: &PgPool,
        heartbeat_timeout_secs: i64,
    ) -> Result<Vec<Uuid>> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"
            UPDATE worker
            SET status = 'inactive'
            WHERE status = 'active'
              AND last_heartbeat < NOW() - make_interval(secs => $1::double precision)
            RETURNING worker_id
            "#,
        )
        .bind(heartbeat_timeout_secs as f64)
        .fetch_all(pool)
        .await
        .context("Failed to mark stale workers inactive")?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Get worker by ID
    pub async fn get(pool: &PgPool, worker_id: Uuid) -> Result<Option<WorkerRow>> {
        let worker = sqlx::query_as::<_, WorkerRow>(&format!(
            "SELECT {} FROM worker WHERE worker_id = $1",
            WORKER_COLUMNS
        ))
        .bind(worker_id)
        .fetch_optional(pool)
        .await
        .context("Failed to get worker by ID")?;

        Ok(worker)
    }

    /// Count all workers
    pub async fn count(pool: &PgPool) -> Result<i64> {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM worker")
            .fetch_one(pool)
            .await
            .context("Failed to count workers")?;
        Ok(count.0)
    }

    /// Delete inactive workers whose last heartbeat is older than the given threshold.
    /// Returns the number of deleted rows.
    pub async fn delete_stale(pool: &PgPool, retention_hours: f64) -> Result<u64> {
        let result = sqlx::query(
            r#"
            DELETE FROM worker
            WHERE status = 'inactive'
              AND last_heartbeat < NOW() - make_interval(secs => $1::double precision)
            "#,
        )
        .bind(retention_hours * 3600.0)
        .execute(pool)
        .await
        .context("Failed to delete stale workers")?;
        Ok(result.rows_affected())
    }

    /// List workers ordered by status (active first), then by registered_at descending
    pub async fn list(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<WorkerRow>> {
        let workers = sqlx::query_as::<_, WorkerRow>(&format!(
            "SELECT {} FROM worker ORDER BY status ASC, registered_at DESC LIMIT $1 OFFSET $2",
            WORKER_COLUMNS
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .context("Failed to list workers")?;

        Ok(workers)
    }
}
