//! PKCE (RFC 7636) verifier ↔ challenge comparison.
//!
//! The flow:
//! 1. Client generates a random `code_verifier` (43–128 chars, URL-safe).
//! 2. Client sends `code_challenge = BASE64URL(SHA256(code_verifier))` to
//!    `/oauth/authorize` along with `code_challenge_method=S256`.
//! 3. Strøm stores the challenge alongside the issued authorization code.
//! 4. Client posts `code_verifier` to `/oauth/token`.
//! 5. We recompute `BASE64URL(SHA256(code_verifier))` and compare against
//!    the stored challenge in constant time.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Validate a PKCE verifier against the stored `S256` challenge.
///
/// Returns `true` iff `BASE64URL(SHA256(verifier))` equals `challenge`.
/// Uses constant-time comparison: a timing attack on the challenge byte
/// pattern would otherwise leak the SHA-256 hash and thus the verifier.
///
/// RFC 7636 §4.1 requires verifier length 43..=128 chars; we enforce that
/// upper bound here to prevent very long inputs from chewing CPU.
pub fn verify_s256(challenge: &str, verifier: &str) -> bool {
    if verifier.len() < 43 || verifier.len() > 128 {
        return false;
    }
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    let computed = URL_SAFE_NO_PAD.encode(digest);
    computed.as_bytes().ct_eq(challenge.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical RFC 7636 §A.1 example.
    #[test]
    fn test_s256_rfc7636_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_s256(challenge, verifier));
    }

    #[test]
    fn test_s256_wrong_verifier_rejected() {
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        // Same length, completely different bytes
        let bad = "a".repeat(43);
        assert!(!verify_s256(challenge, &bad));
    }

    #[test]
    fn test_s256_short_verifier_rejected() {
        // 42 chars — one below RFC 7636 minimum
        let short = "a".repeat(42);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(short.as_bytes()));
        assert!(!verify_s256(&challenge, &short));
    }

    #[test]
    fn test_s256_long_verifier_rejected() {
        // 129 chars — one above RFC 7636 maximum
        let long = "a".repeat(129);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(long.as_bytes()));
        assert!(!verify_s256(&challenge, &long));
    }
}
