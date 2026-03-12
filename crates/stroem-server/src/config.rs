use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    pub url: String,
}

/// S3 configuration for log archival
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    #[serde(default)]
    pub prefix: String,
    pub endpoint: Option<String>,
}

/// Log storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogStorageConfig {
    pub local_dir: String,
    pub s3: Option<S3Config>,
}

/// Git auth configuration for workspace sources
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitAuthConfig {
    #[serde(rename = "type")]
    pub auth_type: String, // "ssh_key" | "token"
    pub key_path: Option<String>,
    pub key: Option<String>,
    pub token: Option<String>,
    pub username: Option<String>,
}

fn default_git_ref() -> String {
    "main".to_string()
}

/// Library source definition — shared action/task/connection-type libraries
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LibraryDef {
    #[serde(rename = "folder")]
    Folder { path: String },
    #[serde(rename = "git")]
    Git {
        url: String,
        #[serde(rename = "ref", default = "default_git_ref")]
        git_ref: String,
        /// References a named entry in `git_auth` (server config level)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<String>,
    },
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
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub provider_type: String, // "internal" or "oidc"
    pub display_name: Option<String>,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

/// Initial user to seed on startup
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialUserConfig {
    pub email: String,
    pub password: String,
}

/// Auth configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub refresh_secret: String,
    pub base_url: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    pub initial_user: Option<InitialUserConfig>,
}

/// ACL action: what a matching rule grants
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AclAction {
    Run,
    View,
    Deny,
}

impl AclAction {
    /// Numeric priority for highest-wins evaluation (higher = more permissive)
    pub fn priority(self) -> u8 {
        match self {
            AclAction::Deny => 1,
            AclAction::View => 2,
            AclAction::Run => 3,
        }
    }
}

/// A single ACL rule
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AclRule {
    /// Workspace name or "*" for all
    pub workspace: String,
    /// Glob patterns on task names (e.g. ["deploy/*", "build"])
    #[serde(default = "default_wildcard_vec")]
    pub tasks: Vec<String>,
    /// Permission to grant
    pub action: AclAction,
    /// Groups that this rule applies to (OR'd with users)
    #[serde(default)]
    pub groups: Vec<String>,
    /// User emails that this rule applies to (OR'd with groups)
    #[serde(default)]
    pub users: Vec<String>,
}

fn default_wildcard_vec() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_acl_action() -> AclAction {
    AclAction::Deny
}

/// ACL configuration section in server-config.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AclConfig {
    /// Default action when no rule matches (default: deny)
    #[serde(default = "default_acl_action")]
    pub default: AclAction,
    /// ACL rules — all matching rules are checked, highest permission wins
    #[serde(default)]
    pub rules: Vec<AclRule>,
}

/// MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// Whether the MCP endpoint is enabled (default: false)
    #[serde(default)]
    pub enabled: bool,
}

/// Recovery configuration for detecting stale workers and recovering stuck steps
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryConfig {
    /// Seconds without heartbeat before a worker is considered stale (default: 120)
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_secs: u64,
    /// How often the recovery sweeper runs in seconds (default: 60)
    #[serde(default = "default_sweep_interval")]
    pub sweep_interval_secs: u64,
    /// Seconds a ready step can wait without a matching worker before being failed (default: 30)
    #[serde(default = "default_unmatched_step_timeout")]
    pub unmatched_step_timeout_secs: u64,
}

fn default_heartbeat_timeout() -> u64 {
    120
}
fn default_sweep_interval() -> u64 {
    60
}
fn default_unmatched_step_timeout() -> u64 {
    30
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            heartbeat_timeout_secs: 120,
            sweep_interval_secs: 60,
            unmatched_step_timeout_secs: 30,
        }
    }
}

/// Server configuration - loaded from YAML
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String, // "0.0.0.0:8080"
    pub db: DbConfig,
    pub log_storage: LogStorageConfig,
    #[serde(default)]
    pub workspaces: HashMap<String, WorkspaceSourceDef>,
    /// Shared action/task/connection-type libraries available to all workspaces
    #[serde(default)]
    pub libraries: HashMap<String, LibraryDef>,
    /// Named git auth configs referenced by library `auth` field
    #[serde(default)]
    pub git_auth: HashMap<String, GitAuthConfig>,
    pub worker_token: String, // shared secret for worker auth
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub recovery: RecoveryConfig,
    /// ACL configuration (optional — when absent, all authenticated users have full access)
    pub acl: Option<AclConfig>,
    /// MCP server endpoint configuration (optional)
    pub mcp: Option<McpConfig>,
}

