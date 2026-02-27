use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKeyRow {
    pub key_hash: String,
    pub user_id: Uuid,
    pub name: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
}

pub struct ApiKeyRepo;

impl ApiKeyRepo {
    pub async fn create(
        pool: &PgPool,
        key_hash: &str,
        user_id: Uuid,
        name: &str,
        prefix: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO api_key (key_hash, user_id, name, prefix, expires_at) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(key_hash)
        .bind(user_id)
        .bind(name)
        .bind(prefix)
        .bind(expires_at)
        .execute(pool)
        .await
        .context("Failed to create API key")?;
        Ok(())
    }

    pub async fn get_by_hash(pool: &PgPool, hash: &str) -> Result<Option<ApiKeyRow>> {
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT key_hash, user_id, name, prefix, created_at, expires_at, last_used_at FROM api_key WHERE key_hash = $1",
        )
        .bind(hash)
        .fetch_optional(pool)
        .await
        .context("Failed to get API key")?;
        Ok(row)
    }

    pub async fn list_by_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<ApiKeyRow>> {
        let rows = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT key_hash, user_id, name, prefix, created_at, expires_at, last_used_at FROM api_key WHERE user_id = $1 ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await
        .context("Failed to list API keys")?;
        Ok(rows)
    }

    pub async fn delete(pool: &PgPool, hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM api_key WHERE key_hash = $1")
            .bind(hash)
            .execute(pool)
            .await
            .context("Failed to delete API key")?;
        Ok(())
    }

    pub async fn delete_by_prefix_and_user(
        pool: &PgPool,
        prefix: &str,
        user_id: Uuid,
    ) -> Result<bool> {
        let result = sqlx::query("DELETE FROM api_key WHERE prefix = $1 AND user_id = $2")
            .bind(prefix)
            .bind(user_id)
            .execute(pool)
            .await
            .context("Failed to delete API key by prefix")?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_last_used(pool: &PgPool, hash: &str) -> Result<()> {
        sqlx::query("UPDATE api_key SET last_used_at = NOW() WHERE key_hash = $1")
            .bind(hash)
            .execute(pool)
            .await
            .context("Failed to update API key last_used_at")?;
        Ok(())
    }
}
