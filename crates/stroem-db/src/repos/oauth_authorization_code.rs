use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OAuthAuthorizationCodeRow {
    pub code_hash: String,
    pub client_id: String,
    pub user_id: Uuid,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub scope: String,
    pub resource: String,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
}

pub struct OAuthAuthorizationCodeRepo;

impl OAuthAuthorizationCodeRepo {
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        pool: &PgPool,
        code_hash: &str,
        client_id: &str,
        user_id: Uuid,
        redirect_uri: &str,
        code_challenge: &str,
        code_challenge_method: &str,
        scope: &str,
        resource: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO oauth_authorization_code \
             (code_hash, client_id, user_id, redirect_uri, code_challenge, code_challenge_method, scope, resource, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(code_hash)
        .bind(client_id)
        .bind(user_id)
        .bind(redirect_uri)
        .bind(code_challenge)
        .bind(code_challenge_method)
        .bind(scope)
        .bind(resource)
        .bind(expires_at)
        .execute(pool)
        .await
        .context("Failed to insert oauth_authorization_code")?;
        Ok(())
    }

    /// Read an authorization code without consuming it. Returns rows that
    /// are still unused and unexpired. Callers MUST validate client/PKCE
    /// against this row first and only call `consume` on success — that
    /// way an attacker who races the legitimate client with a wrong
    /// `code_verifier` can't burn the user's code.
    pub async fn get_unused(
        pool: &PgPool,
        code_hash: &str,
    ) -> Result<Option<OAuthAuthorizationCodeRow>> {
        let row = sqlx::query_as::<_, OAuthAuthorizationCodeRow>(
            "SELECT code_hash, client_id, user_id, redirect_uri, code_challenge, \
                    code_challenge_method, scope, resource, expires_at, used_at \
             FROM oauth_authorization_code \
             WHERE code_hash = $1 AND used_at IS NULL AND expires_at > NOW()",
        )
        .bind(code_hash)
        .fetch_optional(pool)
        .await
        .context("Failed to load oauth_authorization_code")?;
        Ok(row)
    }

    /// Atomically consume an authorization code: mark `used_at` and return
    /// the row only if it wasn't already used. Returns `Ok(None)` if the
    /// code is unknown, expired, or already consumed — three cases the
    /// caller maps to the same `invalid_grant` error per OAuth 2.1 §5.2.
    ///
    /// The `UPDATE ... WHERE used_at IS NULL ... RETURNING` pattern is the
    /// safest single-row CAS in Postgres: even under concurrent /token
    /// requests with the same code (a replay attack), only one wins.
    pub async fn consume(
        pool: &PgPool,
        code_hash: &str,
    ) -> Result<Option<OAuthAuthorizationCodeRow>> {
        let row = sqlx::query_as::<_, OAuthAuthorizationCodeRow>(
            "UPDATE oauth_authorization_code \
             SET used_at = NOW() \
             WHERE code_hash = $1 AND used_at IS NULL AND expires_at > NOW() \
             RETURNING code_hash, client_id, user_id, redirect_uri, code_challenge, \
                       code_challenge_method, scope, resource, expires_at, used_at",
        )
        .bind(code_hash)
        .fetch_optional(pool)
        .await
        .context("Failed to consume oauth_authorization_code")?;
        Ok(row)
    }

    pub async fn purge_expired(pool: &PgPool) -> Result<u64> {
        let result = sqlx::query("DELETE FROM oauth_authorization_code WHERE expires_at < NOW()")
            .execute(pool)
            .await
            .context("Failed to purge expired oauth_authorization_codes")?;
        Ok(result.rows_affected())
    }
}
