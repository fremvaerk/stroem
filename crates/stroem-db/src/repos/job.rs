use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::{AssertSqlSafe, PgPool};
use std::collections::HashMap;
use stroem_common::models::job::JobStatus;
use uuid::Uuid;

/// Maximum `type: task` nesting depth, mirroring `job_creator::MAX_TASK_DEPTH`.
/// Bounds the descendant walk in
/// [`JobRepo::get_settled_descendants_with_running_parent_step`].
const MAX_TASK_DEPTH: i32 = 10;

/// The [`JobRow`] column list. A macro rather than a `const` so a query can
/// `concat!` it into a `&'static str`, which sqlx accepts as SQL directly.
macro_rules! job_columns {
    () => {
        "job_id, workspace, task_name, mode, input, output, status, source_type, source_id, worker_id, revision, created_at, started_at, completed_at, log_path, parent_job_id, parent_step_name, timeout_secs, retry_of_job_id, retry_job_id, retry_attempt, max_retries, raw_input, source_job_id, restart_from_step, git_ref, task_folder"
    };
}
const JOB_COLUMNS: &str = job_columns!();

/// Escape LIKE/ILIKE special characters so the search term is a pure substring match.
fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

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
    pub timeout_secs: Option<i32>,
    pub retry_of_job_id: Option<Uuid>,
    pub retry_job_id: Option<Uuid>,
    pub retry_attempt: i32,
    pub max_retries: Option<i32>,
    pub raw_input: Option<JsonValue>,
    pub source_job_id: Option<Uuid>,
    pub restart_from_step: Option<String>,
    /// The spec's `job.ref` (2026-10-02 § 6): the ref string as written, for a
    /// job created in owner@ref. `revision` holds its commit. `Some` ⇔ pinned job.
    pub git_ref: Option<String>,
    /// For a pinned job, the task's `folder` in the pinned config (`None` = no
    /// folder). The pinned job's ACL folder (§ 7.8).
    pub task_folder: Option<String>,
}

impl JobRow {
    /// Minimal row for tests that only exercise pure decision logic (e.g.
    /// settlement's `plan`). Not for use against a real database.
    #[doc(hidden)]
    pub fn test_default() -> Self {
        JobRow {
            job_id: Uuid::new_v4(),
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            mode: "distributed".to_string(),
            input: None,
            output: None,
            status: "pending".to_string(),
            source_type: "api".to_string(),
            source_id: None,
            worker_id: None,
            revision: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            log_path: None,
            parent_job_id: None,
            parent_step_name: None,
            timeout_secs: None,
            retry_of_job_id: None,
            retry_job_id: None,
            retry_attempt: 0,
            max_retries: None,
            raw_input: None,
            source_job_id: None,
            restart_from_step: None,
            git_ref: None,
            task_folder: None,
        }
    }
}

/// Lightweight projection returned by [`JobRepo::get_old_terminal_jobs`].
///
/// Contains only the fields needed to construct a `JobLogMeta` and to identify
/// the job for deletion — avoids a secondary per-job `get()` call.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RetentionJobInfo {
    pub job_id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub created_at: DateTime<Utc>,
}

/// A row of [`JobRepo::get_stalled_pinned_jobs`]: the job and its own pin's
/// workspace + commit (`revision`), so recovery can skip the other jobs of a
/// pin it just failed to load.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StalledPinnedJob {
    pub job_id: Uuid,
    pub workspace: String,
    pub revision: Option<String>,
}

/// Aggregate duration statistics over a window of completed jobs.
///
/// `sample_size` is the row count behind the percentiles. Callers should treat
/// the percentile fields as `None` (or hide them) when the sample is too small
/// to be meaningful — recommended threshold is 5.
///
/// **Inclusion policy**: `source_type='rerun'` jobs are counted as independent
/// runs because they represent real production execution durations, not
/// duplicates of prior runs. Single retry attempts (`source_type='retry'`) are
/// also counted — each completed retry is a real run whose duration matters for
/// performance analysis. Jobs are filtered to `completed_at >= started_at` to
/// exclude any rows with clock-skew anomalies.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DurationStatsRow {
    pub sample_size: i64,
    pub avg_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

/// One historical run, used to draw a duration history sparkline.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RecentDurationRow {
    pub job_id: Uuid,
    pub duration_ms: f64,
    pub completed_at: DateTime<Utc>,
}

/// The bounds of [`JobRepo::redaction_closure_pins`]. A bound never cuts a
/// walk silently. An edge it refuses, or a closure larger than `max_jobs`,
/// makes the result [`RedactionClosure::Truncated`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosureBounds {
    /// `type: task` nesting: the levels walked up to a root, and down from it.
    pub task_depth: i32,
    /// `source_type = 'hook'` links followed to the job that fired the hook.
    pub hook_hops: i32,
    /// `source_type = 'restart'` and `source_type = 'rerun'` links followed
    /// to the source job (`source_job_id`), counted together.
    pub source_hops: i32,
    /// `retry_of_job_id` links followed to the job a task retry re-runs.
    pub retry_hops: i32,
    /// Jobs in the closure (each walk counted separately).
    pub max_jobs: i64,
}

/// What [`JobRepo::redaction_closure_pins`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactionClosure {
    /// The whole closure was read: these are its distinct pins.
    Pins(Vec<ClosurePinRow>),
    /// A bound refused an edge (a parent, a child, a hook, restart, re-run
    /// or retry source), or the closure holds more than `max_jobs` jobs. Pins beyond
    /// the bound are unknown, so the caller must fail closed.
    Truncated,
}

/// One pin referenced inside a job's redaction closure
/// ([`JobRepo::redaction_closure_pins`]): the owner workspace, the ref as
/// written and the commit it resolved to.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ClosurePinRow {
    pub workspace: String,
    pub git_ref: String,
    pub revision: String,
}

/// Pin columns of a job created in owner@ref (spec 2026-10-02 § 6). The commit
/// itself goes in the `revision` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobPinCols {
    pub git_ref: String,
    pub task_folder: Option<String>,
}

/// Authorisation for job lists and counts (spec 2026-10-02 § 7.8), applied in
/// SQL before ordering and pagination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobAclScope {
    /// `(workspace, task_name)` the user may see — for UNPINNED jobs, whose
    /// folder is the live task's.
    pub live_pairs: Vec<(String, String)>,
    /// `(workspace, task_name, folder)` the user may see — for PINNED jobs,
    /// whose folder is their own `task_folder` (`""` = no folder).
    pub pinned_triples: Vec<(String, String, String)>,
}

