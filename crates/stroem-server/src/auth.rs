use anyhow::{Context, Result};
use argon2::{
    password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier},
    Argon2,
};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use rand::{rand_core::UnwrapErr, rngs::SysRng, Rng};
use sha2::{Digest, Sha256};
use stroem_common::models::auth::Claims;

/// Hash a password using argon2id
pub fn hash_password(password: &str) -> Result<String> {
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("Failed to hash password: {}", e))?;
    Ok(hash.to_string())
}

/// Verify a password against a hash
pub fn verify_password(password: &str, hash: &str) -> Result<bool> {
    let parsed_hash =
        PasswordHash::new(hash).map_err(|e| anyhow::anyhow!("Invalid password hash: {}", e))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

/// Create an access token (JWT) with 15-minute TTL
pub fn create_access_token(
    user_id: &str,
    email: &str,
    is_admin: bool,
    jwt_secret: &str,
) -> Result<String> {
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        email: email.to_string(),
        is_admin,
        iat: now,
        exp: now + 900, // 15 minutes
        // Login JWTs aren't audience-bound; OAuth-issued tokens (Phase 2)
        // set aud/iss/scope/client_id. MCP middleware treats absent aud
        // as a legacy token and skips the audience check.
        ..Default::default()
    };
    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
    .context("Failed to create access token")
}

/// Validate an access token and return claims.
///
/// `expected_aud` enforces audience binding at the resource-server boundary:
///
/// * `None` — the token **must NOT carry an `aud` claim**. This is the
///   posture for login-flow JWTs and the REST API (`/api/*`). An OAuth
///   token minted for `/mcp` carries `aud=".../mcp"`; it must be rejected
///   here so the consent-scoped MCP grant does not bleed into the full
///   REST surface.
/// * `Some(expected)` — the token **must carry `aud == expected`**. This
///   is the posture for `/mcp`: tokens from `/oauth/token` set this claim,
///   and absence (legacy login JWT) is rejected so the consent screen is
///   actually the only path that authorises an MCP client.
///
/// We disable `jsonwebtoken`'s built-in audience check because its
/// "missing aud is OK" / "any aud is OK if validate_aud=false" modes
/// don't match the two-direction posture above. We do the comparison
/// explicitly so the policy is local to one function.
pub fn validate_access_token(
    token: &str,
    jwt_secret: &str,
    expected_aud: Option<&str>,
) -> Result<Claims> {
    let validation = Validation {
        validate_aud: false,
        ..Default::default()
    };
    let token_data = jsonwebtoken::decode::<Claims>(
        token,
        &DecodingKey::from_secret(jwt_secret.as_bytes()),
        &validation,
    )
    .context("Invalid access token")?;
    let claims = token_data.claims;

    match (expected_aud, claims.aud.as_deref()) {
        (None, None) => Ok(claims),
        (None, Some(a)) => {
            anyhow::bail!(
                "Token bound to audience '{a}' cannot be used here; \
                 use the appropriate resource-server endpoint instead"
            )
        }
        (Some(_), None) => {
            anyhow::bail!("Token is not bound to this resource; obtain one via the OAuth flow")
        }
        (Some(expected), Some(actual)) if expected == actual => Ok(claims),
        (Some(expected), Some(actual)) => {
            anyhow::bail!("Token audience mismatch (expected '{expected}', got '{actual}')")
        }
    }
}

/// Generate a refresh token: returns (raw_token, token_hash)
pub fn generate_refresh_token() -> (String, String) {
    let raw = uuid::Uuid::new_v4().to_string();
    let hash = hash_refresh_token(&raw);
    (raw, hash)
}

/// Hash an arbitrary token string using SHA256.
///
/// This is the canonical implementation used by all token hashing functions.
///
/// # Examples
///
/// ```
/// use stroem_server::auth::hash_token;
/// let h1 = hash_token("my-token");
/// let h2 = hash_token("my-token");
/// assert_eq!(h1, h2);
/// ```
pub fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}

/// Hash a refresh token using SHA256.
///
/// Thin wrapper around [`hash_token`] kept for backward compatibility.
pub fn hash_refresh_token(raw_token: &str) -> String {
    hash_token(raw_token)
}

/// Generate an API key: returns (raw_key, key_hash)
///
/// Format: `strm_` prefix + 32 random hex chars = 37 chars total.
/// The prefix lets middleware distinguish API keys from JWTs.
pub fn generate_api_key() -> (String, String) {
    let bytes: [u8; 16] = os_random_bytes();
    let hex_part: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    let raw = format!("strm_{}", hex_part);
    let hash = hash_api_key(&raw);
    (raw, hash)
}

/// `N` bytes straight from the operating system's RNG, for tokens and keys.
/// Panics if the OS RNG fails, like `rand_core` 0.6's `OsRng::fill_bytes` did.
pub fn os_random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    UnwrapErr(SysRng).fill_bytes(&mut bytes);
    bytes
}

/// Hash an API key using SHA256.
///
/// Thin wrapper around [`hash_token`] kept for backward compatibility.
pub fn hash_api_key(raw_key: &str) -> String {
    hash_token(raw_key)
}

#[cfg(test)]
mod tests;
