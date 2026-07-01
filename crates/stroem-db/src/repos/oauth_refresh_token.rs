use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OAuthRefreshTokenRow {
    pub token_hash: String,
    pub client_id: String,
    pub user_id: Uuid,
    pub scope: String,
    pub resource: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub struct OAuthRefreshTokenRepo;

impl OAuthRefreshTokenRepo {
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        pool: &PgPool,
        token_hash: &str,
        client_id: &str,
        user_id: Uuid,
        scope: &str,
        resource: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO oauth_refresh_token \
             (token_hash, client_id, user_id, scope, resource, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(token_hash)
        .bind(client_id)
        .bind(user_id)
        .bind(scope)
        .bind(resource)
        .bind(expires_at)
        .execute(pool)
        .await
        .context("Failed to insert oauth_refresh_token")?;
        Ok(())
    }

    pub async fn get(pool: &PgPool, token_hash: &str) -> Result<Option<OAuthRefreshTokenRow>> {
        let row = sqlx::query_as::<_, OAuthRefreshTokenRow>(
            "SELECT token_hash, client_id, user_id, scope, resource, expires_at, created_at, revoked_at \
             FROM oauth_refresh_token WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(pool)
        .await
        .context("Failed to load oauth_refresh_token")?;
        Ok(row)
    }

    /// Rotate: atomically revoke the old token and insert the new one. OAuth
    /// 2.1 §4.13 requires refresh-token rotation for public clients; the AS
    /// MUST treat any subsequent use of the old token as a sentinel for token
    /// theft and revoke the chain. We achieve that by setting `revoked_at`
    /// here and checking `revoked_at IS NULL` on every refresh.
    #[allow(clippy::too_many_arguments)]
    pub async fn rotate(
        pool: &PgPool,
        old_hash: &str,
        new_hash: &str,
        client_id: &str,
        user_id: Uuid,
        scope: &str,
        resource: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut tx = pool.begin().await.context("Failed to begin transaction")?;

        let revoked = sqlx::query(
            "UPDATE oauth_refresh_token SET revoked_at = NOW() \
             WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > NOW()",
        )
        .bind(old_hash)
        .execute(&mut *tx)
        .await
        .context("Failed to revoke prior refresh token")?;

        if revoked.rows_affected() == 0 {
            tx.rollback().await.ok();
            return Ok(false);
        }

        sqlx::query(
            "INSERT INTO oauth_refresh_token \
             (token_hash, client_id, user_id, scope, resource, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(new_hash)
        .bind(client_id)
        .bind(user_id)
        .bind(scope)
        .bind(resource)
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .context("Failed to insert rotated oauth_refresh_token")?;

        tx.commit().await.context("Failed to commit rotation")?;
        Ok(true)
    }

    /// Revoke every refresh token for a (client_id, user_id) pair. Called
    /// on token-reuse detection per OAuth 2.1 §4.13 — if a previously
    /// rotated (revoked) refresh token shows up again, one of the parties
    /// is an attacker, so we kill the entire chain rather than just the
    /// replayed token. The legitimate client will see its next refresh
    /// fail and have to re-authorize, which is the intended UX.
    pub async fn revoke_chain(pool: &PgPool, client_id: &str, user_id: Uuid) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE oauth_refresh_token \
             SET revoked_at = NOW() \
             WHERE client_id = $1 AND user_id = $2 AND revoked_at IS NULL",
        )
        .bind(client_id)
        .bind(user_id)
        .execute(pool)
        .await
        .context("Failed to revoke refresh-token chain")?;
        Ok(result.rows_affected())
    }

    pub async fn purge_expired(pool: &PgPool) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM oauth_refresh_token \
             WHERE expires_at < NOW() OR (revoked_at IS NOT NULL AND revoked_at < NOW() - INTERVAL '7 days')",
        )
        .execute(pool)
        .await
        .context("Failed to purge expired oauth_refresh_tokens")?;
        Ok(result.rows_affected())
    }
}
