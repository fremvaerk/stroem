use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConfig {
    pub url: String,
}

/// S3 configuration for log archival
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    #[serde(default)]
    pub prefix: String,
    pub endpoint: Option<String>,
}

/// Log storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogStorageConfig {
    pub local_dir: String,
    pub s3: Option<S3Config>,
}

/// Git auth configuration for workspace sources
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitAuthConfig {
    #[serde(rename = "type")]
    pub auth_type: String, // "ssh_key" | "token"
    pub key_path: Option<String>,
    pub token: Option<String>,
    pub username: Option<String>,
}

fn default_git_ref() -> String {
    "main".to_string()
}

fn default_poll_interval() -> u64 {
    60
}

/// Workspace source definition (folder or git)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkspaceSourceDef {
    #[serde(rename = "folder")]
    Folder { path: String },
    #[serde(rename = "git")]
    Git {
        url: String,
        #[serde(rename = "ref", default = "default_git_ref")]
        git_ref: String,
        #[serde(default = "default_poll_interval")]
        poll_interval_secs: u64,
        auth: Option<GitAuthConfig>,
    },
}

/// OIDC/internal provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_type: String, // "internal" or "oidc"
    pub display_name: Option<String>,
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
    pub base_url: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    pub initial_user: Option<InitialUserConfig>,
}

/// Recovery configuration for detecting stale workers and recovering stuck steps
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    /// Seconds without heartbeat before a worker is considered stale (default: 120)
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_secs: u64,
    /// How often the recovery sweeper runs in seconds (default: 60)
    #[serde(default = "default_sweep_interval")]
    pub sweep_interval_secs: u64,
}

fn default_heartbeat_timeout() -> u64 {
    120
}
fn default_sweep_interval() -> u64 {
    60
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            heartbeat_timeout_secs: 120,
            sweep_interval_secs: 60,
        }
    }
}

/// Server configuration - loaded from YAML
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listen: String, // "0.0.0.0:8080"
    pub db: DbConfig,
    pub log_storage: LogStorageConfig,
    #[serde(default)]
    pub workspaces: HashMap<String, WorkspaceSourceDef>,
    pub worker_token: String, // shared secret for worker auth
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub recovery: RecoveryConfig,
}

