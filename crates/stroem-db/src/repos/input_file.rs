use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

pub struct NewInputFile<'a> {
    pub file_id: &'a str,
    pub workspace: &'a str,
    pub task_name: &'a str,
    pub uploaded_by_user_id: Option<Uuid>,
    pub origin: &'a str,
    pub original_name: &'a str,
    pub safe_name: &'a str,
    pub content_type: &'a str,
    pub size_bytes: i64,
    pub sha256: &'a str,
    pub storage_key: &'a str,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InputFileRow {
    pub file_id: String,
    pub workspace: String,
    pub task_name: String,
    pub uploaded_by_user_id: Option<Uuid>,
    pub origin: String,
    pub original_name: String,
    pub safe_name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub storage_key: String,
    pub created_at: DateTime<Utc>,
    pub deleting_at: Option<DateTime<Utc>>,
}

macro_rules! input_file_columns {
    () => {
        "file_id, workspace, task_name, uploaded_by_user_id, origin, original_name, safe_name, \
         content_type, size_bytes, sha256, storage_key, created_at, deleting_at"
    };
}

pub struct InputFileRepo;

impl InputFileRepo {
    pub async fn insert(pool: &PgPool, f: &NewInputFile<'_>) -> Result<()> {
        sqlx::query(
            "INSERT INTO input_file (file_id, workspace, task_name, uploaded_by_user_id, origin, \
             original_name, safe_name, content_type, size_bytes, sha256, storage_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(f.file_id)
        .bind(f.workspace)
        .bind(f.task_name)
        .bind(f.uploaded_by_user_id)
        .bind(f.origin)
        .bind(f.original_name)
        .bind(f.safe_name)
        .bind(f.content_type)
        .bind(f.size_bytes)
        .bind(f.sha256)
        .bind(f.storage_key)
        .execute(pool)
        .await
        .context("insert input_file")?;
        Ok(())
    }

    pub async fn get(pool: &PgPool, file_id: &str) -> Result<Option<InputFileRow>> {
        sqlx::query_as(concat!(
            "SELECT ",
            input_file_columns!(),
            " FROM input_file WHERE file_id = $1"
        ))
        .bind(file_id)
        .fetch_optional(pool)
        .await
        .context("get input_file")
    }

    /// Files the sweep should examine: already marked `deleting_at`, or
    /// unattached and older than `ttl_secs`. Oldest first.
    pub async fn sweep_candidates(pool: &PgPool, ttl_secs: i64, limit: i64) -> Result<Vec<String>> {
        sqlx::query_scalar(
            "SELECT f.file_id FROM input_file f \
             WHERE f.deleting_at IS NOT NULL \
                OR (NOT EXISTS (SELECT 1 FROM job_input_file j WHERE j.file_id = f.file_id) \
                    AND f.created_at < NOW() - make_interval(secs => $1)) \
             ORDER BY f.created_at, f.file_id LIMIT $2",
        )
        .bind(ttl_secs as f64)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("input_file sweep_candidates")
    }

    /// Spec § 4.7 step 1, on its own transaction.
    pub async fn mark_deleting(pool: &PgPool, file_id: &str) -> Result<Option<InputFileRow>> {
        let mut tx = pool.begin().await.context("begin mark_deleting")?;
        let row = Self::mark_deleting_tx(&mut tx, file_id).await?;
        tx.commit().await.context("commit mark_deleting")?;
        Ok(row)
    }

    /// Lock the file row `FOR UPDATE`, then (a separate statement, so it sees
    /// attachments committed while the lock was awaited) refuse when an
    /// attachment exists; otherwise set `deleting_at`. `None` = absent or
    /// attached.
    #[doc(hidden)]
    pub async fn mark_deleting_tx(
        tx: &mut sqlx::PgConnection,
        file_id: &str,
    ) -> Result<Option<InputFileRow>> {
        let locked: Option<String> =
            sqlx::query_scalar("SELECT file_id FROM input_file WHERE file_id = $1 FOR UPDATE")
                .bind(file_id)
                .fetch_optional(&mut *tx)
                .await
                .context("lock input_file")?;
        if locked.is_none() {
            return Ok(None);
        }
        let attached: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM job_input_file WHERE file_id = $1)")
                .bind(file_id)
                .fetch_one(&mut *tx)
                .await
                .context("check input_file attachments")?;
        if attached {
            return Ok(None);
        }
        sqlx::query_as(concat!(
            "UPDATE input_file SET deleting_at = COALESCE(deleting_at, NOW()) \
             WHERE file_id = $1 RETURNING ",
            input_file_columns!()
        ))
        .bind(file_id)
        .fetch_optional(&mut *tx)
        .await
        .context("mark input_file deleting")
    }

    /// Delete a row marked `deleting_at` with no attachment.
    pub async fn delete(pool: &PgPool, file_id: &str) -> Result<bool> {
        let r = sqlx::query(
            "DELETE FROM input_file WHERE file_id = $1 AND deleting_at IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM job_input_file WHERE file_id = $1)",
        )
        .bind(file_id)
        .execute(pool)
        .await
        .context("delete input_file")?;
        Ok(r.rows_affected() > 0)
    }
}

pub enum AttachRule<'a> {
    Own {
        workspace: &'a str,
        task_name: &'a str,
        user_id: Option<Uuid>,
        ttl_secs: i64,
    },
    Lineage {
        job_id: Uuid,
    },
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobInputFileRow {
    pub job_id: Uuid,
    pub field: String,
    pub file_id: String,
    pub original_name: String,
    pub safe_name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub storage_key: String,
}