impl JobAclScope {
    pub fn is_empty(&self) -> bool {
        self.live_pairs.is_empty() && self.pinned_triples.is_empty()
    }

    /// The predicate with placeholders from `$first`, and the next free index.
    /// Callers must not call it on an empty scope (`IN ()` is not SQL).
    fn predicate(&self, first: u32) -> (String, u32) {
        let mut idx = first;
        let mut parts = Vec::new();
        if !self.live_pairs.is_empty() {
            let values: Vec<String> = self
                .live_pairs
                .iter()
                .map(|_| {
                    let v = format!("(${}, ${})", idx, idx + 1);
                    idx += 2;
                    v
                })
                .collect();
            parts.push(format!(
                "(git_ref IS NULL AND (workspace, task_name) IN ({}))",
                values.join(", ")
            ));
        }
        if !self.pinned_triples.is_empty() {
            let values: Vec<String> = self
                .pinned_triples
                .iter()
                .map(|_| {
                    let v = format!("(${}, ${}, ${})", idx, idx + 1, idx + 2);
                    idx += 3;
                    v
                })
                .collect();
            parts.push(format!(
                "(git_ref IS NOT NULL AND (workspace, task_name, COALESCE(task_folder, '')) IN ({}))",
                values.join(", ")
            ));
        }
        (format!("({})", parts.join(" OR ")), idx)
    }

    /// Bind values in placeholder order.
    fn bind_values(&self) -> impl Iterator<Item = &str> {
        self.live_pairs
            .iter()
            .flat_map(|(w, t)| [w.as_str(), t.as_str()])
            .chain(
                self.pinned_triples
                    .iter()
                    .flat_map(|(w, t, f)| [w.as_str(), t.as_str(), f.as_str()]),
            )
    }
}

/// Repository for job operations
pub struct JobRepo;