impl ServerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.worker_token.len() < 32 {
            anyhow::bail!("worker_token must be at least 32 characters");
        }
        if let Some(ref auth) = self.auth {
            if auth.jwt_secret.len() < 32 {
                anyhow::bail!("jwt_secret must be at least 32 characters");
            }
            if auth.refresh_secret.len() < 32 {
                anyhow::bail!("refresh_secret must be at least 32 characters");
            }
        }
        if self.recovery.heartbeat_timeout_secs < 10 {
            anyhow::bail!("heartbeat_timeout_secs must be at least 10");
        }
        if self.recovery.sweep_interval_secs < 5 {
            anyhow::bail!("sweep_interval_secs must be at least 5");
        }
        if self.recovery.unmatched_step_timeout_secs < 5 {
            anyhow::bail!("unmatched_step_timeout_secs must be at least 5");
        }
        Ok(())
    }
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
    config.validate()?;
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let result = serde_yaml::from_str::<ServerConfig>(yaml);
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
        let result = serde_yaml::from_str::<ServerConfig>(yaml);
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
        let result = serde_yaml::from_str::<ServerConfig>(yaml);
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let result = serde_yaml::from_str::<ServerConfig>(yaml);
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
        let result = serde_yaml::from_str::<ServerConfig>(yaml);
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.recovery.heartbeat_timeout_secs, 120);
        assert_eq!(config.recovery.sweep_interval_secs, 60);
        assert_eq!(config.recovery.unmatched_step_timeout_secs, 30);
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
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
worker_token: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, yaml.as_bytes()).unwrap();
        std::io::Write::flush(&mut file).unwrap();

        // SAFETY: test-only, serialized by ENV_MUTEX
        unsafe {
            std::env::set_var("STROEM__DB__URL", "postgres://overridden:5432/stroem");
            std::env::set_var(
                "STROEM__WORKER_TOKEN",
                "env-token-that-is-long-enough-for-validation",
            );
        }

        let config = load_config(file.path().to_str().unwrap()).unwrap();

        unsafe {
            std::env::remove_var("STROEM__DB__URL");
            std::env::remove_var("STROEM__WORKER_TOKEN");
        }

        assert_eq!(config.db.url, "postgres://overridden:5432/stroem");
        assert_eq!(
            config.worker_token,
            "env-token-that-is-long-enough-for-validation"
        );
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
worker_token: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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

    #[test]
    fn test_parse_config_with_folder_library() {
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
libraries:
  local-lib:
    type: folder
    path: "/shared/stroem-libraries/notifications"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.libraries.len(), 1);
        match &config.libraries["local-lib"] {
            LibraryDef::Folder { path } => {
                assert_eq!(path, "/shared/stroem-libraries/notifications");
            }
            _ => panic!("Expected folder library"),
        }
    }

    #[test]
    fn test_parse_config_with_git_library() {
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
libraries:
  common:
    type: git
    url: https://github.com/org/stroem-common-library.git
    ref: v1.2.0
    auth: my-git-token
git_auth:
  my-git-token:
    type: token
    token: "ghp_xxxx"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.libraries.len(), 1);
        match &config.libraries["common"] {
            LibraryDef::Git { url, git_ref, auth } => {
                assert_eq!(url, "https://github.com/org/stroem-common-library.git");
                assert_eq!(git_ref, "v1.2.0");
                assert_eq!(auth.as_deref(), Some("my-git-token"));
            }
            _ => panic!("Expected git library"),
        }
        assert_eq!(config.git_auth.len(), 1);
        let auth = &config.git_auth["my-git-token"];
        assert_eq!(auth.auth_type, "token");
        assert_eq!(auth.token.as_deref(), Some("ghp_xxxx"));
    }

    #[test]
    fn test_parse_config_git_library_defaults() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {}
libraries:
  common:
    type: git
    url: https://github.com/org/lib.git
worker_token: "token"
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        match &config.libraries["common"] {
            LibraryDef::Git { url, git_ref, auth } => {
                assert_eq!(url, "https://github.com/org/lib.git");
                assert_eq!(git_ref, "main"); // default
                assert!(auth.is_none());
            }
            _ => panic!("Expected git library"),
        }
    }

    #[test]
    fn test_parse_config_no_libraries_defaults_empty() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {}
worker_token: "token"
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.libraries.is_empty());
        assert!(config.git_auth.is_empty());
    }

    fn valid_32char_token() -> &'static str {
        "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"
    }

    #[test]
    fn test_server_config_deny_unknown_fields() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {}
worker_token: "token"
unknown_field: "should fail"
"#;
        let result = serde_yaml::from_str::<ServerConfig>(yaml);
        assert!(result.is_err(), "Unknown fields should be rejected");
    }

    #[test]
    fn test_server_config_validate_short_worker_token() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {}
