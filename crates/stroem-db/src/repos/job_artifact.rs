use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct JobArtifactRecord {
    pub id: Uuid,
    pub job_id: Uuid,
    pub step_name: String,
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_key: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewArtifactRow {
    pub job_id: Uuid,
    pub step_name: String,
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_key: String,
}

pub struct JobArtifactRepo {
    pool: PgPool,
}

impl JobArtifactRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn upsert(&self, row: NewArtifactRow) -> Result<JobArtifactRecord> {
        let id = Uuid::new_v4();
        let rec = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                String,
                String,
                String,
                i64,
                String,
                DateTime<Utc>,
            ),
        >(
            r#"
            INSERT INTO job_artifact
                (id, job_id, step_name, name, content_type, size_bytes, storage_key)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (job_id, name) DO UPDATE
                SET step_name    = EXCLUDED.step_name,
                    content_type = EXCLUDED.content_type,
                    size_bytes   = EXCLUDED.size_bytes,
                    storage_key  = EXCLUDED.storage_key,
                    created_at   = NOW()
            RETURNING id, job_id, step_name, name, content_type, size_bytes, storage_key, created_at
            "#,
        )
        .bind(id)
        .bind(row.job_id)
        .bind(&row.step_name)
        .bind(&row.name)
        .bind(&row.content_type)
        .bind(row.size_bytes)
        .bind(&row.storage_key)
        .fetch_one(&self.pool)
        .await?;

        Ok(JobArtifactRecord {
            id: rec.0,
            job_id: rec.1,
            step_name: rec.2,
            name: rec.3,
            content_type: rec.4,
            size_bytes: rec.5,
            storage_key: rec.6,
            created_at: rec.7,
        })
    }

    pub async fn list_for_job(&self, job_id: Uuid) -> Result<Vec<JobArtifactRecord>> {
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                String,
                String,
                String,
                i64,
                String,
                DateTime<Utc>,
            ),
        >(
            "SELECT id, job_id, step_name, name, content_type, size_bytes, storage_key, created_at
             FROM job_artifact WHERE job_id = $1 ORDER BY created_at ASC, name ASC",
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| JobArtifactRecord {
                id: r.0,
                job_id: r.1,
                step_name: r.2,
                name: r.3,
                content_type: r.4,
                size_bytes: r.5,
                storage_key: r.6,
                created_at: r.7,
            })
            .collect())
    }

    pub async fn get_by_name(&self, job_id: Uuid, name: &str) -> Result<Option<JobArtifactRecord>> {
        let row = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                String,
                String,
                String,
                i64,
                String,
                DateTime<Utc>,
            ),
        >(
            "SELECT id, job_id, step_name, name, content_type, size_bytes, storage_key, created_at
             FROM job_artifact WHERE job_id = $1 AND name = $2",
        )
        .bind(job_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| JobArtifactRecord {
            id: r.0,
            job_id: r.1,
            step_name: r.2,
            name: r.3,
            content_type: r.4,
            size_bytes: r.5,
            storage_key: r.6,
            created_at: r.7,
        }))
    }

    pub async fn total_size_for_job(&self, job_id: Uuid) -> Result<i64> {
        let total: (Option<i64>,) = sqlx::query_as(
            "SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM job_artifact WHERE job_id = $1",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(total.0.unwrap_or(0))
    }

    pub async fn delete_for_job(&self, job_id: Uuid) -> Result<u64> {
        let res = sqlx::query("DELETE FROM job_artifact WHERE job_id = $1")
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    pub async fn delete_for_step(&self, job_id: Uuid, step_name: &str) -> Result<u64> {
        let res = sqlx::query("DELETE FROM job_artifact WHERE job_id = $1 AND step_name = $2")
            .bind(job_id)
            .bind(step_name)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}
