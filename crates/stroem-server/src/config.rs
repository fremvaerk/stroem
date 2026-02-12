use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConfig {
    pub url: String,
}

/// Log storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogStorageConfig {
    pub local_dir: String,
}

/// Workspace source configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSourceConfig {
    #[serde(rename = "type")]
    pub source_type: String, // "folder"
    pub path: String,
}

/// OIDC/internal provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_type: String, // "internal" or "oidc"
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

/// Initial user to seed on startup
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitialUserConfig {
    pub email: String,
    pub password: String,
}

/// Auth configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub refresh_secret: String,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    pub initial_user: Option<InitialUserConfig>,
}

/// Server configuration - loaded from YAML
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listen: String, // "0.0.0.0:8080"
    pub db: DbConfig,
    pub log_storage: LogStorageConfig,
    pub workspace: WorkspaceSourceConfig,
    pub worker_token: String, // shared secret for worker auth
    pub auth: Option<AuthConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://user:pass@localhost:5432/stroem"
log_storage:
  local_dir: "/var/stroem/logs"
workspace:
  type: "folder"
  path: "/var/stroem/workspace"
worker_token: "secret-token-123"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(config.listen, "0.0.0.0:8080");
        assert_eq!(config.db.url, "postgres://user:pass@localhost:5432/stroem");
        assert_eq!(config.log_storage.local_dir, "/var/stroem/logs");
        assert_eq!(config.workspace.source_type, "folder");
        assert_eq!(config.workspace.path, "/var/stroem/workspace");
        assert_eq!(config.worker_token, "secret-token-123");
        assert!(config.auth.is_none());
    }

    #[test]
    fn test_parse_minimal_config() {
        let yaml = r#"
listen: "127.0.0.1:3000"
db:
  url: "postgres://localhost/test"
log_storage:
  local_dir: "./logs"
workspace:
  type: "folder"
  path: "./workspace"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(config.listen, "127.0.0.1:3000");
        assert_eq!(config.worker_token, "token");
        assert!(config.auth.is_none());
    }

    #[test]
    fn test_parse_config_with_auth() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspace:
  type: "folder"
  path: "./workspace"
worker_token: "token"
auth:
  jwt_secret: "my-jwt-secret"
  refresh_secret: "my-refresh-secret"
  providers:
    internal:
      provider_type: "internal"
  initial_user:
    email: "admin@example.com"
    password: "changeme"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        let auth = config.auth.unwrap();
        assert_eq!(auth.jwt_secret, "my-jwt-secret");
        assert_eq!(auth.refresh_secret, "my-refresh-secret");
        assert!(auth.providers.contains_key("internal"));
        assert_eq!(auth.providers["internal"].provider_type, "internal");
        let initial = auth.initial_user.unwrap();
        assert_eq!(initial.email, "admin@example.com");
        assert_eq!(initial.password, "changeme");
    }

    #[test]
    fn test_parse_config_with_auth_no_initial_user() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspace:
  type: "folder"
  path: "./workspace"
worker_token: "token"
auth:
  jwt_secret: "secret"
  refresh_secret: "refresh"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        let auth = config.auth.unwrap();
        assert_eq!(auth.jwt_secret, "secret");
        assert!(auth.initial_user.is_none());
        assert!(auth.providers.is_empty());
    }
}