/// Load server config from a YAML file with STROEM__ env var overrides.
pub fn load_config(path: &str) -> anyhow::Result<ServerConfig> {
    use anyhow::Context;
    let config: ServerConfig = config::Config::builder()
        .add_source(config::File::new(path, config::FileFormat::Yaml))
        .add_source(
            config::Environment::with_prefix("STROEM")
                .prefix_separator("__")
                .separator("__"),
        )
        .build()
        .with_context(|| format!("Failed to build config from: {}", path))?
        .try_deserialize()
        .with_context(|| format!("Failed to deserialize config from: {}", path))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config_folder_workspace() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://user:pass@localhost:5432/stroem"
log_storage:
  local_dir: "/var/stroem/logs"
workspaces:
  default:
    type: folder
    path: "/var/stroem/workspace"
worker_token: "secret-token-123"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(config.listen, "0.0.0.0:8080");
        assert_eq!(config.db.url, "postgres://user:pass@localhost:5432/stroem");
        assert_eq!(config.log_storage.local_dir, "/var/stroem/logs");
        assert_eq!(config.workspaces.len(), 1);
        match &config.workspaces["default"] {
            WorkspaceSourceDef::Folder { path } => {
                assert_eq!(path, "/var/stroem/workspace");
            }
            _ => panic!("Expected folder workspace"),
        }
        assert_eq!(config.worker_token, "secret-token-123");
        assert!(config.auth.is_none());
    }

    #[test]
    fn test_parse_multiple_workspaces() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
  data-team:
    type: git
    url: "https://github.com/org/data-workflows.git"
    ref: main
    poll_interval_secs: 120
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(config.workspaces.len(), 2);
        match &config.workspaces["default"] {
            WorkspaceSourceDef::Folder { path } => assert_eq!(path, "./workspace"),
            _ => panic!("Expected folder"),
        }
        match &config.workspaces["data-team"] {
            WorkspaceSourceDef::Git {
                url,
                git_ref,
                poll_interval_secs,
                auth,
            } => {
                assert_eq!(url, "https://github.com/org/data-workflows.git");
                assert_eq!(git_ref, "main");
                assert_eq!(*poll_interval_secs, 120);
                assert!(auth.is_none());
            }
            _ => panic!("Expected git"),
        }
    }

    #[test]
    fn test_parse_git_workspace_with_auth() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  private:
    type: git
    url: "git@github.com:org/private.git"
    auth:
      type: ssh_key
      key_path: "/home/user/.ssh/id_rsa"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        match &config.workspaces["private"] {
            WorkspaceSourceDef::Git {
                url,
                git_ref,
                poll_interval_secs,
                auth,
            } => {
                assert_eq!(url, "git@github.com:org/private.git");
                assert_eq!(git_ref, "main"); // default
                assert_eq!(*poll_interval_secs, 60); // default
                let auth = auth.as_ref().unwrap();
                assert_eq!(auth.auth_type, "ssh_key");
                assert_eq!(auth.key_path.as_deref(), Some("/home/user/.ssh/id_rsa"));
            }
            _ => panic!("Expected git"),
        }
    }

    #[test]
    fn test_parse_git_workspace_with_token_auth() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  private:
    type: git
    url: "https://github.com/org/private.git"
    ref: develop
    auth:
      type: token
      token: "ghp_xxxx"
      username: "x-access-token"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        match &config.workspaces["private"] {
            WorkspaceSourceDef::Git { auth, git_ref, .. } => {
                assert_eq!(git_ref, "develop");
                let auth = auth.as_ref().unwrap();
                assert_eq!(auth.auth_type, "token");
                assert_eq!(auth.token.as_deref(), Some("ghp_xxxx"));
                assert_eq!(auth.username.as_deref(), Some("x-access-token"));
            }
            _ => panic!("Expected git"),
        }
    }

    #[test]
    fn test_parse_config_with_s3() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
  s3:
    bucket: "my-stroem-logs"
    region: "eu-west-1"
    prefix: "logs/"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        let s3 = config.log_storage.s3.unwrap();
        assert_eq!(s3.bucket, "my-stroem-logs");
        assert_eq!(s3.region, "eu-west-1");
        assert_eq!(s3.prefix, "logs/");
        assert!(s3.endpoint.is_none());
    }

    #[test]
    fn test_parse_config_without_s3() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert!(config.log_storage.s3.is_none());
    }

    #[test]
    fn test_parse_config_s3_with_endpoint() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
  s3:
    bucket: "stroem-logs"
    region: "us-east-1"
    endpoint: "http://minio:9000"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        let s3 = config.log_storage.s3.unwrap();
        assert_eq!(s3.bucket, "stroem-logs");
        assert_eq!(s3.region, "us-east-1");
        assert_eq!(s3.prefix, ""); // default
        assert_eq!(s3.endpoint.as_deref(), Some("http://minio:9000"));
    }

    #[test]
    fn test_parse_config_with_auth() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
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
workspaces:
  default:
    type: folder
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

    #[test]
    fn test_parse_missing_workspaces_field_defaults_to_empty() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert!(
            config.workspaces.is_empty(),
            "Config without workspaces field should default to empty"
        );
    }

    #[test]
    fn test_parse_empty_workspaces_succeeds() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {}
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert!(config.workspaces.is_empty());
    }

    #[test]
    fn test_parse_unknown_workspace_type_fails() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: s3
    bucket: my-bucket
worker_token: "token"
"#;
        let result = serde_yml::from_str::<ServerConfig>(yaml);
        assert!(result.is_err(), "Unknown workspace type should fail");
    }

    #[test]
    fn test_parse_missing_db_url_fails() {
        let yaml = r#"
listen: "0.0.0.0:8080"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
"#;
        let result = serde_yml::from_str::<ServerConfig>(yaml);
        assert!(result.is_err(), "Config without db section should fail");
    }

    #[test]
    fn test_parse_missing_worker_token_fails() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
