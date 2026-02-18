use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    pub user_id: Uuid,
    pub name: Option<String>,
    pub email: String,
    pub password_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

pub struct UserRepo;

impl UserRepo {
    pub async fn create(
        pool: &PgPool,
        user_id: Uuid,
        email: &str,
        password_hash: Option<&str>,
        name: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO "user" (user_id, email, password_hash, name) VALUES ($1, $2, $3, $4)"#,
        )
        .bind(user_id)
        .bind(email)
        .bind(password_hash)
        .bind(name)
        .execute(pool)
        .await
        .context("Failed to create user")?;
        Ok(())
    }

    pub async fn get_by_email(pool: &PgPool, email: &str) -> Result<Option<UserRow>> {
        let row = sqlx::query_as::<_, UserRow>(
            r#"SELECT user_id, name, email, password_hash, created_at, last_login_at FROM "user" WHERE email = $1"#,
        )
        .bind(email)
        .fetch_optional(pool)
        .await
        .context("Failed to get user by email")?;
        Ok(row)
    }

    pub async fn get_by_id(pool: &PgPool, user_id: Uuid) -> Result<Option<UserRow>> {
        let row = sqlx::query_as::<_, UserRow>(
            r#"SELECT user_id, name, email, password_hash, created_at, last_login_at FROM "user" WHERE user_id = $1"#,
        )
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .context("Failed to get user by id")?;
        Ok(row)
    }

    pub async fn touch_last_login(pool: &PgPool, user_id: Uuid) -> Result<()> {
        sqlx::query(r#"UPDATE "user" SET last_login_at = NOW() WHERE user_id = $1"#)
            .bind(user_id)
            .execute(pool)
            .await
            .context("Failed to update last_login_at")?;
        Ok(())
    }

    pub async fn list(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<UserRow>> {
        let rows = sqlx::query_as::<_, UserRow>(
            r#"SELECT user_id, name, email, password_hash, created_at, last_login_at FROM "user" ORDER BY created_at DESC LIMIT $1 OFFSET $2"#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .context("Failed to list users")?;
        Ok(rows)
    }
}
