use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RefreshTokenRow {
    pub token_hash: String,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

pub struct RefreshTokenRepo;

impl RefreshTokenRepo {
    pub async fn create(
        pool: &PgPool,
        token_hash: &str,
        user_id: Uuid,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO refresh_token (token_hash, user_id, expires_at) VALUES ($1, $2, $3)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(expires_at)
        .execute(pool)
        .await
        .context("Failed to create refresh token")?;
        Ok(())
    }

    pub async fn get_by_hash(pool: &PgPool, hash: &str) -> Result<Option<RefreshTokenRow>> {
        let row = sqlx::query_as::<_, RefreshTokenRow>(
            "SELECT token_hash, user_id, expires_at, created_at FROM refresh_token WHERE token_hash = $1",
        )
        .bind(hash)
        .fetch_optional(pool)
        .await
        .context("Failed to get refresh token")?;
        Ok(row)
    }

    pub async fn delete(pool: &PgPool, hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM refresh_token WHERE token_hash = $1")
            .bind(hash)
            .execute(pool)
            .await
            .context("Failed to delete refresh token")?;
        Ok(())
    }

    pub async fn delete_all_for_user(pool: &PgPool, user_id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM refresh_token WHERE user_id = $1")
            .bind(user_id)
            .execute(pool)
            .await
            .context("Failed to delete all refresh tokens for user")?;
        Ok(())
    }
}
