//! `job_ref_pin`: one commit per `(job, owner workspace, ref as written or
//! rendered)` for the job's whole life (spec 2026-10-15 § 5.2). Seeded with a
//! creation's `RefMemo` inside its transaction; extended by the deferred-step
//! pass. First writer wins.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

/// One memo entry.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct JobRefPinRow {
    pub workspace: String,
    pub git_ref: String,
    pub commit: String,
}

pub struct JobRefPinRepo;

impl JobRefPinRepo {
    /// Write `entries` for `job_id`; an existing key keeps its commit.
    pub async fn insert_many_tx<'e, E>(
        executor: E,
        job_id: Uuid,
        entries: &[JobRefPinRow],
    ) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        if entries.is_empty() {
            return Ok(());
        }
        let workspaces: Vec<String> = entries.iter().map(|e| e.workspace.clone()).collect();
        let refs: Vec<String> = entries.iter().map(|e| e.git_ref.clone()).collect();
        let commits: Vec<String> = entries.iter().map(|e| e.commit.clone()).collect();
        sqlx::query(
            "INSERT INTO job_ref_pin (job_id, workspace, git_ref, commit) \
             SELECT $1, w, r, c FROM UNNEST($2::text[], $3::text[], $4::text[]) AS t(w, r, c) \
             ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(workspaces)
        .bind(refs)
        .bind(commits)
        .execute(executor)
        .await
        .context("insert job_ref_pin entries")?;
        Ok(())
    }

    /// The commit `(workspace, git_ref)` resolved to in `job_id`, if any.
    pub async fn get(
        pool: &PgPool,
        job_id: Uuid,
        workspace: &str,
        git_ref: &str,
    ) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT commit FROM job_ref_pin WHERE job_id = $1 AND workspace = $2 AND git_ref = $3",
        )
        .bind(job_id)
        .bind(workspace)
        .bind(git_ref)
        .fetch_optional(pool)
        .await
        .context("get job_ref_pin entry")
    }

    /// First writer wins: write `commit` unless the key exists, then return
    /// the STORED commit — another resolver's, if it wrote first.
    pub async fn insert_first_wins(
        pool: &PgPool,
        job_id: Uuid,
        workspace: &str,
        git_ref: &str,
        commit: &str,
    ) -> Result<String> {
        sqlx::query(
            "INSERT INTO job_ref_pin (job_id, workspace, git_ref, commit) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(workspace)
        .bind(git_ref)
        .bind(commit)
        .execute(pool)
        .await
        .context("insert job_ref_pin entry")?;
        Self::get(pool, job_id, workspace, git_ref)
            .await?
            .context("job_ref_pin entry missing right after its insert")
    }

    /// Every entry of `job_id`, ordered by workspace then ref.
    pub async fn list_for_job(pool: &PgPool, job_id: Uuid) -> Result<Vec<JobRefPinRow>> {
        sqlx::query_as::<_, JobRefPinRow>(
            "SELECT workspace, git_ref, commit FROM job_ref_pin \
             WHERE job_id = $1 ORDER BY workspace, git_ref",
        )
        .bind(job_id)
        .fetch_all(pool)
        .await
        .context("list job_ref_pin entries")
    }
}