"#;
        let result = serde_yml::from_str::<ServerConfig>(yaml);
        assert!(result.is_err(), "Config without worker_token should fail");
    }

    #[test]
    fn test_parse_git_workspace_defaults() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  repo:
    type: git
    url: "https://github.com/org/repo.git"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        match &config.workspaces["repo"] {
            WorkspaceSourceDef::Git {
                url,
                git_ref,
                poll_interval_secs,
                auth,
            } => {
                assert_eq!(url, "https://github.com/org/repo.git");
                assert_eq!(git_ref, "main");
                assert_eq!(*poll_interval_secs, 60);
                assert!(auth.is_none());
            }
            _ => panic!("Expected git workspace"),
        }
    }

    #[test]
    fn test_parse_git_workspace_missing_url_fails() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  repo:
    type: git
    ref: main
worker_token: "token"
"#;
        let result = serde_yml::from_str::<ServerConfig>(yaml);
        assert!(result.is_err(), "Git workspace without url should fail");
    }

    #[test]
    fn test_parse_folder_workspace_missing_path_fails() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
worker_token: "token"
"#;
        let result = serde_yml::from_str::<ServerConfig>(yaml);
        assert!(result.is_err(), "Folder workspace without path should fail");
    }

    #[test]
    fn test_parse_git_auth_ssh_key_without_key_path() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  private:
    type: git
    url: "git@github.com:org/private.git"
    auth:
      type: ssh_key
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        match &config.workspaces["private"] {
            WorkspaceSourceDef::Git { auth, .. } => {
                let auth = auth.as_ref().unwrap();
                assert_eq!(auth.auth_type, "ssh_key");
                assert!(auth.key_path.is_none());
            }
            _ => panic!("Expected git workspace"),
        }
    }

    #[test]
    fn test_parse_git_auth_token_without_username() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  private:
    type: git
    url: "https://github.com/org/private.git"
    auth:
      type: token
      token: "ghp_xxxx"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        match &config.workspaces["private"] {
            WorkspaceSourceDef::Git { auth, .. } => {
                let auth = auth.as_ref().unwrap();
                assert_eq!(auth.auth_type, "token");
                assert_eq!(auth.token.as_deref(), Some("ghp_xxxx"));
                assert!(auth.username.is_none());
            }
            _ => panic!("Expected git workspace"),
        }
    }

    #[test]
    fn test_parse_config_with_many_workspaces() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  local-dev:
    type: folder
    path: "/opt/workflows"
  data-team:
    type: git
    url: "https://github.com/org/data-workflows.git"
    ref: main
  ml-team:
    type: git
    url: "git@github.com:org/ml-workflows.git"
    ref: production
    poll_interval_secs: 300
    auth:
      type: ssh_key
      key_path: "/keys/ml-deploy"
  infra-team:
    type: git
    url: "https://gitlab.com/org/infra-workflows.git"
    ref: stable
    auth:
      type: token
      token: "glpat-xxxx"
      username: "oauth2"
  scratch:
    type: folder
    path: "/tmp/workflows"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(config.workspaces.len(), 5);

        match &config.workspaces["local-dev"] {
            WorkspaceSourceDef::Folder { path } => assert_eq!(path, "/opt/workflows"),
            _ => panic!("Expected folder"),
        }

        match &config.workspaces["data-team"] {
            WorkspaceSourceDef::Git {
                url, git_ref, auth, ..
            } => {
                assert_eq!(url, "https://github.com/org/data-workflows.git");
                assert_eq!(git_ref, "main");
                assert!(auth.is_none());
            }
            _ => panic!("Expected git"),
        }

        match &config.workspaces["ml-team"] {
            WorkspaceSourceDef::Git {
                url,
                git_ref,
                poll_interval_secs,
                auth,
            } => {
                assert_eq!(url, "git@github.com:org/ml-workflows.git");
                assert_eq!(git_ref, "production");
                assert_eq!(*poll_interval_secs, 300);
                let auth = auth.as_ref().unwrap();
                assert_eq!(auth.auth_type, "ssh_key");
                assert_eq!(auth.key_path.as_deref(), Some("/keys/ml-deploy"));
            }
            _ => panic!("Expected git"),
        }

        match &config.workspaces["infra-team"] {
            WorkspaceSourceDef::Git { auth, .. } => {
                let auth = auth.as_ref().unwrap();
                assert_eq!(auth.auth_type, "token");
                assert_eq!(auth.token.as_deref(), Some("glpat-xxxx"));
                assert_eq!(auth.username.as_deref(), Some("oauth2"));
            }
            _ => panic!("Expected git"),
        }

        match &config.workspaces["scratch"] {
            WorkspaceSourceDef::Folder { path } => assert_eq!(path, "/tmp/workflows"),
            _ => panic!("Expected folder"),
        }
    }

    #[test]
    fn test_parse_config_with_oidc_provider() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
