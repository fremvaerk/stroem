use anyhow::{Context, Result};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use openidconnect::core::CoreProviderMetadata;
use openidconnect::{ClientId, ClientSecret, IssuerUrl, RedirectUrl};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use stroem_db::{UserAuthLinkRepo, UserGroupRepo, UserRepo, UserRow};
use uuid::Uuid;

use crate::config::ProviderConfig;

/// Initialized OIDC provider ready for auth flows
pub struct OidcProvider {
    pub metadata: CoreProviderMetadata,
    pub client_id: ClientId,
    pub client_secret: ClientSecret,
    pub redirect_url: RedirectUrl,
    pub display_name: String,
    /// Group memberships to assign the first time a user is created via
    /// this provider. See `ProviderConfig::default_groups`.
    pub default_groups: Vec<String>,
}

/// Claims stored in the OIDC state cookie JWT
#[derive(Debug, Serialize, Deserialize)]
pub struct OidcStateClaims {
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub provider: String,
    pub exp: i64,
    /// Same-origin path to land on after the OIDC callback succeeds.
    /// Used by the consent flow to round-trip `/consent?...` through
    /// `/login?next=/consent?...` and back. Validated as a same-origin
    /// path on the callback side; absent or invalid → land at `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Create a signed JWT for OIDC state (stored in HttpOnly cookie)
pub fn create_state_jwt(claims: &OidcStateClaims, secret: &str) -> Result<String> {
    jsonwebtoken::encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .context("Failed to create OIDC state JWT")
}

/// Validate and decode an OIDC state JWT
pub fn validate_state_jwt(token: &str, secret: &str) -> Result<OidcStateClaims> {
    let mut validation = Validation::default();
    // State JWT has no sub/iss claims
    validation.required_spec_claims.clear();
    validation.validate_exp = true;

    let token_data = jsonwebtoken::decode::<OidcStateClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .context("Invalid OIDC state JWT")?;
    Ok(token_data.claims)
}

/// Initialize OIDC providers from config (performs discovery for each)
#[tracing::instrument(skip(providers))]
pub async fn init_providers(
    providers: &HashMap<String, ProviderConfig>,
    base_url: &str,
) -> Result<HashMap<String, OidcProvider>> {
    let http_client = openidconnect::reqwest::ClientBuilder::new()
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .build()
        .context("Failed to build HTTP client for OIDC discovery")?;

    let mut result = HashMap::new();

    for (name, config) in providers {
        if config.provider_type != "oidc" {
            continue;
        }

        let issuer_url = config
            .issuer_url
            .as_ref()
            .with_context(|| format!("OIDC provider '{}' missing issuer_url", name))?;

        let client_id = config
            .client_id
            .as_ref()
            .with_context(|| format!("OIDC provider '{}' missing client_id", name))?;

        let client_secret = config
            .client_secret
            .as_ref()
            .with_context(|| format!("OIDC provider '{}' missing client_secret", name))?;

        tracing::info!("Discovering OIDC provider '{}' at {}", name, issuer_url);

        let issuer = IssuerUrl::new(issuer_url.clone())
            .with_context(|| format!("Invalid issuer URL for provider '{}'", name))?;

        let metadata = CoreProviderMetadata::discover_async(issuer, &http_client)
            .await
            .with_context(|| format!("OIDC discovery failed for provider '{}'", name))?;

        let redirect_uri = format!("{}/api/auth/oidc/{}/callback", base_url, name);
        let redirect_url = RedirectUrl::new(redirect_uri)
            .with_context(|| format!("Invalid redirect URL for provider '{}'", name))?;

        let display_name = config.display_name.clone().unwrap_or_else(|| name.clone());

        // Validate default_groups here rather than at provision time so
        // a bad config fails startup, not silently the first time
        // someone logs in via this provider.
        for g in &config.default_groups {
            if g.is_empty() || g.len() > 64 {
                anyhow::bail!(
                    "OIDC provider '{name}': default_groups entry must be 1-64 chars, got '{g}'"
                );
            }
            if !g
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                anyhow::bail!(
                    "OIDC provider '{name}': default_groups entry '{g}' has invalid characters \
                     (only alphanumeric, _, - allowed)"
                );
            }
        }

        tracing::info!(
            "OIDC provider '{}' initialized successfully ({} default group(s))",
            name,
            config.default_groups.len()
        );

        result.insert(
            name.clone(),
            OidcProvider {
                metadata,
                client_id: ClientId::new(client_id.clone()),
                client_secret: ClientSecret::new(client_secret.clone()),
                redirect_url,
                display_name,
                default_groups: config.default_groups.clone(),
            },
        );
    }

