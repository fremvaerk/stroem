use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

const WORKER_COLUMNS: &str =
    "worker_id, name, capabilities, tags, last_heartbeat, registered_at, status";

/// Worker row from database
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkerRow {
    pub worker_id: Uuid,
    pub name: String,
    pub capabilities: JsonValue,
    pub tags: JsonValue,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub registered_at: DateTime<Utc>,
    pub status: String,
}

/// Repository for worker operations
pub struct WorkerRepo;

impl WorkerRepo {
    /// Register a new worker
    pub async fn register(
        pool: &PgPool,
        worker_id: Uuid,
        name: &str,
        capabilities: &[String],
        tags: &[String],
    ) -> Result<()> {
        let capabilities_json = serde_json::to_value(capabilities)?;
        let tags_json = serde_json::to_value(tags)?;

        sqlx::query(
            r#"
            INSERT INTO worker (worker_id, name, capabilities, tags, last_heartbeat)
            VALUES ($1, $2, $3, $4, NOW())
            "#,
        )
        .bind(worker_id)
        .bind(name)
        .bind(capabilities_json)
        .bind(tags_json)
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