worker_token: "short"
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("worker_token"),
            "Error should mention worker_token: {}",
            err
        );
    }

    #[test]
    fn test_server_config_validate_short_jwt_secret() {
        let token = valid_32char_token();
        let yaml = format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {{}}
worker_token: "{token}"
auth:
  jwt_secret: "short"
  refresh_secret: "{token}"
"#
        );
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("jwt_secret"),
            "Error should mention jwt_secret: {}",
            err
        );
    }

    #[test]
    fn test_server_config_validate_short_refresh_secret() {
        let token = valid_32char_token();
        let yaml = format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {{}}
worker_token: "{token}"
auth:
  jwt_secret: "{token}"
  refresh_secret: "short"
"#
        );
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("refresh_secret"),
            "Error should mention refresh_secret: {}",
            err
        );
    }

    #[test]
    fn test_server_config_validate_valid() {
        let token = valid_32char_token();
        let yaml = format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {{}}
worker_token: "{token}"
auth:
  jwt_secret: "{token}"
  refresh_secret: "{token}"
"#
        );
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_parse_config_multiple_libraries() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {}
libraries:
  common:
    type: git
    url: https://github.com/org/common.git
    ref: v1.0.0
  local-lib:
    type: folder
    path: /shared/libs
  infra:
    type: git
    url: https://github.com/org/infra.git
    ref: main
    auth: gh-token
git_auth:
  gh-token:
    type: token
    token: "ghp_xxxx"
    username: "x-access-token"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.libraries.len(), 3);
        assert!(matches!(
            &config.libraries["common"],
            LibraryDef::Git { .. }
        ));
        assert!(matches!(
            &config.libraries["local-lib"],
            LibraryDef::Folder { .. }
        ));
        assert!(matches!(&config.libraries["infra"], LibraryDef::Git { .. }));
        assert_eq!(config.git_auth.len(), 1);
    }

    // ─── Boundary validation tests ────────────────────────────────────────────

    fn make_valid_server_config(worker_token: &str) -> ServerConfig {
        let yaml = format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {{}}
worker_token: "{worker_token}"
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    fn make_valid_server_config_with_auth(worker_token: &str, jwt_secret: &str) -> ServerConfig {
        let yaml = format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {{}}
worker_token: "{worker_token}"
auth:
  jwt_secret: "{jwt_secret}"
  refresh_secret: "{worker_token}"
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    fn make_valid_server_config_with_recovery(
        heartbeat_timeout_secs: u64,
        sweep_interval_secs: u64,
    ) -> ServerConfig {
        let token = valid_32char_token();
        let yaml = format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {{}}
worker_token: "{token}"
recovery:
  heartbeat_timeout_secs: {heartbeat_timeout_secs}
  sweep_interval_secs: {sweep_interval_secs}
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn test_validate_worker_token_exactly_32_chars() {
        // Exactly 32 characters — must pass
        let token: String = "a".repeat(32);
        let config = make_valid_server_config(&token);
        assert!(
            config.validate().is_ok(),
            "32-char worker_token should pass validation"
        );
    }

    #[test]
    fn test_validate_worker_token_31_chars_fails() {
        // 31 characters — must fail (below the 32-char minimum)
        let token: String = "a".repeat(31);
        let config = make_valid_server_config(&token);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("worker_token"),
            "Error should mention worker_token: {}",
            err
        );
    }

    #[test]
    fn test_validate_jwt_secret_exactly_32_chars() {
        // Exactly 32 characters — must pass
        let token = valid_32char_token();
        let jwt: String = "b".repeat(32);
        let config = make_valid_server_config_with_auth(token, &jwt);
        assert!(
            config.validate().is_ok(),
            "32-char jwt_secret should pass validation"
        );
    }

    #[test]
    fn test_validate_jwt_secret_31_chars_fails() {
        // 31 characters — must fail
        let token = valid_32char_token();
        let jwt: String = "b".repeat(31);
        let config = make_valid_server_config_with_auth(token, &jwt);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("jwt_secret"),
            "Error should mention jwt_secret: {}",
            err
        );
    }

    #[test]
    fn test_validate_heartbeat_timeout_exactly_10() {
        // heartbeat_timeout_secs = 10 (the minimum) — must pass
        let config = make_valid_server_config_with_recovery(10, 5);
        assert!(
            config.validate().is_ok(),
            "heartbeat_timeout_secs = 10 should pass validation"
        );
    }

    #[test]
    fn test_validate_heartbeat_timeout_9_fails() {
        // heartbeat_timeout_secs = 9 (below the minimum of 10) — must fail
        let config = make_valid_server_config_with_recovery(9, 5);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("heartbeat_timeout_secs"),
            "Error should mention heartbeat_timeout_secs: {}",
            err
        );
    }

    #[test]
    fn test_validate_sweep_interval_exactly_5() {
        // sweep_interval_secs = 5 (the minimum) — must pass
        let config = make_valid_server_config_with_recovery(10, 5);
        assert!(
            config.validate().is_ok(),
            "sweep_interval_secs = 5 should pass validation"
        );
    }

    #[test]
    fn test_validate_sweep_interval_4_fails() {
        // sweep_interval_secs = 4 (below the minimum of 5) — must fail
        let config = make_valid_server_config_with_recovery(10, 4);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("sweep_interval_secs"),
            "Error should mention sweep_interval_secs: {}",
            err
        );
    }

    fn make_valid_server_config_with_unmatched_timeout(
        unmatched_step_timeout_secs: u64,
    ) -> ServerConfig {
        let token = valid_32char_token();
        let yaml = format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces: {{}}
worker_token: "{token}"
recovery:
  heartbeat_timeout_secs: 120
  sweep_interval_secs: 60
  unmatched_step_timeout_secs: {unmatched_step_timeout_secs}
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn test_validate_unmatched_step_timeout_exactly_5() {
        let config = make_valid_server_config_with_unmatched_timeout(5);
        assert!(
            config.validate().is_ok(),
            "unmatched_step_timeout_secs = 5 should pass validation"
        );
    }

    #[test]
    fn test_validate_unmatched_step_timeout_4_fails() {
        let config = make_valid_server_config_with_unmatched_timeout(4);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("unmatched_step_timeout_secs"),
            "Error should mention unmatched_step_timeout_secs: {}",
            err
        );
    }

    #[test]
    fn test_parse_config_without_acl() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.acl.is_none());
    }

    #[test]
    fn test_parse_config_with_acl() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
