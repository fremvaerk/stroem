use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskStateRow {
    pub id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub job_id: Option<Uuid>,
    pub storage_key: String,
    pub size_bytes: i64,
    pub has_json: bool,
    pub state_json: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    /// The partition (spec 2026-10-02 § 7.6): the pinned job's ref string, or
    /// `None` for unpinned jobs and manual uploads.
    pub git_ref: Option<String>,
}

const INSERT_SQL: &str = "INSERT INTO task_state (id, workspace, task_name, job_id, storage_key, size_bytes, has_json, state_json, git_ref) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8::json, $9)";

/// The [`TaskStateRow`] column list. A macro rather than a `const` so a query
/// can `concat!` it into a `&'static str`, which sqlx accepts as SQL directly.
macro_rules! columns {
    () => {
        "id, workspace, task_name, job_id, storage_key, size_bytes, has_json, state_json, created_at, git_ref"
    };
}

/// Latest snapshot of a partition: `git_ref = $3`, or `git_ref IS NULL` for the
/// unpinned partition. Two shapes rather than `IS NOT DISTINCT FROM`, so the
/// btree index applies.
fn latest_sql(git_ref: Option<&str>) -> &'static str {
    match git_ref {
        Some(_) => concat!(
            "SELECT ",
            columns!(),
            " FROM task_state \
             WHERE workspace = $1 AND task_name = $2 AND git_ref = $3 \
             ORDER BY created_at DESC, id DESC LIMIT 1"
        ),
        None => concat!(
            "SELECT ",
            columns!(),
            " FROM task_state \
             WHERE workspace = $1 AND task_name = $2 AND git_ref IS NULL \
             ORDER BY created_at DESC, id DESC LIMIT 1"
        ),
    }
}

pub struct TaskStateRepo;

impl TaskStateRepo {
    /// Latest snapshot of the unpinned partition. See [`Self::get_latest_for_ref`].
    pub async fn get_latest(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
    ) -> Result<Option<TaskStateRow>> {
        Self::get_latest_for_ref(pool, workspace, task_name, None).await
    }

    /// Latest snapshot of one `(workspace, task, git_ref)` partition.
    pub async fn get_latest_for_ref(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        git_ref: Option<&str>,
    ) -> Result<Option<TaskStateRow>> {
        let mut q = sqlx::query_as::<_, TaskStateRow>(latest_sql(git_ref))
            .bind(workspace)
            .bind(task_name);
        if let Some(r) = git_ref {
            q = q.bind(r);
        }
        q.fetch_optional(pool)
            .await
            .context("Failed to get latest task state snapshot")
    }

    /// Get a specific snapshot by ID.
    pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<TaskStateRow>> {
        sqlx::query_as::<_, TaskStateRow>(concat!(
            "SELECT ",
            columns!(),
            " FROM task_state WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("Failed to get task state snapshot")
    }

    /// Insert into the unpinned partition. See [`Self::insert_for_ref`].
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
        state_json: Option<&serde_json::Value>,
    ) -> Result<Uuid> {
        Self::insert_for_ref(
            pool,
            workspace,
            task_name,
            None,
            job_id,
            storage_key,
            size_bytes,
            has_json,
            state_json,
        )
        .await
    }

    /// Insert a new snapshot record into `git_ref`'s partition. Returns the ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_for_ref(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        git_ref: Option<&str>,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
        state_json: Option<&serde_json::Value>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let state_json_text = state_json.map(|v| v.to_string());
        sqlx::query(INSERT_SQL)
            .bind(id)
            .bind(workspace)
            .bind(task_name)
            .bind(job_id)
            .bind(storage_key)
            .bind(size_bytes)
            .bind(has_json)
            .bind(state_json_text)
            .bind(git_ref)
            .execute(pool)
            .await
            .context("Failed to insert task state snapshot")?;
        Ok(id)
    }

