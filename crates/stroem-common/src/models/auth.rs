use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// User model (safe for client responses -- no password_hash)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub user_id: Uuid,
    pub name: Option<String>,
    pub email: String,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
}

/// JWT claims.
///
/// The first five fields are present in every token Strøm issues. The optional
/// fields are populated only by tokens minted through the OAuth 2.1 flow at
/// `/oauth/token`; login-flow JWTs and API-key shims leave them `None`. The
/// MCP middleware uses `aud` to verify audience-bound tokens are intended for
/// `/mcp` and rejects mis-targeted ones.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub email: String,
    #[serde(default)]
    pub is_admin: bool,
    pub exp: i64,
    pub iat: i64,

    /// Audience — RFC 7519 §4.1.3. For OAuth-issued MCP tokens this is the
    /// canonical resource URL (e.g. `https://stroem.example.com/mcp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,

    /// Issuer — RFC 7519 §4.1.1. Canonical AS origin for OAuth-issued tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,

    /// Space-separated granted scopes (`"mcp"` today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,

    /// OAuth client_id that requested the token. Useful for audit logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}
