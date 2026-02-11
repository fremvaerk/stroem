use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

/// Worker row from database
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkerRow {
    pub worker_id: Uuid,
    pub name: String,
    pub capabilities: JsonValue,
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
    ) -> Result<()> {
        let capabilities_json = serde_json::to_value(capabilities)?;

        sqlx::query(
            r#"
            INSERT INTO worker (worker_id, name, capabilities, last_heartbeat)
            VALUES ($1, $2, $3, NOW())
            "#,
        )
        .bind(worker_id)
        .bind(name)
        .bind(capabilities_json)
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Update worker heartbeat timestamp
    pub async fn heartbeat(pool: &PgPool, worker_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE worker
            SET last_heartbeat = NOW()
            WHERE worker_id = $1
            "#,
        )
        .bind(worker_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Get worker by ID
    pub async fn get(pool: &PgPool, worker_id: Uuid) -> Result<Option<WorkerRow>> {
        let worker = sqlx::query_as::<_, WorkerRow>(
            r#"
            SELECT worker_id, name, capabilities, last_heartbeat, registered_at, status
            FROM worker
            WHERE worker_id = $1
            "#,
        )
        .bind(worker_id)
        .fetch_optional(pool)
        .await?;

        Ok(worker)
    }
}