    /// [`Self::insert_and_prune_for_ref`] on the unpinned partition.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_and_prune<'a>(
        tx: &mut Tx<'a>,
        workspace: &str,
        task_name: &str,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
        state_json: Option<&serde_json::Value>,
        keep: usize,
        snapshot_id: Option<Uuid>,
    ) -> Result<(Uuid, Vec<String>)> {
        Self::insert_and_prune_for_ref(
            tx,
            workspace,
            task_name,
            None,
            job_id,
            storage_key,
            size_bytes,
            has_json,
            state_json,
            keep,
            snapshot_id,
        )
        .await
    }

    /// Insert a new snapshot into `git_ref`'s partition and prune that
    /// partition to `keep`, both against the caller's transaction. Other
    /// partitions are never touched (spec 2026-10-02 § 7.6).
    ///
    /// `snapshot_id`: pass `Some(id)` to use a pre-generated UUID. Returns the
    /// snapshot UUID and the storage keys of any pruned rows.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_and_prune_for_ref<'a>(
        tx: &mut Tx<'a>,
        workspace: &str,
        task_name: &str,
        git_ref: Option<&str>,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
        state_json: Option<&serde_json::Value>,
        keep: usize,
        snapshot_id: Option<Uuid>,
    ) -> Result<(Uuid, Vec<String>)> {
        let id = snapshot_id.unwrap_or_else(Uuid::new_v4);
        let state_json_text = state_json.map(|v| v.to_string());

        sqlx::query(INSERT_SQL)
            .bind(id)
            .bind(workspace)
            .bind(task_name)
            .bind(job_id)
            .bind(storage_key)
            .bind(size_bytes)
            .bind(has_json)
            .bind(state_json_text)
            .bind(git_ref)
            .execute(&mut **tx)
            .await
            .context("Failed to insert task state snapshot")?;

        let mut q = sqlx::query_scalar::<_, String>(prune_sql(git_ref))
            .bind(workspace)
            .bind(task_name)
            .bind(keep as i64);
        if let Some(r) = git_ref {
            q = q.bind(r);
        }
        let deleted_keys = q
            .fetch_all(&mut **tx)
            .await
            .context("Failed to prune task state snapshots")?;
        Ok((id, deleted_keys))
    }

    /// List every snapshot of a workspace+task, all partitions, newest first.
    pub async fn list(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
    ) -> Result<Vec<TaskStateRow>> {
        sqlx::query_as::<_, TaskStateRow>(concat!(
            "SELECT ",
            columns!(),
            " FROM task_state \
             WHERE workspace = $1 AND task_name = $2 \
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(workspace)
        .bind(task_name)
        .fetch_all(pool)
        .await
        .context("Failed to list task state snapshots")
    }

    /// [`Self::prune_for_ref`] on the unpinned partition.
    pub async fn prune(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        keep: usize,
    ) -> Result<Vec<String>> {
        Self::prune_for_ref(pool, workspace, task_name, None, keep).await
    }

    /// Keep the `keep` newest snapshots of one partition; returns the deleted
    /// storage keys so the caller can remove them from the archive.
    pub async fn prune_for_ref(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
        git_ref: Option<&str>,
        keep: usize,
    ) -> Result<Vec<String>> {
        let mut q = sqlx::query_scalar::<_, String>(prune_sql(git_ref))
            .bind(workspace)
            .bind(task_name)
            .bind(keep as i64);
        if let Some(r) = git_ref {
            q = q.bind(r);
        }
        q.fetch_all(pool)
            .await
            .context("Failed to prune task state snapshots")
    }

    /// Delete all snapshots for a task.
    /// Returns the storage keys of deleted rows so the caller can remove them from the archive.
    pub async fn delete_all(
        pool: &PgPool,
        workspace: &str,
        task_name: &str,
    ) -> Result<Vec<String>> {
        let keys = sqlx::query_scalar::<_, String>(
            "DELETE FROM task_state \
             WHERE workspace = $1 AND task_name = $2 \
             RETURNING storage_key",
        )
        .bind(workspace)
        .bind(task_name)
        .fetch_all(pool)
        .await
        .context("Failed to delete all task state snapshots")?;
        Ok(keys)
    }
}

/// The prune statement for one partition: `$1` workspace, `$2` task, `$3`
/// keep, and `$4` git_ref only for `Some`. Two `'static` shapes rather than
/// `IS NOT DISTINCT FROM`, so the btree index applies.
fn prune_sql(git_ref: Option<&str>) -> &'static str {
    match git_ref {
        Some(_) => {
            "DELETE FROM task_state WHERE id IN ( \
                 SELECT id FROM task_state \
                 WHERE workspace = $1 AND task_name = $2 AND git_ref = $4 \
                 ORDER BY created_at DESC, id DESC OFFSET $3) \
             RETURNING storage_key"
        }
        None => {
            "DELETE FROM task_state WHERE id IN ( \
                 SELECT id FROM task_state \
                 WHERE workspace = $1 AND task_name = $2 AND git_ref IS NULL \
                 ORDER BY created_at DESC, id DESC OFFSET $3) \
             RETURNING storage_key"
        }
    }
}
