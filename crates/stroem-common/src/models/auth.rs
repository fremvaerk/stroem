use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// User model (safe for client responses -- no password_hash)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub user_id: Uuid,
    pub name: Option<String>,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

/// JWT claims
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub email: String,
    pub exp: i64,
    pub iat: i64,
}