auth:
  jwt_secret: "secret"
  refresh_secret: "refresh"
  base_url: "https://stroem.company.com"
  providers:
    internal:
      provider_type: "internal"
    google:
      provider_type: "oidc"
      display_name: "Google"
      issuer_url: "https://accounts.google.com"
      client_id: "123456.apps.googleusercontent.com"
      client_secret: "GOCSPX-secret"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        let auth = config.auth.unwrap();
        assert_eq!(auth.base_url.as_deref(), Some("https://stroem.company.com"));
        assert_eq!(auth.providers.len(), 2);

        let internal = &auth.providers["internal"];
        assert_eq!(internal.provider_type, "internal");

        let google = &auth.providers["google"];
        assert_eq!(google.provider_type, "oidc");
        assert_eq!(google.display_name.as_deref(), Some("Google"));
        assert_eq!(
            google.issuer_url.as_deref(),
            Some("https://accounts.google.com")
        );
        assert_eq!(
            google.client_id.as_deref(),
            Some("123456.apps.googleusercontent.com")
        );
        assert_eq!(google.client_secret.as_deref(), Some("GOCSPX-secret"));
    }

    #[test]
    fn test_parse_config_recovery_defaults() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(config.recovery.heartbeat_timeout_secs, 120);
        assert_eq!(config.recovery.sweep_interval_secs, 60);
    }

    #[test]
    fn test_parse_config_recovery_custom() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
recovery:
  heartbeat_timeout_secs: 300
  sweep_interval_secs: 30
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(config.recovery.heartbeat_timeout_secs, 300);
        assert_eq!(config.recovery.sweep_interval_secs, 30);
    }

    #[test]
    fn test_parse_config_recovery_partial() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
recovery:
  heartbeat_timeout_secs: 180
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(config.recovery.heartbeat_timeout_secs, 180);
        assert_eq!(config.recovery.sweep_interval_secs, 60); // default
    }

    #[test]
    fn test_parse_config_oidc_display_name_defaults() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "token"
auth:
  jwt_secret: "secret"
  refresh_secret: "refresh"
  providers:
    google:
      provider_type: "oidc"
      issuer_url: "https://accounts.google.com"
      client_id: "id"
      client_secret: "secret"
"#;
        let config: ServerConfig = serde_yml::from_str(yaml).unwrap();
        let auth = config.auth.unwrap();
        let google = &auth.providers["google"];
        // display_name is optional (defaults to None, handled at runtime)
        assert!(google.display_name.is_none());
        // base_url is optional
        assert!(auth.base_url.is_none());
    }

    /// Serialize access to env vars in tests to avoid races between parallel tests
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_env_override_db_url() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://placeholder:5432/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {}
worker_token: "yaml-token"
"#;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, yaml.as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        // SAFETY: test-only, serialized by ENV_MUTEX
        unsafe {
            std::env::set_var("STROEM__DB__URL", "postgres://overridden:5432/stroem");
            std::env::set_var("STROEM__WORKER_TOKEN", "env-token");
        }

        let config = load_config(file.path().to_str().unwrap()).unwrap();

        unsafe {
            std::env::remove_var("STROEM__DB__URL");
            std::env::remove_var("STROEM__WORKER_TOKEN");
        }

        assert_eq!(config.db.url, "postgres://overridden:5432/stroem");
        assert_eq!(config.worker_token, "env-token");
        // Non-overridden values preserved from YAML
        assert_eq!(config.listen, "0.0.0.0:8080");
        assert_eq!(config.log_storage.local_dir, "./logs");
    }

    #[test]
    fn test_env_override_listen() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost:5432/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {}
worker_token: "token"
"#;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, yaml.as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        // SAFETY: test-only, serialized by ENV_MUTEX
        unsafe {
            std::env::set_var("STROEM__LISTEN", "0.0.0.0:9090");
        }

        let config = load_config(file.path().to_str().unwrap()).unwrap();

        unsafe {
            std::env::remove_var("STROEM__LISTEN");
        }

        assert_eq!(config.listen, "0.0.0.0:9090");
    }
}
