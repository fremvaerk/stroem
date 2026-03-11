use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserGroupRow {
    pub group_name: String,
    pub user_id: Uuid,
    pub created_at: DateTime<Utc>,
}

pub struct UserGroupRepo;

impl UserGroupRepo {
    /// Get all group names for a user.
    pub async fn get_groups_for_user(pool: &PgPool, user_id: Uuid) -> Result<HashSet<String>> {
        let rows =
            sqlx::query_scalar::<_, String>("SELECT group_name FROM user_group WHERE user_id = $1")
                .bind(user_id)
                .fetch_all(pool)
                .await
                .context("Failed to get groups for user")?;
        Ok(rows.into_iter().collect())
    }

    /// Batch query: get groups for multiple users.
    pub async fn get_groups_for_users(
        pool: &PgPool,
        user_ids: &[Uuid],
    ) -> Result<Vec<UserGroupRow>> {
        let rows = sqlx::query_as::<_, UserGroupRow>(
            "SELECT group_name, user_id, created_at FROM user_group WHERE user_id = ANY($1)",
        )
        .bind(user_ids)
        .fetch_all(pool)
        .await
        .context("Failed to get groups for users")?;
        Ok(rows)
    }

    /// Add user to a group (idempotent).
    pub async fn add(pool: &PgPool, user_id: Uuid, group_name: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_group (group_name, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(group_name)
        .bind(user_id)
        .execute(pool)
        .await
        .context("Failed to add user to group")?;
        Ok(())
    }

    /// Remove user from a group.
    pub async fn remove(pool: &PgPool, user_id: Uuid, group_name: &str) -> Result<()> {
        sqlx::query("DELETE FROM user_group WHERE group_name = $1 AND user_id = $2")
            .bind(group_name)
            .bind(user_id)
            .execute(pool)
            .await
            .context("Failed to remove user from group")?;
        Ok(())
    }

    /// Set groups for a user (replaces all existing groups atomically).
    pub async fn set_groups(pool: &PgPool, user_id: Uuid, groups: &[String]) -> Result<()> {
        let mut tx = pool.begin().await.context("Failed to begin transaction")?;
        sqlx::query("DELETE FROM user_group WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .context("Failed to clear user groups")?;
        for group in groups {
            sqlx::query("INSERT INTO user_group (group_name, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                .bind(group)
                .bind(user_id)
                .execute(&mut *tx)
                .await
                .context("Failed to insert user group")?;
        }
        tx.commit().await.context("Failed to commit group update")?;
        Ok(())
    }

    /// List all distinct group names.
    pub async fn list_groups(pool: &PgPool) -> Result<Vec<String>> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT group_name FROM user_group ORDER BY group_name",
        )
        .fetch_all(pool)
        .await
        .context("Failed to list groups")?;
        Ok(rows)
    }
}