impl JobRepo {
    /// Create a new job
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        mode: &str,
        input: Option<JsonValue>,
        source_type: &str,
        source_id: Option<&str>,
        revision: Option<&str>,
        raw_input: Option<JsonValue>,
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
            None,
            revision,
            raw_input,
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
        timeout_secs: Option<i32>,
        revision: Option<&str>,
        raw_input: Option<JsonValue>,
        source_job_id: Option<Uuid>,
        restart_from_step: Option<&str>,
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
            timeout_secs,
            revision,
            raw_input,
            source_job_id,
            restart_from_step,
            None, // max_retries
            None, // pin
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
        timeout_secs: Option<i32>,
        revision: Option<&str>,
        raw_input: Option<JsonValue>,
        source_job_id: Option<Uuid>,
        restart_from_step: Option<&str>,
        max_retries: Option<i32>,
        pin: Option<&JobPinCols>,
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
            timeout_secs,
            revision,
            raw_input,
            source_job_id,
            restart_from_step,
            max_retries,
            pin,
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
        timeout_secs: Option<i32>,
        revision: Option<&str>,
        raw_input: Option<JsonValue>,
        source_job_id: Option<Uuid>,
        restart_from_step: Option<&str>,
        max_retries: Option<i32>,
        pin: Option<&JobPinCols>,
    ) -> Result<Uuid>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        sqlx::query(
            r#"
            INSERT INTO job (job_id, workspace, task_name, mode, input, source_type, source_id, parent_job_id, parent_step_name, timeout_secs, revision, raw_input, source_job_id, restart_from_step, max_retries, git_ref, task_folder)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
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
        .bind(timeout_secs)
        .bind(revision)
        .bind(raw_input)
        .bind(source_job_id)
        .bind(restart_from_step)
        .bind(max_retries)
        .bind(pin.map(|p| p.git_ref.as_str()))
        .bind(pin.and_then(|p| p.task_folder.as_deref()))
        .execute(executor)
        .await
        .context("Failed to create job")?;

        Ok(job_id)
    }

    /// Create a bare "skipped" job record with no steps.
    ///
    /// Used when a `concurrency: skip` trigger fires but an active job already
    /// exists — the job is recorded for visibility but never executed.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_skipped(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        input: Option<JsonValue>,
        source_type: &str,
        source_id: Option<&str>,
        revision: Option<&str>,
        pin: Option<&JobPinCols>,
    ) -> Result<Uuid> {
        let job_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO job (job_id, workspace, task_name, mode, input, status, source_type, source_id, completed_at, revision, git_ref, task_folder)
            VALUES ($1, $2, $3, 'distributed', $4, 'skipped', $5, $6, NOW(), $7, $8, $9)
            "#,
        )
        .bind(job_id)
        .bind(workspace)
        .bind(task_name)
        .bind(input)
        .bind(source_type)
        .bind(source_id)
        .bind(revision)
        .bind(pin.map(|p| p.git_ref.as_str()))
        .bind(pin.and_then(|p| p.task_folder.as_deref()))
        .execute(pool)
        .await
        .context("Failed to create skipped job")?;

        Ok(job_id)
    }

    /// Get job by ID
    pub async fn get(pool: &PgPool, job_id: Uuid) -> Result<Option<JobRow>> {
        let job = sqlx::query_as::<_, JobRow>(concat!(
            "SELECT ",
            job_columns!(),
            " FROM job WHERE job_id = $1"
        ))
        .bind(job_id)
        .fetch_optional(pool)
        .await
        .context("Failed to get job by ID")?;

        Ok(job)
    }

    /// List jobs with pagination and optional workspace/status/source_type/search filters
    pub async fn list(
        pool: &PgPool,
        workspace: Option<&str>,
        status: Option<&str>,
        source_type: Option<&str>,
        search: Option<&str>,
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
        if source_type.is_some() {
            conditions.push(format!("source_type = ${param_idx}"));
            param_idx += 1;
        }
        if search.is_some() {
            conditions.push(format!("(task_name ILIKE ${param_idx} OR workspace ILIKE ${param_idx} OR source_id ILIKE ${param_idx} OR job_id::text ILIKE ${param_idx})"));
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

        // AssertSqlSafe: only constant fragments and `$n` placeholders are interpolated; every value is bound.
        let mut query = sqlx::query_as::<_, JobRow>(AssertSqlSafe(sql));
        if let Some(ws) = workspace {
            query = query.bind(ws);
        }
        if let Some(s) = status {
            query = query.bind(s);
        }
        if let Some(st) = source_type {
            query = query.bind(st);
        }
        if let Some(s) = search {
            query = query.bind(format!("%{}%", escape_like(s)));
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

    /// Transaction variant of `mark_running_if_pending_server`. Zero rows is normal
    /// once the job is already running.
    pub async fn mark_running_if_pending_tx<'e, E>(executor: E, job_id: Uuid) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        sqlx::query("UPDATE job SET status = 'running', started_at = NOW() WHERE job_id = $1 AND status = 'pending'")
            .bind(job_id)
            .execute(executor)
            .await
            .context("mark_running_if_pending_tx")?;
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

    /// Predicated settlement write (spec §7): moves a `pending`/`running` job
    /// to `status` and returns whether the row was written. A `false` means
    /// the row was already terminal — typically an explicit cancellation —
    /// and must not be overwritten. `output` is `COALESCE`d so `failed` and
    /// `cancelled` (which pass `None`) never clear an existing output.
    pub async fn settle(
        pool: &PgPool,
        job_id: Uuid,
        status: JobStatus,
        output: Option<JsonValue>,
    ) -> Result<bool> {
        Self::settle_tx(pool, job_id, status, output).await
    }

    /// Executor-generic variant of [`Self::settle`]. Use inside a transaction
    /// that also writes the job's steps, job row first (the lock order of
    /// `release_claim` and the creation compensation).
    pub async fn settle_tx<'e, E>(
        executor: E,
        job_id: Uuid,
        status: JobStatus,
        output: Option<JsonValue>,
    ) -> Result<bool>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let result = sqlx::query(
            r#"
            UPDATE job
            SET status = $2, output = COALESCE($3, output), completed_at = NOW()
            WHERE job_id = $1 AND status IN ('pending', 'running')
            "#,
        )
        .bind(job_id)
        .bind(status.as_ref())
        .bind(output)
        .execute(executor)
        .await
        .context("Failed to settle job")?;
        Ok(result.rows_affected() > 0)
    }

    /// Link a task-retry job to its chain inside the transaction that creates
    /// it: `retry_of_job_id` (the chain's root) + `retry_attempt` on the new
    /// job, `retry_job_id` on the job that failed. The redaction closure
    /// follows `retry_of_job_id` to the jobs a retry copied its input from, so
    /// a retry row must never be committed without it.
    ///
    /// Takes `&mut PgConnection` (two statements in one transaction) — pass
    /// `&mut tx` at the call site.
    pub async fn link_retry_tx(
        tx: &mut sqlx::PgConnection,
        failed_job_id: Uuid,
        retry_job_id: Uuid,
        root_job_id: Uuid,
        retry_attempt: i32,
    ) -> Result<()> {
        let linked = sqlx::query(
            "UPDATE job SET retry_of_job_id = $1, retry_attempt = $2 WHERE job_id = $3",
        )
        .bind(root_job_id)
        .bind(retry_attempt)
        .bind(retry_job_id)
        .execute(&mut *tx)
        .await
        .context("set retry lineage on the retry job")?;
        if linked.rows_affected() != 1 {
            anyhow::bail!("retry job {retry_job_id} not found while linking its lineage");
        }
        sqlx::query("UPDATE job SET retry_job_id = $1 WHERE job_id = $2")
            .bind(retry_job_id)
            .bind(failed_job_id)
            .execute(&mut *tx)
            .await
            .context("link the failed job to its retry")?;
        Ok(())
    }

    /// Mark job as failed
    pub async fn mark_failed(pool: &PgPool, job_id: Uuid) -> Result<()> {
        Self::mark_failed_tx(pool, job_id).await
    }

    /// Executor-generic variant of [`mark_failed`]. Use inside a transaction
    /// together with [`crate::JobStepRepo::fail_non_terminal_steps_tx`] so a
    /// job and its steps reach `failed` atomically.
    pub async fn mark_failed_tx<'e, E>(executor: E, job_id: Uuid) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        sqlx::query(
            r#"
            UPDATE job
            SET status = 'failed', completed_at = NOW()
            WHERE job_id = $1
            "#,
        )
        .bind(job_id)
        .execute(executor)
        .await
        .context("Failed to mark job as failed")?;

        Ok(())
    }

    /// Mark job as cancelled (stamps `completed_at`, unlike `update_status`).
    pub async fn mark_cancelled(pool: &PgPool, job_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE job
            SET status = 'cancelled', completed_at = NOW()
            WHERE job_id = $1
            "#,
        )
        .bind(job_id)
        .execute(pool)
        .await
        .context("Failed to mark job as cancelled")?;
        Ok(())
    }

    /// Set the retry_job_id on a job (linking original → retry).
    pub async fn set_retry_job_id(pool: &PgPool, job_id: Uuid, retry_job_id: Uuid) -> Result<()> {
        sqlx::query("UPDATE job SET retry_job_id = $1 WHERE job_id = $2")
            .bind(retry_job_id)
            .bind(job_id)
            .execute(pool)
            .await
            .context("Failed to set retry_job_id")?;
        Ok(())
    }

    /// `(workspace, revision)` pairs whose workspace tarball the server must
    /// keep cached:
    /// 1. every non-terminal job's own revision;
    /// 2. cross-workspace action revisions (`job_step.action_revision`) of
    ///    steps in non-terminal jobs — a step claims its OWNER's revision;
    /// 3. failed top-level jobs still owed a task-level retry — settlement
    ///    observes the job terminal before the retry job exists, and the
    ///    retry inherits this revision. Mirrors `terminal::plan`'s retry
    ///    gate; bounded to one hour so a retry that never got created does
    ///    not pin a revision forever.
    pub async fn tarball_keep_revisions(pool: &PgPool) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT workspace, revision FROM job \
              WHERE status IN ('pending', 'running') AND revision IS NOT NULL \
             UNION \
             SELECT s.action_workspace, s.action_revision FROM job_step s \
               JOIN job j ON j.job_id = s.job_id \
              WHERE j.status IN ('pending', 'running') \
                AND s.action_workspace IS NOT NULL AND s.action_revision IS NOT NULL \
             UNION \
             SELECT workspace, revision FROM job \
              WHERE status = 'failed' AND revision IS NOT NULL \
                AND parent_job_id IS NULL AND retry_job_id IS NULL \
                AND max_retries IS NOT NULL AND retry_attempt < max_retries \
                AND completed_at > NOW() - INTERVAL '1 hour'",
        )
        .fetch_all(pool)
        .await
        .context("query tarball keep revisions")?;
        Ok(rows)
    }

    /// `(workspace, commit)` pins the server must keep loaded (spec § 10):
    /// 1. every non-terminal PINNED job's own commit;
    /// 2. the action pin (`action_ref`) and the task pin (`task_ref`) of
    ///    every step in a non-terminal job;
    /// 3. failed pinned top-level jobs still owed a task-level retry
    ///    (the same one-hour window as `tarball_keep_revisions`).
    pub async fn pin_keep_set(pool: &PgPool) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT workspace, revision FROM job \
              WHERE status IN ('pending', 'running') \
                AND git_ref IS NOT NULL AND revision IS NOT NULL \
             UNION \
             SELECT s.action_workspace, s.action_revision FROM job_step s \
               JOIN job j ON j.job_id = s.job_id \
              WHERE j.status IN ('pending', 'running') AND s.action_ref IS NOT NULL \
                AND s.action_workspace IS NOT NULL AND s.action_revision IS NOT NULL \
             UNION \
             SELECT s.task_workspace, s.task_revision FROM job_step s \
               JOIN job j ON j.job_id = s.job_id \
              WHERE j.status IN ('pending', 'running') AND s.task_ref IS NOT NULL \
                AND s.task_workspace IS NOT NULL AND s.task_revision IS NOT NULL \
             UNION \
             SELECT workspace, revision FROM job \
              WHERE status = 'failed' AND git_ref IS NOT NULL AND revision IS NOT NULL \
                AND parent_job_id IS NULL AND retry_job_id IS NULL \
                AND max_retries IS NOT NULL AND retry_attempt < max_retries \
                AND completed_at > NOW() - INTERVAL '1 hour'",
        )
        .fetch_all(pool)
        .await
        .context("query pin keep set")?;
        Ok(rows)
    }

    /// Count jobs with optional workspace/status/source_type/search filters (mirrors `list()`)
    pub async fn count(
        pool: &PgPool,
        workspace: Option<&str>,
        status: Option<&str>,
        source_type: Option<&str>,
        search: Option<&str>,
    ) -> Result<i64> {
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
        if source_type.is_some() {
            conditions.push(format!("source_type = ${param_idx}"));
            param_idx += 1;
        }
        if search.is_some() {
            conditions.push(format!("(task_name ILIKE ${param_idx} OR workspace ILIKE ${param_idx} OR source_id ILIKE ${param_idx} OR job_id::text ILIKE ${param_idx})"));
            param_idx += 1;
        }
        // suppress unused warning when both source_type and search are None
        let _ = param_idx;

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let sql = format!("SELECT COUNT(*) FROM job{where_clause}");

        // AssertSqlSafe: only constant fragments and `$n` placeholders are interpolated; every value is bound.
        let mut query = sqlx::query_as::<_, (i64,)>(AssertSqlSafe(sql));
        if let Some(ws) = workspace {
            query = query.bind(ws);
        }
        if let Some(s) = status {
            query = query.bind(s);
        }
        if let Some(st) = source_type {
            query = query.bind(st);
        }
        if let Some(s) = search {
            query = query.bind(format!("%{}%", escape_like(s)));
        }

        let count = query
            .fetch_one(pool)
            .await
            .context("Failed to count jobs")?;
        Ok(count.0)
    }

    /// Count jobs by workspace + task name with optional status/source_type filter (mirrors `list_by_task()`)
    pub async fn count_by_task(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        status: Option<&str>,
        source_type: Option<&str>,
    ) -> Result<i64> {
        let mut conditions = vec!["workspace = $1".to_string(), "task_name = $2".to_string()];
        let mut param_idx = 3u32;
        if status.is_some() {
            conditions.push(format!("status = ${param_idx}"));
            param_idx += 1;
        }
        if source_type.is_some() {
            conditions.push(format!("source_type = ${param_idx}"));
            param_idx += 1;
        }
        let _ = param_idx;
        let sql = format!(
            "SELECT COUNT(*) FROM job WHERE {}",
            conditions.join(" AND ")
        );
        // AssertSqlSafe: only constant fragments and `$n` placeholders are interpolated; every value is bound.
        let mut query = sqlx::query_as::<_, (i64,)>(AssertSqlSafe(sql))
            .bind(workspace)
            .bind(task_name);
        if let Some(s) = status {
            query = query.bind(s);
        }
        if let Some(st) = source_type {
            query = query.bind(st);
        }
        let count = query
            .fetch_one(pool)
            .await
            .context("Failed to count jobs by task")?;
        Ok(count.0)
    }

    /// List jobs by workspace + task name with pagination and optional status/source_type filter
    pub async fn list_by_task(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        status: Option<&str>,
        source_type: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JobRow>> {
        let mut conditions = vec!["workspace = $1".to_string(), "task_name = $2".to_string()];
        let mut param_idx = 3u32;
        if status.is_some() {
            conditions.push(format!("status = ${param_idx}"));
            param_idx += 1;
        }
        if source_type.is_some() {
            conditions.push(format!("source_type = ${param_idx}"));
            param_idx += 1;
        }
        let limit_idx = param_idx;
        let offset_idx = param_idx + 1;
        let sql = format!(
            "SELECT {} FROM job WHERE {} ORDER BY created_at DESC LIMIT ${limit_idx} OFFSET ${offset_idx}",
            JOB_COLUMNS,
            conditions.join(" AND ")
        );
        // AssertSqlSafe: only constant fragments and `$n` placeholders are interpolated; every value is bound.
        let mut query = sqlx::query_as::<_, JobRow>(AssertSqlSafe(sql))
            .bind(workspace)
            .bind(task_name);
        if let Some(s) = status {
            query = query.bind(s);
        }
        if let Some(st) = source_type {
            query = query.bind(st);
        }
        let jobs = query
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await
            .context("Failed to list jobs by task")?;

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
        let jobs = sqlx::query_as::<_, JobRow>(concat!(
            "SELECT ",
            job_columns!(),
            " FROM job WHERE parent_job_id = $1 AND status IN ('pending', 'running')"
        ))
        .bind(parent_job_id)
        .fetch_all(pool)
        .await
        .context("Failed to get child jobs")?;

        Ok(jobs)
    }

    /// Every child job of `parent_job_id`, any status, newest first — the
    /// execution history of the parent's `type: task` steps. Ties on
    /// `created_at` are broken by id so the order is deterministic.
    pub async fn list_children(pool: &PgPool, parent_job_id: Uuid) -> Result<Vec<JobRow>> {
        let jobs = sqlx::query_as::<_, JobRow>(concat!(
            "SELECT ",
            job_columns!(),
            " FROM job WHERE parent_job_id = $1 ORDER BY created_at DESC, job_id DESC"
        ))
        .bind(parent_job_id)
        .fetch_all(pool)
        .await
        .context("Failed to list child jobs")?;
        Ok(jobs)
    }

    /// `type: task` **descendant** jobs that are terminal while the parent step
    /// that spawned them is still `running` — i.e. jobs that settled
    /// synchronously inside `create_job_for_task_inner` (all steps skipped,
    /// or a server-dispatched root step failed) and never went through
    /// terminal handling / parent propagation, because the creator has no
    /// `AppState` to run it with.
    ///
    /// Walks the whole `parent_job_id` chain below `root_job_id`, bounded at
    /// `MAX_TASK_DEPTH` levels, because a job that settles at creation can sit
    /// at any depth: with P → C → G, creating C creates G, G settles, and C is
    /// left `running` — so nothing but a descendant walk from P ever sees G.
    /// Rows come back **deepest first**, so the caller settles G (which
    /// propagates into C) before it reaches C itself.
    ///
    /// Scoped to `source_type = 'task'` deliberately: `agent_tool` children are
    /// propagated only by normal step completion (`propagate_to_parent`'s
    /// dedicated `agent_tool` branch), which intentionally leaves the parent
    /// agent step `running` across multiple tool calls — such a child would
    /// otherwise match this predicate permanently and be re-finalized
    /// (re-firing its hooks) on every unrelated sibling-step completion. An
    /// agent-tool child that would be born terminal is rejected at creation by
    /// the `agent_task_tool` endpoint instead.
    ///
    /// Execution quiescence is also required: a descendant with a `running` or
    /// `claimed` step of its own is still owned by a worker, so its terminal
    /// handling must not be consumed yet. A cancelled child in particular must
    /// keep its cancellation signal (and go on collecting log lines) until the
    /// worker acknowledges by settling the step — reconciling it early would
    /// clear the cancelled cache and upload the log archive while the worker
    /// is still writing.
    pub async fn get_settled_descendants_with_running_parent_step(
        pool: &PgPool,
        root_job_id: Uuid,
    ) -> Result<Vec<JobRow>> {
        let rows = sqlx::query_as::<_, JobRow>(concat!(
            "WITH RECURSIVE descendants AS ( \
                 SELECT j.*, 1 AS depth FROM job j WHERE j.parent_job_id = $1 \
                 UNION ALL \
                 SELECT c.*, d.depth + 1 FROM job c \
                 JOIN descendants d ON c.parent_job_id = d.job_id \
                 WHERE d.depth < $2 \
             ) \
             SELECT ",
            job_columns!(),
            " FROM descendants j \
             WHERE j.source_type = 'task' \
               AND j.status IN ('completed', 'failed', 'cancelled', 'skipped') \
               AND EXISTS ( \
                   SELECT 1 FROM job_step s \
                   WHERE s.job_id = j.parent_job_id \
                     AND s.step_name = j.parent_step_name \
                     AND s.status = 'running' \
               ) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM job_step ls \
                   WHERE ls.job_id = j.job_id \
                     AND ls.status IN ('running', 'claimed') \
               ) \
             ORDER BY j.depth DESC"
        ))
        .bind(root_job_id)
        .bind(MAX_TASK_DEPTH)
        .fetch_all(pool)
        .await
        .context("Failed to get settled descendants with running parent step")?;
        Ok(rows)
    }

    /// Every distinct pin referenced by `job_id`'s **redaction closure**
    /// (spec 2026-10-02 git refs § 7.4): the jobs whose content can be copied
    /// into this job's rows.
    ///
    /// The **lineage** is the job plus the jobs it was made from. From any job
    /// in it, the walk follows these edges:
    /// - up to its parent (`parent_job_id`);
    /// - from a `source_type = 'hook'` job, to the job that fired it:
    ///   `source_job_id`, or the UUID prefix of `source_id` on a hook row
    ///   written before migration 048 (the same fallback as the server's
    ///   hook-chain walk);
    /// - from a `source_type = 'restart'` or `'rerun'` job, to its source
    ///   (`source_job_id`). A re-run replays its source's `raw_input`, and a
    ///   task retry's `raw_input` is the failed job's RESOLVED input, so a
    ///   re-run can carry another commit's secrets (final review M2);
    /// - from a task retry, to the job it re-runs (`retry_of_job_id`).
    ///
    /// The closure is the **whole tree** of every lineage job: its root and
    /// all the root's descendants. That covers values copied up (a child's
    /// output into its parent step), down (a parent's values into a child's
    /// input), across (a sibling's output into another child's input), into
    /// a hook payload, into a restart's carried rows, into a re-run's replayed
    /// input, and into a task retry's replayed input.
    ///
    /// Over the closure, a pin is one of three kinds:
    /// - a job's own (`workspace`, `git_ref`, `revision`);
    /// - a step's action pin (`action_workspace`, defaulting to the job's
    ///   workspace, `action_ref`, `action_revision`);
    /// - a step's task pin (`task_workspace`, `task_ref`, `task_revision`).
    ///
    /// Rows are distinct and sorted.
    ///
    /// Every bound fails closed. An edge refused by a bound (a parent or a
    /// child at the depth limit, a hop past its cap) or more than
    /// `bounds.max_jobs` jobs answers [`RedactionClosure::Truncated`]. The
    /// recursion reads at most `max_jobs + 1` rows of each walk, so the cap
    /// also bounds the work.
    pub async fn redaction_closure_pins(
        pool: &PgPool,
        job_id: Uuid,
        bounds: ClosureBounds,
    ) -> Result<RedactionClosure> {
        let rows = sqlx::query_as::<_, (bool, Option<String>, Option<String>, Option<String>)>(
            r#"
            WITH RECURSIVE up(job_id, depth, hook_hops, source_hops, retry_hops, refused) AS (
                SELECT $1::uuid, 0, 0, 0, 0, false
                UNION ALL
                -- A refused edge is recorded (refused = true) and not walked.
                SELECT e.job_id, e.depth, e.hook_hops, e.source_hops, e.retry_hops, NOT e.ok
                  FROM up u
                  JOIN job j ON j.job_id = u.job_id
                 CROSS JOIN LATERAL (
                     VALUES
                         (j.parent_job_id, u.depth + 1,
                          u.hook_hops, u.source_hops, u.retry_hops,
                          u.depth < $2),
                         (CASE j.source_type
                              WHEN 'hook' THEN COALESCE(
                                  j.source_job_id,
                                  CASE WHEN split_part(j.source_id, '/', 1)
                                            ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                                       THEN split_part(j.source_id, '/', 1)::uuid
                                  END)
                              WHEN 'restart' THEN j.source_job_id
                              WHEN 'rerun' THEN j.source_job_id
                          END,
                          0,
                          u.hook_hops + (j.source_type = 'hook')::int,
                          u.source_hops + (j.source_type IN ('restart', 'rerun'))::int,
                          u.retry_hops,
                          CASE j.source_type
                              WHEN 'hook' THEN u.hook_hops < $3
                              ELSE u.source_hops < $4
                          END),
                         (j.retry_of_job_id, 0,
                          u.hook_hops, u.source_hops, u.retry_hops + 1,
                          u.retry_hops < $5)
                 ) AS e(job_id, depth, hook_hops, source_hops, retry_hops, ok)
                 WHERE NOT u.refused AND e.job_id IS NOT NULL
            ),
            up_capped AS (SELECT job_id, refused FROM up LIMIT $6 + 1),
            tree(job_id, depth, refused) AS (
                SELECT DISTINCT u.job_id, 0, false
                  FROM up_capped u
                  JOIN job j ON j.job_id = u.job_id
                 WHERE NOT u.refused AND j.parent_job_id IS NULL
                UNION ALL
                SELECT child.job_id, t.depth + 1, t.depth >= $2
                  FROM tree t
                  JOIN job child ON child.parent_job_id = t.job_id
                 WHERE NOT t.refused
            ),
            tree_capped AS (SELECT job_id, refused FROM tree LIMIT $6 + 1),
            closure AS (
                SELECT job_id FROM up_capped WHERE NOT refused
                UNION
                SELECT job_id FROM tree_capped WHERE NOT refused
            ),
            flags AS (
                SELECT EXISTS (SELECT 1 FROM up_capped WHERE refused)
                    OR EXISTS (SELECT 1 FROM tree_capped WHERE refused)
                    OR (SELECT count(*) FROM up_capped) > $6
                    OR (SELECT count(*) FROM tree_capped) > $6 AS truncated
            ),
            pins AS (
                SELECT DISTINCT workspace, git_ref, revision FROM (
                    SELECT j.workspace, j.git_ref, j.revision
                      FROM job j
                      JOIN closure cl ON cl.job_id = j.job_id
                     WHERE j.git_ref IS NOT NULL AND j.revision IS NOT NULL
                    UNION ALL
                    SELECT COALESCE(s.action_workspace, j.workspace), s.action_ref, s.action_revision
                      FROM job_step s
                      JOIN closure cl ON cl.job_id = s.job_id
                      JOIN job j ON j.job_id = s.job_id
                     WHERE s.action_ref IS NOT NULL AND s.action_revision IS NOT NULL
                    UNION ALL
                    SELECT s.task_workspace, s.task_ref, s.task_revision
                      FROM job_step s
                      JOIN closure cl ON cl.job_id = s.job_id
                     WHERE s.task_workspace IS NOT NULL
                       AND s.task_ref IS NOT NULL
                       AND s.task_revision IS NOT NULL
                ) p
            )
            SELECT f.truncated, p.workspace, p.git_ref, p.revision
              FROM flags f
              LEFT JOIN pins p ON NOT f.truncated
             ORDER BY p.workspace, p.revision, p.git_ref
            "#,
        )
        .bind(job_id)
        .bind(bounds.task_depth)
        .bind(bounds.hook_hops)
        .bind(bounds.source_hops)
        .bind(bounds.retry_hops)
        .bind(bounds.max_jobs)
        .fetch_all(pool)
        .await
        .context("Failed to read the job's redaction closure pins")?;

        if rows.first().is_none_or(|r| r.0) {
            return Ok(RedactionClosure::Truncated);
        }
        Ok(RedactionClosure::Pins(
            rows.into_iter()
                .filter_map(|(_, workspace, git_ref, revision)| {
                    Some(ClosurePinRow {
                        workspace: workspace?,
                        git_ref: git_ref?,
                        revision: revision?,
                    })
                })
                .collect(),
        ))
    }

    /// Whether ANY row references a pin: a pinned job (`git_ref`), or a step
    /// with an action or task pin. With none, every redaction closure's pin
    /// set is empty, so the redaction set is the live values alone. Two
    /// `EXISTS` probes on the partial indexes `idx_job_pinned_tasks` and
    /// `idx_job_step_pinned`.
    pub async fn any_pinned_rows(pool: &PgPool) -> Result<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM job WHERE git_ref IS NOT NULL) \
                 OR EXISTS (SELECT 1 FROM job_step \
                             WHERE action_ref IS NOT NULL OR task_ref IS NOT NULL)",
        )
        .fetch_one(pool)
        .await
        .context("Failed to check for pinned rows")
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

    /// Return IDs of pending/running jobs whose `timeout_secs` deadline has elapsed.
    pub async fn get_timed_out_jobs(pool: &PgPool) -> Result<Vec<Uuid>> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            "SELECT job_id FROM job \
             WHERE status IN ('pending', 'running') \
               AND timeout_secs IS NOT NULL \
               AND created_at + make_interval(secs => timeout_secs::double precision) < NOW()",
        )
        .fetch_all(pool)
        .await
        .context("get_timed_out_jobs")?;
        Ok(rows)
    }

    /// Non-terminal pinned jobs with no live step (git-refs spec § 7.3, R7).
    /// A job lands here when an `advance` could not load its pin
    /// (`PinUnavailable` on a cold replica during a git outage) after the
    /// step that triggered it went terminal — nothing is left to re-enter it.
    /// A `type: task` step waiting on a child is `running` and an approval is
    /// `suspended`, so neither is listed; `claimed` counts as live like in
    /// `has_live_steps`.
    ///
    /// A job is still `pending` until a worker calls `/start`, so a step that
    /// failed at claim (the release cap, a permanent pin or render error) or
    /// whose tarball download was exhausted leaves it `pending`. Such a job
    /// is listed only once it has a TERMINAL step that is not carried over:
    /// a `pending` job with none is one whose creation-time init has not
    /// promoted its first steps yet, and is left to that init. This narrows
    /// the overlap with init but does not exclude it: a ready-at-creation root
    /// step that fails at claim, or an init dispatch failure, can make a job
    /// listable while its init still runs — the same race class as the
    /// failure's own `advance`. A restart's carried-over rows are terminal
    /// from creation on, so they do not count.
    ///
    /// One page of at most `limit` rows, in `job_id` order starting AFTER
    /// `after` and wrapping around to the lowest ids (recovery keeps the last
    /// visited id as a cursor, so a bounded sweep resumes where the previous
    /// one stopped and every stalled job is reached in turn).
    pub async fn get_stalled_pinned_jobs(
        pool: &PgPool,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<StalledPinnedJob>> {
        let rows = sqlx::query_as::<_, StalledPinnedJob>(
            r#"
            SELECT j.job_id, j.workspace, j.revision
            FROM job j
            WHERE j.status IN ('pending', 'running')
              AND j.git_ref IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1 FROM job_step s
                  WHERE s.job_id = j.job_id
                    AND s.status IN ('ready', 'claimed', 'running', 'suspended')
              )
              AND (
                  j.status = 'running'
                  OR EXISTS (
                      SELECT 1 FROM job_step s
                      WHERE s.job_id = j.job_id
                        AND s.status IN ('completed', 'failed', 'skipped', 'cancelled')
                        AND NOT s.carried_over
                  )
              )
            ORDER BY ($1::uuid IS NOT NULL AND j.job_id <= $1), j.job_id
            LIMIT $2
            "#,
        )
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("Failed to list stalled pinned jobs")?;
        Ok(rows)
    }

    /// Return up to `batch_size` terminal jobs that FINISHED more than the given
    /// number of days ago (`completed_at`, falling back to `created_at` for a
    /// terminal row without one).
    ///
    /// Counting from creation let the sweep delete a long-running job the moment
    /// it turned terminal, while its hooks, task retry and log archive were still
    /// being written — and a hook job's `source_job_id` / a retry's
    /// `retry_of_job_id` FK then rejected the insert. The redundant `created_at`
    /// bound keeps the scan on its index (`completed_at >= created_at`).
    ///
    /// Only considers jobs with status `completed`, `failed`, `cancelled` or `skipped`.
    /// Callers should loop until an empty result is returned to process all matching rows.
    pub async fn get_old_terminal_jobs(
        pool: &PgPool,
        retention_days: f64,
        batch_size: i64,
    ) -> Result<Vec<RetentionJobInfo>> {
        let rows = sqlx::query_as::<_, RetentionJobInfo>(
            "SELECT job_id, workspace, task_name, created_at FROM job \
             WHERE status IN ('completed', 'failed', 'cancelled', 'skipped') \
               AND created_at < NOW() - make_interval(secs => $1::double precision) \
               AND COALESCE(completed_at, created_at) < NOW() - make_interval(secs => $1::double precision) \
             LIMIT $2",
        )
        .bind(retention_days * 86400.0)
        .bind(batch_size)
        .fetch_all(pool)
        .await
        .context("get_old_terminal_jobs")?;
        Ok(rows)
    }

    /// Delete a job by ID. Steps are cascade-deleted via FK constraint.
    pub async fn delete(pool: &PgPool, job_id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM job WHERE job_id = $1")
            .bind(job_id)
            .execute(pool)
            .await
            .context("Failed to delete job")?;
        Ok(())
    }

    /// Count pending/running jobs matching the given `source_type` and `source_id`.
    ///
    /// Used to enforce cron concurrency limits before creating a new job.
    pub async fn count_active_by_source(
        pool: &PgPool,
        source_type: &str,
        source_id: &str,
    ) -> Result<i64> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM job \
             WHERE source_type = $1 AND source_id = $2 \
               AND status IN ('pending', 'running')",
        )
        .bind(source_type)
        .bind(source_id)
        .fetch_one(pool)
        .await
        .context("count_active_by_source")?;
        Ok(count)
    }

    /// Return IDs of pending/running jobs matching the given `source_type` and `source_id`.
    pub async fn get_active_job_ids_by_source(
        pool: &PgPool,
        source_type: &str,
        source_id: &str,
    ) -> Result<Vec<Uuid>> {
        let rows = sqlx::query_scalar::<_, Uuid>(
            "SELECT job_id FROM job \
             WHERE source_type = $1 AND source_id = $2 \
               AND status IN ('pending', 'running')",
        )
        .bind(source_type)
        .bind(source_id)
        .fetch_all(pool)
        .await
        .context("get_active_job_ids_by_source")?;
        Ok(rows)
    }

    /// Return all pending/running jobs matching the given `source_type` and `source_id`.
    pub async fn get_active_by_source(
        pool: &PgPool,
        source_type: &str,
        source_id: &str,
    ) -> Result<Vec<JobRow>> {
        let sql = concat!(
            "SELECT ",
            job_columns!(),
            " FROM job \
             WHERE source_type = $1 AND source_id = $2 \
               AND status IN ('pending', 'running')"
        );
        let rows = sqlx::query_as::<_, JobRow>(sql)
            .bind(source_type)
            .bind(source_id)
            .fetch_all(pool)
            .await
            .context("get_active_by_source")?;
        Ok(rows)
    }

    /// Distinct `(workspace, task_name, task_folder)` of pinned jobs — the
    /// input from which the server builds [`JobAclScope::pinned_triples`].
    /// Served by the partial index `idx_job_pinned_tasks`.
    pub async fn pinned_task_triples(
        pool: &PgPool,
    ) -> Result<Vec<(String, String, Option<String>)>> {
        sqlx::query_as(
            "SELECT DISTINCT workspace, task_name, task_folder FROM job WHERE git_ref IS NOT NULL",
        )
        .fetch_all(pool)
        .await
        .context("Failed to list pinned task triples")
    }

    /// List jobs the scope allows (spec 2026-10-02 § 7.8), newest first.
    /// Returns an empty vec immediately for an empty scope.
    pub async fn list_with_acl(
        pool: &PgPool,
        scope: &JobAclScope,
        status: Option<&str>,
        source_type: Option<&str>,
        search: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JobRow>> {
        if scope.is_empty() {
            return Ok(vec![]);
        }
        let (acl, next) = scope.predicate(1);
        let (filters, next) = Self::list_filters(next, status, source_type, search);
        let sql = format!(
            "SELECT {JOB_COLUMNS} FROM job WHERE {acl}{filters} \
             ORDER BY created_at DESC LIMIT ${next} OFFSET ${}",
            next + 1
        );
        // AssertSqlSafe: only constant fragments and `$n` placeholders are interpolated; every value is bound.
        let mut query = sqlx::query_as::<_, JobRow>(AssertSqlSafe(sql));
        for v in scope.bind_values() {
            query = query.bind(v);
        }
        if let Some(s) = status {
            query = query.bind(s);
        }
        if let Some(st) = source_type {
            query = query.bind(st);
        }
        if let Some(s) = search {
            query = query.bind(format!("%{}%", escape_like(s)));
        }
        query
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await
            .context("Failed to list jobs with ACL")
    }

    /// Count jobs the scope allows. Returns 0 immediately for an empty scope.
    pub async fn count_with_acl(
        pool: &PgPool,
        scope: &JobAclScope,
        status: Option<&str>,
        source_type: Option<&str>,
        search: Option<&str>,
    ) -> Result<i64> {
        if scope.is_empty() {
            return Ok(0);
        }
        let (acl, next) = scope.predicate(1);
        let (filters, _) = Self::list_filters(next, status, source_type, search);
        let sql = format!("SELECT COUNT(*) FROM job WHERE {acl}{filters}");
        // AssertSqlSafe: only constant fragments and `$n` placeholders are interpolated; every value is bound.
        let mut query = sqlx::query_as::<_, (i64,)>(AssertSqlSafe(sql));
        for v in scope.bind_values() {
            query = query.bind(v);
        }
        if let Some(s) = status {
            query = query.bind(s);
        }
        if let Some(st) = source_type {
            query = query.bind(st);
        }
        if let Some(s) = search {
            query = query.bind(format!("%{}%", escape_like(s)));
        }
        let count = query
            .fetch_one(pool)
            .await
            .context("Failed to count jobs with ACL")?;
        Ok(count.0)
    }

    /// Status counts of the jobs the scope allows. Empty map for an empty scope.
    pub async fn get_status_counts_with_acl(
        pool: &PgPool,
        scope: &JobAclScope,
    ) -> Result<HashMap<String, i64>> {
        if scope.is_empty() {
            return Ok(HashMap::new());
        }
        let (acl, _) = scope.predicate(1);
        let sql = format!("SELECT status, COUNT(*) FROM job WHERE {acl} GROUP BY status");
        // AssertSqlSafe: only constant fragments and `$n` placeholders are interpolated; every value is bound.
        let mut query = sqlx::query_as::<_, (String, i64)>(AssertSqlSafe(sql));
        for v in scope.bind_values() {
            query = query.bind(v);
        }
        let rows = query
            .fetch_all(pool)
            .await
            .context("Failed to get status counts with ACL")?;
        Ok(rows.into_iter().collect())
    }

    /// ` AND …` for the optional list filters, numbered from `first`; returns
    /// the clause and the next free placeholder index. Bind order: status,
    /// source_type, search.
    fn list_filters(
        first: u32,
        status: Option<&str>,
        source_type: Option<&str>,
        search: Option<&str>,
    ) -> (String, u32) {
        let mut idx = first;
        let mut sql = String::new();
        if status.is_some() {
            sql.push_str(&format!(" AND status = ${idx}"));
            idx += 1;
        }
        if source_type.is_some() {
            sql.push_str(&format!(" AND source_type = ${idx}"));
            idx += 1;
        }
        if search.is_some() {
            sql.push_str(&format!(
                " AND (task_name ILIKE ${idx} OR workspace ILIKE ${idx} OR source_id ILIKE ${idx} OR job_id::text ILIKE ${idx})"
            ));
            idx += 1;
        }
        (sql, idx)
    }

    /// Aggregate duration statistics over the last `limit` *completed* runs of a task.
    ///
    /// Only includes jobs with `status = 'completed'` and non-NULL `started_at` /
    /// `completed_at`. Excludes `source_type = 'restart'` jobs (spec §6.4) —
    /// restart jobs re-run only a suffix of the flow, so their duration is not
    /// comparable to a full run and would skew percentiles. Also excludes pinned
    /// jobs (`git_ref IS NOT NULL`, spec 2026-10-02 § 7.8): stats describe the live
    /// task, and a release's runs, with their own flows, are not its runs. Returns zero-sample
    /// row (all fields `None`) when no runs match — never returns `Err` for
    /// "no data".
    pub async fn get_task_duration_stats(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        limit: i64,
    ) -> Result<DurationStatsRow> {
        // NOTE: `EXTRACT(EPOCH FROM ...)` returns `numeric` in PostgreSQL 14+
        // (it was `double precision` in older versions). sqlx maps the row to
        // `Option<f64>`, so we explicitly cast each percentile expression to
        // `double precision` to keep the decode happy across PG versions.
        let row = sqlx::query_as::<_, DurationStatsRow>(
            "SELECT \
               COUNT(*)::BIGINT AS sample_size, \
               (EXTRACT(EPOCH FROM AVG(d)) * 1000.0)::double precision AS avg_ms, \
               (EXTRACT(EPOCH FROM percentile_cont(0.5) WITHIN GROUP (ORDER BY d)) * 1000.0)::double precision AS p50_ms, \
               (EXTRACT(EPOCH FROM percentile_cont(0.95) WITHIN GROUP (ORDER BY d)) * 1000.0)::double precision AS p95_ms, \
               (EXTRACT(EPOCH FROM MIN(d)) * 1000.0)::double precision AS min_ms, \
               (EXTRACT(EPOCH FROM MAX(d)) * 1000.0)::double precision AS max_ms \
             FROM ( \
               SELECT (completed_at - started_at) AS d \
               FROM job \
               WHERE workspace = $1 AND task_name = $2 \
                 AND status = 'completed' \
                 AND source_type <> 'restart' \
                 AND git_ref IS NULL \
                 AND started_at IS NOT NULL AND completed_at IS NOT NULL \
                 AND completed_at >= started_at \
               ORDER BY completed_at DESC, job_id \
               LIMIT $3 \
             ) recent",
        )
        .bind(workspace)
        .bind(task_name)
        .bind(limit)
        .fetch_one(pool)
        .await
        .context("get_task_duration_stats")?;
        Ok(row)
    }

    /// Most recent N completed-run durations, newest-first.
    ///
    /// Companion to [`get_task_duration_stats`] for sparkline rendering. The
    /// caller typically reverses the slice so the sparkline reads oldest→newest
    /// left-to-right. Excludes pinned jobs (`git_ref IS NOT NULL`, spec 2026-10-02 § 7.8).
    pub async fn get_recent_durations(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        limit: i64,
    ) -> Result<Vec<RecentDurationRow>> {
        // See cast note in `get_task_duration_stats` above.
        let rows = sqlx::query_as::<_, RecentDurationRow>(
            "SELECT job_id, \
                    (EXTRACT(EPOCH FROM (completed_at - started_at)) * 1000.0)::double precision AS duration_ms, \
                    completed_at \
             FROM job \
             WHERE workspace = $1 AND task_name = $2 \
               AND status = 'completed' \
               AND source_type <> 'restart' \
               AND git_ref IS NULL \
               AND started_at IS NOT NULL AND completed_at IS NOT NULL \
               AND completed_at >= started_at \
             ORDER BY completed_at DESC, job_id \
             LIMIT $3",
        )
        .bind(workspace)
        .bind(task_name)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("get_recent_durations")?;
        Ok(rows)
    }
}
