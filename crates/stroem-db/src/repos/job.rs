use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

/// Job row from database
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRow {
    pub job_id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub mode: String,
    pub input: Option<JsonValue>,
    pub output: Option<JsonValue>,
    pub status: String,
    pub source_type: String,
    pub source_id: Option<String>,
    pub worker_id: Option<Uuid>,
    pub revision: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub log_path: Option<String>,
    pub parent_job_id: Option<Uuid>,
    pub parent_step_name: Option<String>,
}

/// Repository for job operations
pub struct JobRepo;

impl JobRepo {
    /// Create a new job
    pub async fn create(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        mode: &str,
        input: Option<JsonValue>,
        source_type: &str,
        source_id: Option<&str>,
    ) -> Result<Uuid> {
        Self::create_with_parent(
            pool,
            workspace,
            task_name,
            mode,
            input,
            source_type,
            source_id,
            None,
            None,
        )
        .await
    }

    /// Create a new job with optional parent tracking (for type: task sub-jobs)
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_parent(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        mode: &str,
        input: Option<JsonValue>,
        source_type: &str,
        source_id: Option<&str>,
        parent_job_id: Option<Uuid>,
        parent_step_name: Option<&str>,
    ) -> Result<Uuid> {
        let job_id = Uuid::new_v4();

        sqlx::query(
            r#"
            INSERT INTO job (job_id, workspace, task_name, mode, input, source_type, source_id, parent_job_id, parent_step_name)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(job_id)
        .bind(workspace)
        .bind(task_name)
        .bind(mode)
        .bind(input)
        .bind(source_type)
        .bind(source_id)
        .bind(parent_job_id)
        .bind(parent_step_name)
        .execute(pool)
        .await?;

        Ok(job_id)
    }

    /// Get job by ID
    pub async fn get(pool: &PgPool, job_id: Uuid) -> Result<Option<JobRow>> {
        let job = sqlx::query_as::<_, JobRow>(
            r#"
            SELECT job_id, workspace, task_name, mode, input, output, status, source_type,
                   source_id, worker_id, revision, created_at, started_at, completed_at, log_path,
                   parent_job_id, parent_step_name
            FROM job
            WHERE job_id = $1
            "#,
        )
        .bind(job_id)
        .fetch_optional(pool)
        .await?;

        Ok(job)
    }

    /// List jobs with pagination and optional workspace filter
    pub async fn list(
        pool: &PgPool,
        workspace: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JobRow>> {
        let jobs = if let Some(ws) = workspace {
            sqlx::query_as::<_, JobRow>(
                r#"
                SELECT job_id, workspace, task_name, mode, input, output, status, source_type,
                       source_id, worker_id, revision, created_at, started_at, completed_at, log_path,
                       parent_job_id, parent_step_name
                FROM job
                WHERE workspace = $1
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(ws)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await?
        } else {
            sqlx::query_as::<_, JobRow>(
                r#"
                SELECT job_id, workspace, task_name, mode, input, output, status, source_type,
                       source_id, worker_id, revision, created_at, started_at, completed_at, log_path,
                       parent_job_id, parent_step_name
                FROM job
                ORDER BY created_at DESC
                LIMIT $1 OFFSET $2
                "#,
            )
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await?
        };

        Ok(jobs)
    }

    /// Update job status
    pub async fn update_status(pool: &PgPool, job_id: Uuid, status: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET status = $1
            WHERE job_id = $2
            "#,
        )
        .bind(status)
        .bind(job_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Mark job as running with a worker
    pub async fn mark_running(pool: &PgPool, job_id: Uuid, worker_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET status = 'running', worker_id = $1, started_at = NOW()
            WHERE job_id = $2
            "#,
        )
        .bind(worker_id)
        .bind(job_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Transition job from pending to running (idempotent — no-op if already running/completed/failed)
    pub async fn mark_running_if_pending(
        pool: &PgPool,
        job_id: Uuid,
        worker_id: Uuid,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET status = 'running', worker_id = $1, started_at = NOW()
            WHERE job_id = $2 AND status = 'pending'
            "#,
        )
        .bind(worker_id)
        .bind(job_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Transition job from pending to running without a worker (server-side dispatch for type: task)
    pub async fn mark_running_if_pending_server(pool: &PgPool, job_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET status = 'running', started_at = NOW()
            WHERE job_id = $1 AND status = 'pending'
            "#,
        )
        .bind(job_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Mark job as completed
    pub async fn mark_completed(
        pool: &PgPool,
        job_id: Uuid,
        output: Option<JsonValue>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET status = 'completed', output = $1, completed_at = NOW()
            WHERE job_id = $2
            "#,
        )
        .bind(output)
        .bind(job_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Mark job as failed
    pub async fn mark_failed(pool: &PgPool, job_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET status = 'failed', completed_at = NOW()
            WHERE job_id = $1
            "#,
        )
        .bind(job_id)
        .execute(pool)
        .await?;

        Ok(())
    }

    /// List jobs by workspace + task name with pagination
    pub async fn list_by_task(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JobRow>> {
        let jobs = sqlx::query_as::<_, JobRow>(
            r#"
            SELECT job_id, workspace, task_name, mode, input, output, status, source_type,
                   source_id, worker_id, revision, created_at, started_at, completed_at, log_path,
                   parent_job_id, parent_step_name
            FROM job
            WHERE workspace = $1 AND task_name = $2
            ORDER BY created_at DESC
            LIMIT $3 OFFSET $4
            "#,
        )
        .bind(workspace)
        .bind(task_name)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        Ok(jobs)
    }

    /// List jobs by worker ID with pagination
    pub async fn list_by_worker(
        pool: &PgPool,
        worker_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JobRow>> {
        let jobs = sqlx::query_as::<_, JobRow>(
            r#"
            SELECT job_id, workspace, task_name, mode, input, output, status, source_type,
                   source_id, worker_id, revision, created_at, started_at, completed_at, log_path,
                   parent_job_id, parent_step_name
            FROM job
            WHERE worker_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(worker_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        Ok(jobs)
    }

    /// Set log path
    pub async fn set_log_path(pool: &PgPool, job_id: Uuid, log_path: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET log_path = $1
            WHERE job_id = $2
            "#,
        )
        .bind(log_path)
        .bind(job_id)
        .execute(pool)
        .await?;

        Ok(())
    }
}
