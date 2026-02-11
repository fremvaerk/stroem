use serde::{Deserialize, Serialize};

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

/// Server configuration - loaded from YAML
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listen: String, // "0.0.0.0:8080"
    pub db: DbConfig,
    pub log_storage: LogStorageConfig,
    pub workspace: WorkspaceSourceConfig,
    pub worker_token: String, // shared secret for worker auth
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
    }
}