acl:
  default: deny
  rules:
    - workspace: production
      tasks: ["deploy/*"]
      action: run
      groups: [devops]
      users: [contractor@ext.com]
    - workspace: "*"
      tasks: ["*"]
      action: view
      groups: [engineering]
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let acl = config.acl.unwrap();
        assert!(matches!(acl.default, AclAction::Deny));
        assert_eq!(acl.rules.len(), 2);

        let r0 = &acl.rules[0];
        assert_eq!(r0.workspace, "production");
        assert_eq!(r0.tasks, vec!["deploy/*"]);
        assert!(matches!(r0.action, AclAction::Run));
        assert_eq!(r0.groups, vec!["devops"]);
        assert_eq!(r0.users, vec!["contractor@ext.com"]);

        let r1 = &acl.rules[1];
        assert_eq!(r1.workspace, "*");
        assert_eq!(r1.tasks, vec!["*"]);
        assert!(matches!(r1.action, AclAction::View));
        assert_eq!(r1.groups, vec!["engineering"]);
        assert!(r1.users.is_empty());
    }

    #[test]
    fn test_parse_acl_rule_defaults() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
acl:
  rules:
    - workspace: "*"
      action: view
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let acl = config.acl.unwrap();
        // default action defaults to deny
        assert!(matches!(acl.default, AclAction::Deny));
        let rule = &acl.rules[0];
        // tasks defaults to ["*"]
        assert_eq!(rule.tasks, vec!["*"]);
        // groups and users default to empty
        assert!(rule.groups.is_empty());
        assert!(rule.users.is_empty());
    }

    #[test]
    fn test_parse_acl_empty_rules() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
acl:
  default: view
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let acl = config.acl.unwrap();
        assert!(matches!(acl.default, AclAction::View));
        assert!(acl.rules.is_empty());
    }

    #[test]
    fn test_acl_action_priority() {
        assert_eq!(AclAction::Deny.priority(), 1);
        assert_eq!(AclAction::View.priority(), 2);
        assert_eq!(AclAction::Run.priority(), 3);
    }

    // ─── McpConfig parsing tests ──────────────────────────────────────────────

    #[test]
    fn test_parse_mcp_config_enabled() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
mcp:
  enabled: true
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let mcp = config.mcp.expect("mcp section should be present");
        assert!(mcp.enabled);
    }

    #[test]
    fn test_parse_mcp_config_disabled() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
mcp:
  enabled: false
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let mcp = config.mcp.expect("mcp section should be present");
        assert!(!mcp.enabled);
    }

    #[test]
    fn test_parse_mcp_absent_defaults_to_none() {
        // When the `mcp` section is omitted, `ServerConfig.mcp` should be None.
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
"#;
        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.mcp.is_none());
    }

    #[test]
    fn test_parse_mcp_config_unknown_field_rejected() {
        // `McpConfig` has `#[serde(deny_unknown_fields)]`, so extra fields must fail.
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
worker_token: "token"
mcp:
  enabled: true
  unknown_key: "should fail"
"#;
        let result = serde_yaml::from_str::<ServerConfig>(yaml);
        assert!(
            result.is_err(),
            "Unknown fields in mcp section should be rejected"
        );
    }
}