macro_rules! job_input_file_select {
    () => {
        "SELECT j.job_id, j.field, j.file_id, f.original_name, f.safe_name, f.content_type, \
         f.size_bytes, f.sha256, f.storage_key \
         FROM job_input_file j JOIN input_file f ON f.file_id = j.file_id "
    };
}

pub struct JobInputFileRepo;

impl JobInputFileRepo {
    /// Lock the file row `FOR SHARE`; refuse a missing or `deleting_at` file;
    /// attach when any rule matches (first match wins). `Ok(false)` = refused.
    pub async fn attach_tx(
        tx: &mut sqlx::PgConnection,
        job_id: Uuid,
        field: &str,
        file_id: &str,
        rules: &[AttachRule<'_>],
    ) -> Result<bool> {
        let live: Option<bool> = sqlx::query_scalar(
            "SELECT deleting_at IS NULL FROM input_file WHERE file_id = $1 FOR SHARE",
        )
        .bind(file_id)
        .fetch_optional(&mut *tx)
        .await
        .context("lock input_file for attach")?;
        if live != Some(true) {
            return Ok(false);
        }
        let mut matched = false;
        for rule in rules {
            matched = match rule {
                AttachRule::Own {
                    workspace,
                    task_name,
                    user_id,
                    ttl_secs,
                } => sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM input_file WHERE file_id = $1 \
                     AND workspace = $2 AND task_name = $3 \
                     AND uploaded_by_user_id IS NOT DISTINCT FROM $4 \
                     AND created_at > NOW() - make_interval(secs => $5))",
                )
                .bind(file_id)
                .bind(workspace)
                .bind(task_name)
                .bind(user_id)
                .bind(*ttl_secs as f64)
                .fetch_one(&mut *tx)
                .await
                .context("check own-upload attach rule")?,
                AttachRule::Lineage { job_id: source } => sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM job_input_file WHERE job_id = $1 AND file_id = $2)",
                )
                .bind(source)
                .bind(file_id)
                .fetch_one(&mut *tx)
                .await
                .context("check lineage attach rule")?,
            };
            if matched {
                break;
            }
        }
        if !matched {
            return Ok(false);
        }
        sqlx::query("INSERT INTO job_input_file (job_id, field, file_id) VALUES ($1, $2, $3)")
            .bind(job_id)
            .bind(field)
            .bind(file_id)
            .execute(&mut *tx)
            .await
            .context("insert job_input_file")?;
        Ok(true)
    }

    pub async fn list_for_job(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobInputFileRow>> {
        sqlx::query_as(concat!(
            job_input_file_select!(),
            "WHERE j.job_id = $1 ORDER BY j.field"
        ))
        .bind(job_id)
        .fetch_all(pool)
        .await
        .context("list job_input_file")
    }

    pub async fn get_for_field(
        pool: &PgPool,
        job_id: Uuid,
        field: &str,
    ) -> Result<Option<JobInputFileRow>> {
        sqlx::query_as(concat!(
            job_input_file_select!(),
            "WHERE j.job_id = $1 AND j.field = $2"
        ))
        .bind(job_id)
        .bind(field)
        .fetch_optional(pool)
        .await
        .context("get job_input_file by field")
    }

    pub async fn get_attached(
        pool: &PgPool,
        job_id: Uuid,
        file_id: &str,
    ) -> Result<Option<JobInputFileRow>> {
        sqlx::query_as(concat!(
            job_input_file_select!(),
            "WHERE j.job_id = $1 AND j.file_id = $2 ORDER BY j.field LIMIT 1"
        ))
        .bind(job_id)
        .bind(file_id)
        .fetch_optional(pool)
        .await
        .context("get job_input_file by file")
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UploadTokenRow {
    pub user_id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub original_name: String,
}

pub struct UploadTokenRepo;

impl UploadTokenRepo {
    pub async fn insert(
        pool: &PgPool,
        token_sha256: &str,
        user_id: Uuid,
        workspace: &str,
        task_name: &str,
        original_name: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO input_file_upload_token \
             (token_sha256, user_id, workspace, task_name, original_name, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(token_sha256)
        .bind(user_id)
        .bind(workspace)
        .bind(task_name)
        .bind(original_name)
        .bind(expires_at)
        .execute(pool)
        .await
        .context("insert upload token")?;
        Ok(())
    }

    /// Single use: marks the token used and returns it, or `None` when it is
    /// unknown, used or expired.
    pub async fn consume(pool: &PgPool, token_sha256: &str) -> Result<Option<UploadTokenRow>> {
        sqlx::query_as(
            "UPDATE input_file_upload_token SET used_at = NOW() \
             WHERE token_sha256 = $1 AND used_at IS NULL AND expires_at > NOW() \
             RETURNING user_id, workspace, task_name, original_name",
        )
        .bind(token_sha256)
        .fetch_optional(pool)
        .await
        .context("consume upload token")
    }

    pub async fn delete_expired(pool: &PgPool) -> Result<u64> {
        let r = sqlx::query("DELETE FROM input_file_upload_token WHERE expires_at <= NOW()")
            .execute(pool)
            .await
            .context("delete expired upload tokens")?;
        Ok(r.rows_affected())
    }
}
