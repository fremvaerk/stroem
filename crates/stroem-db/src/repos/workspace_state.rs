use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WorkspaceStateRow {
    pub id: Uuid,
    pub workspace: String,
    pub written_by_task: String,
    pub job_id: Option<Uuid>,
    pub storage_key: String,
    pub size_bytes: i64,
    pub has_json: bool,
    pub state_json: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    /// The partition (spec 2026-10-02 § 7.6); `None` = unpinned.
    pub git_ref: Option<String>,
}

/// The [`WorkspaceStateRow`] column list. A macro rather than a `const` so a
/// query can `concat!` it into a `&'static str`, which sqlx accepts as SQL
/// directly.
macro_rules! columns {
    () => {
        "id, workspace, written_by_task, job_id, storage_key, size_bytes, has_json, state_json, created_at, git_ref"
    };
}

pub struct WorkspaceStateRepo;

impl WorkspaceStateRepo {
    /// Latest snapshot of the unpinned partition.
    pub async fn get_latest(pool: &PgPool, workspace: &str) -> Result<Option<WorkspaceStateRow>> {
        Self::get_latest_for_ref(pool, workspace, None).await
    }

    /// Latest snapshot of one `(workspace, git_ref)` partition.
    pub async fn get_latest_for_ref(
        pool: &PgPool,
        workspace: &str,
        git_ref: Option<&str>,
    ) -> Result<Option<WorkspaceStateRow>> {
        let sql = if git_ref.is_some() {
            concat!(
                "SELECT ",
                columns!(),
                " FROM workspace_state WHERE workspace = $1 AND git_ref = $2 \
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            )
        } else {
            concat!(
                "SELECT ",
                columns!(),
                " FROM workspace_state WHERE workspace = $1 AND git_ref IS NULL \
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            )
        };
        let mut q = sqlx::query_as::<_, WorkspaceStateRow>(sql).bind(workspace);
        if let Some(r) = git_ref {
            q = q.bind(r);
        }
        q.fetch_optional(pool)
            .await
            .context("Failed to get latest workspace state snapshot")
    }

    /// Get a specific snapshot by ID.
    pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<WorkspaceStateRow>> {
        sqlx::query_as::<_, WorkspaceStateRow>(concat!(
            "SELECT ",
            columns!(),
            " FROM workspace_state WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("Failed to get workspace state snapshot")
    }

    /// Insert into the unpinned partition.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        pool: &PgPool,
        workspace: &str,
        written_by_task: &str,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
        state_json: Option<&serde_json::Value>,
    ) -> Result<Uuid> {
        Self::insert_for_ref(
            pool,
            workspace,
            None,
            written_by_task,
            job_id,
            storage_key,
            size_bytes,
            has_json,
            state_json,
        )
        .await
    }

    /// Insert a new snapshot record into `git_ref`'s partition.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_for_ref(
        pool: &PgPool,
        workspace: &str,
        git_ref: Option<&str>,
        written_by_task: &str,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
        state_json: Option<&serde_json::Value>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(INSERT_SQL)
            .bind(id)
            .bind(workspace)
            .bind(written_by_task)
            .bind(job_id)
            .bind(storage_key)
            .bind(size_bytes)
            .bind(has_json)
            .bind(state_json.map(|v| v.to_string()))
            .bind(git_ref)
            .execute(pool)
            .await
            .context("Failed to insert workspace state snapshot")?;
        Ok(id)
    }

    /// [`Self::insert_and_prune_for_ref`] on the unpinned partition.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_and_prune<'a>(
        tx: &mut Tx<'a>,
        workspace: &str,
        written_by_task: &str,
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
            None,
            written_by_task,
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

    /// Insert into `git_ref`'s partition and prune that partition to `keep`,
    /// against the caller's transaction. `written_by_task` is provenance only
    /// and does not scope the prune; the partition does.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_and_prune_for_ref<'a>(
        tx: &mut Tx<'a>,
        workspace: &str,
        git_ref: Option<&str>,
        written_by_task: &str,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
        state_json: Option<&serde_json::Value>,
        keep: usize,
        snapshot_id: Option<Uuid>,
    ) -> Result<(Uuid, Vec<String>)> {
        let id = snapshot_id.unwrap_or_else(Uuid::new_v4);
        sqlx::query(INSERT_SQL)
            .bind(id)
            .bind(workspace)
            .bind(written_by_task)
            .bind(job_id)
            .bind(storage_key)
            .bind(size_bytes)
            .bind(has_json)
            .bind(state_json.map(|v| v.to_string()))
            .bind(git_ref)
            .execute(&mut **tx)
            .await
            .context("Failed to insert workspace state snapshot")?;
        let mut q = sqlx::query_scalar::<_, String>(prune_sql(git_ref))
            .bind(workspace)
            .bind(keep as i64);
        if let Some(r) = git_ref {
            q = q.bind(r);
        }
        let deleted_keys = q
            .fetch_all(&mut **tx)
            .await
            .context("Failed to prune workspace state snapshots")?;
        Ok((id, deleted_keys))
    }

    /// List every snapshot of a workspace, all partitions, newest first.
    pub async fn list(pool: &PgPool, workspace: &str) -> Result<Vec<WorkspaceStateRow>> {
        sqlx::query_as::<_, WorkspaceStateRow>(concat!(
            "SELECT ",
            columns!(),
            " FROM workspace_state WHERE workspace = $1 \
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(workspace)
        .fetch_all(pool)
        .await
        .context("Failed to list workspace state snapshots")
    }

    /// [`Self::prune_for_ref`] on the unpinned partition.
    pub async fn prune(pool: &PgPool, workspace: &str, keep: usize) -> Result<Vec<String>> {
        Self::prune_for_ref(pool, workspace, None, keep).await
    }

    /// Keep the `keep` newest snapshots of one partition.
    pub async fn prune_for_ref(
        pool: &PgPool,
        workspace: &str,
        git_ref: Option<&str>,
        keep: usize,
    ) -> Result<Vec<String>> {
        let mut q = sqlx::query_scalar::<_, String>(prune_sql(git_ref))
            .bind(workspace)
            .bind(keep as i64);
        if let Some(r) = git_ref {
            q = q.bind(r);
        }
        q.fetch_all(pool)
            .await
            .context("Failed to prune workspace state snapshots")
    }

    /// Delete all snapshots for a workspace.
    /// Returns the storage keys of deleted rows so the caller can remove them from the archive.
    pub async fn delete_all(pool: &PgPool, workspace: &str) -> Result<Vec<String>> {
        let keys = sqlx::query_scalar::<_, String>(
            "DELETE FROM workspace_state \
             WHERE workspace = $1 \
             RETURNING storage_key",
        )
        .bind(workspace)
        .fetch_all(pool)
        .await
        .context("Failed to delete all workspace state snapshots")?;
        Ok(keys)
    }
}

const INSERT_SQL: &str = "INSERT INTO workspace_state (id, workspace, written_by_task, job_id, storage_key, size_bytes, has_json, state_json, git_ref) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8::json, $9)";

/// The prune statement for one partition; `$3` is bound only for `Some`.
fn prune_sql(git_ref: Option<&str>) -> &'static str {
    match git_ref {
        Some(_) => {
            "DELETE FROM workspace_state WHERE id IN ( \
                 SELECT id FROM workspace_state WHERE workspace = $1 AND git_ref = $3 \
                 ORDER BY created_at DESC, id DESC OFFSET $2) \
             RETURNING storage_key"
        }
        None => {
            "DELETE FROM workspace_state WHERE id IN ( \
                 SELECT id FROM workspace_state WHERE workspace = $1 AND git_ref IS NULL \
                 ORDER BY created_at DESC, id DESC OFFSET $2) \
             RETURNING storage_key"
        }
    }
}
