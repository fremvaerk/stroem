use super::*;

#[test]
fn test_password_hash_and_verify_correct() {
    let password = "my-secure-password";
    let hash = hash_password(password).unwrap();
    assert!(verify_password(password, &hash).unwrap());
}

/// A hash written by argon2 0.5 (the version before the 0.6 upgrade) must
/// keep verifying: every stored `users.password_hash` predates it.
#[test]
fn test_password_verify_hash_from_argon2_0_5() {
    let legacy = "$argon2id$v=19$m=19456,t=2,p=1$rpPApaufAdPjvmGt0qEUWQ$s98vLtDoHJZMJ2jC6Ebn3TC+Eb38u9FhwdbnwiDzkrA";
    assert!(verify_password("legacy-argon2-0.5-password", legacy).unwrap());
    assert!(!verify_password("wrong-password", legacy).unwrap());
}

#[test]
fn test_os_random_bytes_are_not_constant() {
    let a: [u8; 32] = os_random_bytes();
    let b: [u8; 32] = os_random_bytes();
    assert_ne!(a, b);
    assert_ne!(a, [0u8; 32]);
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
    let token = create_access_token("user-123", "test@example.com", false, secret).unwrap();
    let claims = validate_access_token(&token, secret, None).unwrap();
    assert_eq!(claims.sub, "user-123");
    assert_eq!(claims.email, "test@example.com");
    assert!(!claims.is_admin);
}

#[test]
fn test_jwt_create_and_validate_admin() {
    let secret = "test-jwt-secret";
    let token = create_access_token("admin-1", "admin@example.com", true, secret).unwrap();
    let claims = validate_access_token(&token, secret, None).unwrap();
    assert_eq!(claims.sub, "admin-1");
    assert!(claims.is_admin);
}

#[test]
fn test_jwt_backward_compat_no_is_admin() {
    // Old JWTs without is_admin should deserialize with is_admin = false
    let secret = "test-jwt-secret";
    let now = chrono::Utc::now().timestamp();
    // Manually create a JWT without is_admin
    let claims_json = serde_json::json!({
        "sub": "user-1",
        "email": "old@example.com",
        "iat": now,
        "exp": now + 900,
    });
    let token = jsonwebtoken::encode(
        &Header::default(),
        &claims_json,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap();
    let claims = validate_access_token(&token, secret, None).unwrap();
    assert!(!claims.is_admin);
}

#[test]
fn test_jwt_wrong_secret_fails() {
    let token = create_access_token("user-123", "test@example.com", false, "secret-1").unwrap();
    let result = validate_access_token(&token, "secret-2", None);
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
fn test_hash_token_determinism() {
    let raw = "fixed-token-value";
    let hash1 = hash_token(raw);
    let hash2 = hash_token(raw);
    assert_eq!(hash1, hash2);
}

#[test]
fn test_hash_token_different_inputs_differ() {
    let h1 = hash_token("token-a");
    let h2 = hash_token("token-b");
    assert_ne!(h1, h2);
}

#[test]
fn test_hash_token_wrappers_agree() {
    let raw = "strm_abc123def456";
    // hash_refresh_token and hash_api_key must both delegate to hash_token
    assert_eq!(hash_token(raw), hash_refresh_token(raw));
    assert_eq!(hash_token(raw), hash_api_key(raw));
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
