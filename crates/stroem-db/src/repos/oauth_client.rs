use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OAuthClientRow {
    pub client_id: String,
    /// SHA-256 hex of the assigned client_secret. `None` ⇒ public client (PKCE-only).
    pub client_secret_hash: Option<String>,
    pub client_name: String,
    /// JSONB string array of allowed redirect URIs (exact match at /authorize).
    pub redirect_uris: Value,
    pub grant_types: Value,
    pub scope: String,
    pub is_dynamic: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl OAuthClientRow {
    /// Decode `redirect_uris` JSONB into a `Vec<String>`. Invalid storage
    /// returns an empty list; callers always do an exact-match check, so an
    /// empty list rejects every redirect — fail-closed.
    pub fn redirect_uris_vec(&self) -> Vec<String> {
        self.redirect_uris
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub struct OAuthClientRepo;

impl OAuthClientRepo {
    /// Insert a new client. Used by both admin tooling and Dynamic Client
    /// Registration (Phase 3). DCR sets `is_dynamic = true`.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        pool: &PgPool,
        client_id: &str,
        client_secret_hash: Option<&str>,
        client_name: &str,
        redirect_uris: &[String],
        grant_types: &[String],
        scope: &str,
        is_dynamic: bool,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO oauth_client \
             (client_id, client_secret_hash, client_name, redirect_uris, grant_types, scope, is_dynamic, expires_at) \
             VALUES ($1, $2, $3, $4::jsonb, $5::jsonb, $6, $7, $8)",
        )
        .bind(client_id)
        .bind(client_secret_hash)
        .bind(client_name)
        .bind(serde_json::to_value(redirect_uris)?)
        .bind(serde_json::to_value(grant_types)?)
        .bind(scope)
        .bind(is_dynamic)
        .bind(expires_at)
        .execute(pool)
        .await
        .context("Failed to insert oauth_client")?;
        Ok(())
    }

    pub async fn get(pool: &PgPool, client_id: &str) -> Result<Option<OAuthClientRow>> {
        let row = sqlx::query_as::<_, OAuthClientRow>(
            "SELECT client_id, client_secret_hash, client_name, redirect_uris, grant_types, \
                    scope, is_dynamic, created_at, expires_at \
             FROM oauth_client WHERE client_id = $1",
        )
        .bind(client_id)
        .fetch_optional(pool)
        .await
        .context("Failed to load oauth_client")?;
        Ok(row)
    }

    /// Delete expired DCR clients. Run periodically (alongside other
    /// retention sweeps) to keep the table small.
    pub async fn purge_expired(pool: &PgPool) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM oauth_client WHERE expires_at IS NOT NULL AND expires_at < NOW()",
        )
        .execute(pool)
        .await
        .context("Failed to purge expired oauth clients")?;
        Ok(result.rows_affected())
    }
}
