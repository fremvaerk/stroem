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
    pub created_at: DateTime<Utc>,
}

pub struct WorkspaceStateRepo;

impl WorkspaceStateRepo {
    /// Get the latest snapshot for a workspace (global scope — not scoped to any task).
    pub async fn get_latest(pool: &PgPool, workspace: &str) -> Result<Option<WorkspaceStateRow>> {
        let row = sqlx::query_as::<_, WorkspaceStateRow>(
            "SELECT id, workspace, written_by_task, job_id, storage_key, size_bytes, has_json, created_at \
             FROM workspace_state \
             WHERE workspace = $1 \
             ORDER BY created_at DESC, id DESC \
             LIMIT 1",
        )
        .bind(workspace)
        .fetch_optional(pool)
        .await
        .context("Failed to get latest workspace state snapshot")?;
        Ok(row)
    }

    /// Get a specific snapshot by ID.
    pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<WorkspaceStateRow>> {
        let row = sqlx::query_as::<_, WorkspaceStateRow>(
            "SELECT id, workspace, written_by_task, job_id, storage_key, size_bytes, has_json, created_at \
             FROM workspace_state \
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("Failed to get workspace state snapshot")?;
        Ok(row)
    }

    /// Insert a new snapshot record. Returns the generated ID.
    pub async fn insert(
        pool: &PgPool,
        workspace: &str,
        written_by_task: &str,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workspace_state (id, workspace, written_by_task, job_id, storage_key, size_bytes, has_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(workspace)
        .bind(written_by_task)
        .bind(job_id)
        .bind(storage_key)
        .bind(size_bytes)
        .bind(has_json)
        .execute(pool)
        .await
        .context("Failed to insert workspace state snapshot")?;
        Ok(id)
    }

    /// Insert a new snapshot and prune old ones, running both statements against
    /// the provided transaction.
    ///
    /// The caller is responsible for beginning and committing (or rolling back)
    /// the transaction. This lets callers compose additional SQL statements —
    /// such as inserting a synthetic job row — in the same atomic unit.
    ///
    /// `written_by_task` is stored for provenance but does NOT scope the prune —
    /// the oldest snapshots across the entire workspace are removed.
    ///
    /// `snapshot_id`: pass `Some(id)` to use a pre-generated UUID (useful when
    /// the caller needs the ID before calling this method, e.g. to include it in
    /// a job `output` column). Pass `None` to let the method generate one.
    ///
    /// Returns the snapshot UUID and the storage keys of any pruned rows.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_and_prune<'a>(
        tx: &mut Tx<'a>,
        workspace: &str,
        written_by_task: &str,
        job_id: Uuid,
        storage_key: &str,
        size_bytes: i64,
        has_json: bool,
        keep: usize,
        snapshot_id: Option<Uuid>,
    ) -> Result<(Uuid, Vec<String>)> {
        let id = snapshot_id.unwrap_or_else(Uuid::new_v4);

        sqlx::query(
            "INSERT INTO workspace_state (id, workspace, written_by_task, job_id, storage_key, size_bytes, has_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(workspace)
        .bind(written_by_task)
        .bind(job_id)
        .bind(storage_key)
        .bind(size_bytes)
        .bind(has_json)
        .execute(&mut **tx)
        .await
        .context("Failed to insert workspace state snapshot")?;

        let deleted_keys = sqlx::query_scalar::<_, String>(
            "DELETE FROM workspace_state \
             WHERE id IN ( \
                 SELECT id FROM workspace_state \
                 WHERE workspace = $1 \
                 ORDER BY created_at DESC, id DESC \
                 OFFSET $2 \
             ) \
             RETURNING storage_key",
        )
        .bind(workspace)
        .bind(keep as i64)
        .fetch_all(&mut **tx)
        .await
        .context("Failed to prune workspace state snapshots")?;

        Ok((id, deleted_keys))
    }

    /// List snapshots for a workspace ordered by created_at DESC.
    pub async fn list(pool: &PgPool, workspace: &str) -> Result<Vec<WorkspaceStateRow>> {
        let rows = sqlx::query_as::<_, WorkspaceStateRow>(
            "SELECT id, workspace, written_by_task, job_id, storage_key, size_bytes, has_json, created_at \
             FROM workspace_state \
             WHERE workspace = $1 \
             ORDER BY created_at DESC, id DESC",
        )
        .bind(workspace)
        .fetch_all(pool)
        .await
        .context("Failed to list workspace state snapshots")?;
        Ok(rows)
    }

    /// Delete old snapshots, keeping the N most recent across the workspace.
    /// Returns the storage keys of deleted rows so the caller can remove them from the archive.
    pub async fn prune(pool: &PgPool, workspace: &str, keep: usize) -> Result<Vec<String>> {
        let keys = sqlx::query_scalar::<_, String>(
            "DELETE FROM workspace_state \
             WHERE id IN ( \
                 SELECT id FROM workspace_state \
                 WHERE workspace = $1 \
                 ORDER BY created_at DESC, id DESC \
                 OFFSET $2 \
             ) \
             RETURNING storage_key",
        )
        .bind(workspace)
        .bind(keep as i64)
        .fetch_all(pool)
        .await
        .context("Failed to prune workspace state snapshots")?;
        Ok(keys)
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