    Ok(result)
}

/// JIT (Just-In-Time) user provisioning for OIDC login.
///
/// 1. Check if an auth_link exists for this provider+external_id → return that user
/// 2. Check if a user with this email already exists → create auth_link → return user
/// 3. Create a new user (no password) + auth_link → return user, and assign
///    `default_groups` (idempotent) so ACL rules apply on the first request.
///
/// `default_groups` is only used on path 3. Paths 1 and 2 leave existing
/// memberships alone — an admin who edits groups in the UI must not have
/// them silently re-applied on every login.
#[tracing::instrument(skip(pool, default_groups))]
pub async fn provision_user(
    pool: &PgPool,
    provider_id: &str,
    external_id: &str,
    email: &str,
    name: Option<&str>,
    default_groups: &[String],
) -> Result<UserRow> {
    // 1. Check existing auth link
    if let Some(link) =
        UserAuthLinkRepo::get_by_provider_and_external_id(pool, provider_id, external_id).await?
    {
        let user = UserRepo::get_by_id(pool, link.user_id)
            .await?
            .context("User referenced by auth_link not found")?;
        return Ok(user);
    }

    // 2. Check if user with this email exists → link them
    if let Some(user) = UserRepo::get_by_email(pool, email).await? {
        UserAuthLinkRepo::create(pool, user.user_id, provider_id, external_id).await?;
        tracing::info!(
            "Linked existing user {} to OIDC provider {}",
            user.email,
            provider_id
        );
        return Ok(user);
    }

    // 3. Create new user + link
    // First OIDC user becomes admin (if no users exist yet)
    let is_first_user = UserRepo::count(pool).await.unwrap_or(1) == 0;

    let user_id = Uuid::new_v4();
    UserRepo::create(pool, user_id, email, None, name).await?;
    UserAuthLinkRepo::create(pool, user_id, provider_id, external_id).await?;

    if is_first_user {
        UserRepo::set_admin(pool, user_id, true).await?;
        tracing::info!(
            "Created first user {} as admin via OIDC provider {}",
            email,
            provider_id
        );
    } else {
        tracing::info!(
            "Created new user {} via OIDC provider {} ({} default group(s))",
            email,
            provider_id,
            default_groups.len()
        );
    }

    for group in default_groups {
        if let Err(e) = UserGroupRepo::add(pool, user_id, group).await {
            // Log but don't fail the login — the user still exists and
            // an admin can add missing groups later. A hard fail here
            // would leave the user in an intermediate state (row exists
            // + auth_link exists but they can't log in).
            tracing::warn!(
                user_id = %user_id,
                email = %email,
                group = %group,
                "Failed to add JIT-provisioned user to default group: {:#}",
                e
            );
        }
    }

    let user = UserRepo::get_by_id(pool, user_id)
        .await?
        .context("Newly created user not found")?;

    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_validate_state_jwt() {
        let secret = "test-state-secret";
        let claims = OidcStateClaims {
            state: "random-state-123".to_string(),
            nonce: "nonce-456".to_string(),
            pkce_verifier: "verifier-789".to_string(),
            provider: "google".to_string(),
            exp: chrono::Utc::now().timestamp() + 600,
            next: None,
        };

        let jwt = create_state_jwt(&claims, secret).unwrap();
        let decoded = validate_state_jwt(&jwt, secret).unwrap();

        assert_eq!(decoded.state, "random-state-123");
        assert_eq!(decoded.nonce, "nonce-456");
        assert_eq!(decoded.pkce_verifier, "verifier-789");
        assert_eq!(decoded.provider, "google");
    }

    #[test]
    fn test_expired_state_jwt_fails() {
        let secret = "test-state-secret";
        let claims = OidcStateClaims {
            state: "state".to_string(),
            nonce: "nonce".to_string(),
            pkce_verifier: "verifier".to_string(),
            provider: "google".to_string(),
            exp: chrono::Utc::now().timestamp() - 120, // expired (past leeway)
            next: None,
        };

        let jwt = create_state_jwt(&claims, secret).unwrap();
        let result = validate_state_jwt(&jwt, secret);
        assert!(result.is_err());
    }

    #[test]
    fn test_wrong_secret_fails() {
        let claims = OidcStateClaims {
            state: "state".to_string(),
            nonce: "nonce".to_string(),
            pkce_verifier: "verifier".to_string(),
            provider: "google".to_string(),
            exp: chrono::Utc::now().timestamp() + 600,
            next: None,
        };

        let jwt = create_state_jwt(&claims, "secret-1").unwrap();
        let result = validate_state_jwt(&jwt, "secret-2");
        assert!(result.is_err());
    }
}
