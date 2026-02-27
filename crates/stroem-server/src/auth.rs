use anyhow::{Context, Result};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use sha2::{Digest, Sha256};
use stroem_common::models::auth::Claims;

/// Hash a password using argon2id
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
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
pub fn create_access_token(user_id: &str, email: &str, jwt_secret: &str) -> Result<String> {
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: user_id.to_string(),
        email: email.to_string(),
        iat: now,
        exp: now + 900, // 15 minutes
    };
    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
    .context("Failed to create access token")
}

/// Validate an access token and return claims
pub fn validate_access_token(token: &str, jwt_secret: &str) -> Result<Claims> {
    let token_data = jsonwebtoken::decode::<Claims>(
        token,
        &DecodingKey::from_secret(jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .context("Invalid access token")?;
    Ok(token_data.claims)
}

/// Generate a refresh token: returns (raw_token, token_hash)
pub fn generate_refresh_token() -> (String, String) {
    let raw = uuid::Uuid::new_v4().to_string();
    let hash = hash_refresh_token(&raw);
    (raw, hash)
}

/// Hash a refresh token using SHA256
pub fn hash_refresh_token(raw_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate an API key: returns (raw_key, key_hash)
///
/// Format: `strm_` prefix + 32 random hex chars = 37 chars total.
/// The prefix lets middleware distinguish API keys from JWTs.
pub fn generate_api_key() -> (String, String) {
    use argon2::password_hash::rand_core::RngCore;
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let hex_part: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    let raw = format!("strm_{}", hex_part);
    let hash = hash_api_key(&raw);
    (raw, hash)
}

/// Hash an API key using SHA256 (same algorithm as refresh tokens)
pub fn hash_api_key(raw_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_key.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_password_hash_and_verify_correct() {
        let password = "my-secure-password";
        let hash = hash_password(password).unwrap();
        assert!(verify_password(password, &hash).unwrap());
    }

    #[test]
    fn test_password_verify_wrong() {
        let hash = hash_password("correct-password").unwrap();
        assert!(!verify_password("wrong-password", &hash).unwrap());
    }

    #[test]
    fn test_password_different_salts() {
        let password = "same-password";
        let hash1 = hash_password(password).unwrap();
        let hash2 = hash_password(password).unwrap();
        assert_ne!(hash1, hash2);
        // Both still verify
        assert!(verify_password(password, &hash1).unwrap());
        assert!(verify_password(password, &hash2).unwrap());
    }

    #[test]
    fn test_jwt_create_and_validate() {
        let secret = "test-jwt-secret";
        let token = create_access_token("user-123", "test@example.com", secret).unwrap();
        let claims = validate_access_token(&token, secret).unwrap();
        assert_eq!(claims.sub, "user-123");
        assert_eq!(claims.email, "test@example.com");
    }

    #[test]
    fn test_jwt_wrong_secret_fails() {
        let token = create_access_token("user-123", "test@example.com", "secret-1").unwrap();
        let result = validate_access_token(&token, "secret-2");
        assert!(result.is_err());
    }

    #[test]
    fn test_refresh_token_uniqueness() {
        let (raw1, hash1) = generate_refresh_token();
        let (raw2, hash2) = generate_refresh_token();
        assert_ne!(raw1, raw2);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_refresh_token_hash_determinism() {
        let raw = "fixed-token-value";
        let hash1 = hash_refresh_token(raw);
        let hash2 = hash_refresh_token(raw);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_api_key_format() {
        let (raw, _hash) = generate_api_key();
        assert!(raw.starts_with("strm_"), "API key should start with strm_");
        assert_eq!(raw.len(), 37, "strm_ (5) + 32 hex chars = 37");
    }

    #[test]
    fn test_api_key_uniqueness() {
        let (raw1, hash1) = generate_api_key();
        let (raw2, hash2) = generate_api_key();
        assert_ne!(raw1, raw2);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_api_key_hash_determinism() {
        let raw = "strm_abcdef1234567890abcdef12345678";
        let hash1 = hash_api_key(raw);
        let hash2 = hash_api_key(raw);
        assert_eq!(hash1, hash2);
    }
}
