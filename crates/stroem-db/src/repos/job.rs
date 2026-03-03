use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

const JOB_COLUMNS: &str = "job_id, workspace, task_name, mode, input, output, status, source_type, source_id, worker_id, revision, created_at, started_at, completed_at, log_path, parent_job_id, parent_step_name";

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
        Self::create_with_parent_tx(
            pool,
            workspace,
            task_name,
            mode,
            input,
            source_type,
            source_id,
            parent_job_id,
            parent_step_name,
        )
        .await
    }

    /// Create a new job with optional parent tracking, accepting a generic executor.
    ///
    /// Use this variant inside transactions. The `pool`-based [`create_with_parent`]
    /// delegates here. Generates a new UUID internally; use [`create_with_parent_tx_id`]
    /// when the caller needs the job ID before commit.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_parent_tx<'e, E>(
        executor: E,
        workspace: &str,
        task_name: &str,
        mode: &str,
        input: Option<JsonValue>,
        source_type: &str,
        source_id: Option<&str>,
        parent_job_id: Option<Uuid>,
        parent_step_name: Option<&str>,
    ) -> Result<Uuid>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        Self::create_with_parent_tx_id(
            executor,
            Uuid::new_v4(),
            workspace,
            task_name,
            mode,
            input,
            source_type,
            source_id,
            parent_job_id,
            parent_step_name,
        )
        .await
    }

    /// Like [`create_with_parent_tx`] but uses a caller-provided job ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_parent_tx_id<'e, E>(
        executor: E,
        job_id: Uuid,
        workspace: &str,
        task_name: &str,
        mode: &str,
        input: Option<JsonValue>,
        source_type: &str,
        source_id: Option<&str>,
        parent_job_id: Option<Uuid>,
        parent_step_name: Option<&str>,
    ) -> Result<Uuid>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
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
        .execute(executor)
        .await
        .context("Failed to create job")?;

        Ok(job_id)
    }

    /// Get job by ID
    pub async fn get(pool: &PgPool, job_id: Uuid) -> Result<Option<JobRow>> {
        let job = sqlx::query_as::<_, JobRow>(&format!(
            "SELECT {} FROM job WHERE job_id = $1",
            JOB_COLUMNS
        ))
        .bind(job_id)
        .fetch_optional(pool)
        .await
        .context("Failed to get job by ID")?;

        Ok(job)
    }

    /// List jobs with pagination and optional workspace/status filters
    pub async fn list(
        pool: &PgPool,
        workspace: Option<&str>,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JobRow>> {
        let mut conditions = Vec::new();
        let mut param_idx = 1u32;

        if workspace.is_some() {
            conditions.push(format!("workspace = ${param_idx}"));
            param_idx += 1;
        }
        if status.is_some() {
            conditions.push(format!("status = ${param_idx}"));
            param_idx += 1;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let limit_idx = param_idx;
        let offset_idx = param_idx + 1;

        let sql = format!(
            "SELECT {} FROM job{} ORDER BY created_at DESC LIMIT ${limit_idx} OFFSET ${offset_idx}",
            JOB_COLUMNS, where_clause
        );

        let mut query = sqlx::query_as::<_, JobRow>(&sql);
        if let Some(ws) = workspace {
            query = query.bind(ws);
        }
        if let Some(s) = status {
            query = query.bind(s);
        }
        query = query.bind(limit).bind(offset);

        let jobs = query.fetch_all(pool).await.context("Failed to list jobs")?;

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
        .await
        .context("Failed to update job status")?;

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
        .await
        .context("Failed to mark job as running")?;

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
        .await
        .context("Failed to mark job as running (if pending)")?;

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
        .await
        .context("Failed to mark job as running (server-side)")?;

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
        .await
        .context("Failed to mark job as completed")?;

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
        .await
        .context("Failed to mark job as failed")?;

        Ok(())
    }

    /// Count jobs with optional workspace/status filters (mirrors `list()`)
    pub async fn count(
        pool: &PgPool,
        workspace: Option<&str>,
        status: Option<&str>,
    ) -> Result<i64> {
        let mut conditions = Vec::new();
        let mut param_idx = 1u32;

        if workspace.is_some() {
            conditions.push(format!("workspace = ${param_idx}"));
            param_idx += 1;
        }
        if status.is_some() {
            conditions.push(format!("status = ${param_idx}"));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let sql = format!("SELECT COUNT(*) FROM job{where_clause}");

        let mut query = sqlx::query_as::<_, (i64,)>(&sql);
        if let Some(ws) = workspace {
            query = query.bind(ws);
        }
        if let Some(s) = status {
            query = query.bind(s);
        }

        let count = query
            .fetch_one(pool)
            .await
            .context("Failed to count jobs")?;
        Ok(count.0)
    }

    /// Count jobs by workspace + task name with optional status filter (mirrors `list_by_task()`)
    pub async fn count_by_task(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        status: Option<&str>,
    ) -> Result<i64> {
        let sql = if status.is_some() {
            "SELECT COUNT(*) FROM job WHERE workspace = $1 AND task_name = $2 AND status = $3"
        } else {
            "SELECT COUNT(*) FROM job WHERE workspace = $1 AND task_name = $2"
        };
        let mut query = sqlx::query_as::<_, (i64,)>(sql)
            .bind(workspace)
            .bind(task_name);
        if let Some(s) = status {
            query = query.bind(s);
        }
        let count = query
            .fetch_one(pool)
            .await
            .context("Failed to count jobs by task")?;
        Ok(count.0)
    }

    /// List jobs by workspace + task name with pagination and optional status filter
    pub async fn list_by_task(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JobRow>> {
        let sql = if status.is_some() {
            format!(
                "SELECT {} FROM job WHERE workspace = $1 AND task_name = $2 AND status = $3 ORDER BY created_at DESC LIMIT $4 OFFSET $5",
                JOB_COLUMNS
            )
        } else {
            format!(
                "SELECT {} FROM job WHERE workspace = $1 AND task_name = $2 ORDER BY created_at DESC LIMIT $3 OFFSET $4",
                JOB_COLUMNS
            )
        };
        let mut query = sqlx::query_as::<_, JobRow>(&sql)
            .bind(workspace)
            .bind(task_name);
        if let Some(s) = status {
            query = query.bind(s);
        }
        let jobs = query
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await
            .context("Failed to list jobs by task")?;

        Ok(jobs)
    }

    /// List jobs by worker ID with pagination
    pub async fn list_by_worker(
        pool: &PgPool,
        worker_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JobRow>> {
        let jobs = sqlx::query_as::<_, JobRow>(&format!(
            "SELECT {} FROM job WHERE worker_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
            JOB_COLUMNS
        ))
        .bind(worker_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .context("Failed to list jobs by worker")?;

        Ok(jobs)
    }

    /// Cancel a job (set status to cancelled, completed_at to NOW).
    /// Only cancels jobs that are pending or running. Returns true if the job was updated.
    pub async fn cancel(pool: &PgPool, job_id: Uuid) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE job
            SET status = 'cancelled', completed_at = NOW()
            WHERE job_id = $1 AND status IN ('pending', 'running')
            "#,
        )
        .bind(job_id)
        .execute(pool)
        .await
        .context("Failed to cancel job")?;

        Ok(result.rows_affected() > 0)
    }

    /// Get active child jobs for a parent job (for recursive cancellation)
    pub async fn get_child_jobs(pool: &PgPool, parent_job_id: Uuid) -> Result<Vec<JobRow>> {
        let jobs = sqlx::query_as::<_, JobRow>(&format!(
            "SELECT {} FROM job WHERE parent_job_id = $1 AND status IN ('pending', 'running')",
            JOB_COLUMNS
        ))
        .bind(parent_job_id)
        .fetch_all(pool)
        .await
        .context("Failed to get child jobs")?;

        Ok(jobs)
    }

    /// Get job counts grouped by status (used for dashboard stats)
    pub async fn get_status_counts(pool: &PgPool) -> Result<HashMap<String, i64>> {
        let rows =
            sqlx::query_as::<_, (String, i64)>("SELECT status, COUNT(*) FROM job GROUP BY status")
                .fetch_all(pool)
                .await
                .context("Failed to get job status counts")?;

        let mut counts = HashMap::new();
        for (status, count) in rows {
            counts.insert(status, count);
        }
        Ok(counts)
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
        .await
        .context("Failed to set job log path")?;

        Ok(())
    }
}
