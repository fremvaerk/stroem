use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserAuthLinkRow {
    pub user_id: Uuid,
    pub provider_id: String,
    pub external_id: String,
    pub created_at: DateTime<Utc>,
}

pub struct UserAuthLinkRepo;

impl UserAuthLinkRepo {
    pub async fn get_by_provider_and_external_id(
        pool: &PgPool,
        provider_id: &str,
        external_id: &str,
    ) -> Result<Option<UserAuthLinkRow>> {
        let row = sqlx::query_as::<_, UserAuthLinkRow>(
            "SELECT user_id, provider_id, external_id, created_at FROM user_auth_link WHERE provider_id = $1 AND external_id = $2",
        )
        .bind(provider_id)
        .bind(external_id)
        .fetch_optional(pool)
        .await
        .context("Failed to get auth link by provider and external id")?;
        Ok(row)
    }

    pub async fn create(
        pool: &PgPool,
        user_id: Uuid,
        provider_id: &str,
        external_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_auth_link (user_id, provider_id, external_id) VALUES ($1, $2, $3)",
        )
        .bind(user_id)
        .bind(provider_id)
        .bind(external_id)
        .execute(pool)
        .await
        .context("Failed to create auth link")?;
        Ok(())
    }
}
