use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use stroem_common::duration::HumanDuration;
use stroem_common::validation::{MAX_JOB_TIMEOUT_SECS, MAX_STEP_TIMEOUT_SECS};

/// Database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    pub url: String,
}

/// S3 configuration for log archival (legacy format, still supported)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    #[serde(default)]
    pub prefix: String,
    pub endpoint: Option<String>,
}

/// Archive backend configuration.
///
/// Flat struct with `archive_type` discriminator + optional fields per backend.
/// Using a flat struct instead of `#[serde(tag = "type")]` enum to avoid issues
/// with the `config` crate's env var override mechanism.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveConfig {
    /// Backend type: `"s3"` or `"local"`
    #[serde(rename = "type")]
    pub archive_type: String,

    // --- S3 fields ---
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub endpoint: Option<String>,

    // --- Local fields ---
    pub path: Option<String>,

    // --- Shared ---
    #[serde(default)]
    pub prefix: String,
}

/// Log storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogStorageConfig {
    pub local_dir: String,
    /// Legacy S3 config — use `archive` instead for new deployments.
    pub s3: Option<S3Config>,
    /// New pluggable archive config (takes precedence over `s3`).
    #[serde(default)]
    pub archive: Option<ArchiveConfig>,
}

impl LogStorageConfig {
    /// Resolve the effective archive configuration.
    ///
    /// `archive` takes precedence over legacy `s3`. Returns `None` if neither is set.
    pub fn effective_archive(&self) -> Option<ArchiveConfig> {
        if let Some(ref archive) = self.archive {
            return Some(archive.clone());
        }
        // Convert legacy s3 config to ArchiveConfig
        self.s3.as_ref().map(|s3| ArchiveConfig {
            archive_type: "s3".to_string(),
            bucket: Some(s3.bucket.clone()),
            region: Some(s3.region.clone()),
            endpoint: s3.endpoint.clone(),
            path: None,
            prefix: s3.prefix.clone(),
        })
    }
}

/// Task state snapshot storage configuration (optional)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateStorageConfig {
    /// Key prefix for state objects
    #[serde(default = "default_state_prefix")]
    pub prefix: String,
    /// Max snapshots to retain per workspace + task
    #[serde(default = "default_max_snapshots")]
    pub max_snapshots: usize,
    /// Max snapshots for global workspace state. Defaults to `max_snapshots` when absent.
    pub global_max_snapshots: Option<usize>,
    /// Optional dedicated archive backend (defaults to log_storage archive when absent)
    pub archive: Option<ArchiveConfig>,
}

fn default_state_prefix() -> String {
    "state/".to_string()
}

fn default_max_snapshots() -> usize {
    5
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

/// Prometheus `/metrics` endpoint configuration.
///
/// When absent, the endpoint is enabled and requires a worker-token Bearer
/// header (same auth posture as `/healthz/detail`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// If true, `/metrics` requires no authentication. Default false.
    #[serde(default)]
    pub public: bool,
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

/// Data retention configuration for cleaning up old workers, jobs, and logs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    /// Hours after which inactive workers are deleted (default: disabled).
    /// Only workers with `status = 'inactive'` and `last_heartbeat` older than
    /// this threshold are removed.
    #[serde(default)]
    pub worker_hours: Option<u64>,
    /// Days after which terminal jobs and their logs are deleted (default: disabled).
    /// Only completed, failed, cancelled, or skipped jobs are removed.
    #[serde(default)]
    pub job_days: Option<u64>,
    /// How often the retention cleanup runs in seconds (default: 3600 — once per hour).
    /// The retention cleanup is rate-limited independently of the recovery sweep interval so
    /// that frequent sweeps do not trigger expensive deletion scans on every cycle.
    #[serde(default = "default_retention_interval")]
    pub interval_secs: u64,
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
fn default_retention_interval() -> u64 {
    3600
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

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            worker_hours: None,
            job_days: None,
            interval_secs: 3600,
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
    #[serde(default)]
    pub retention: RetentionConfig,
    /// ACL configuration (optional — when absent, all authenticated users have full access)
    pub acl: Option<AclConfig>,
    /// MCP server endpoint configuration (optional)
    pub mcp: Option<McpConfig>,
    /// Prometheus /metrics endpoint configuration (optional)
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,
    /// Agent provider configuration (optional)
    pub agents: Option<AgentsConfig>,
    /// Task state snapshot configuration (optional — defaults to log archive backend)
    pub state_storage: Option<StateStorageConfig>,
    /// Default step timeout applied when a `FlowStep` has no `timeout` field.
    /// Capped at the same 24h limit as per-step timeouts. `None` = no default
    /// (existing behaviour: steps without timeouts run unbounded).
    #[serde(default)]
    pub default_step_timeout: Option<HumanDuration>,
    /// Default job timeout applied when a `TaskDef` has no `timeout` field.
    /// Capped at the same 7d limit as per-task timeouts. `None` = no default.
    #[serde(default)]
    pub default_job_timeout: Option<HumanDuration>,
}

/// Resolved timeout defaults passed into job creation.
///
/// Bundles the two `Option<i32>` values so we don't have to thread two more
/// arguments through every `create_job_for_task` call site. `Copy` + `Default`
/// keep test callers terse: `JobDefaults::default()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobDefaults {
    /// Default step timeout in seconds. `None` = no default (run unbounded).
    pub step_timeout_secs: Option<i32>,
    /// Default job timeout in seconds. `None` = no default.
    pub job_timeout_secs: Option<i32>,
}

impl From<&ServerConfig> for JobDefaults {
    /// Conversion is fallible only at the `i32::try_from` cast, which
    /// validation guarantees fits (caps are well below `i32::MAX`).
    fn from(config: &ServerConfig) -> Self {
        Self {
            step_timeout_secs: config.default_step_timeout.map(|d| {
                i32::try_from(d.as_secs()).expect("default_step_timeout validated to fit i32")
            }),
            job_timeout_secs: config.default_job_timeout.map(|d| {
                i32::try_from(d.as_secs()).expect("default_job_timeout validated to fit i32")
            }),
        }
    }
}

/// Agent provider configuration types — defined in `stroem-agent` and re-exported here.
pub use stroem_agent::config::{AgentProviderConfig, AgentsConfig, SUPPORTED_AGENT_PROVIDERS};

impl ServerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.worker_token.len() < 32 {
            anyhow::bail!("worker_token must be at least 32 characters");
        }
        if self.worker_token == "change-me-in-production" {
            tracing::warn!(
                "worker_token is set to the default placeholder value \
                 — replace it with a random secret before running in production"
            );
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
        if let Some(hours) = self.retention.worker_hours {
            if hours < 1 {
                anyhow::bail!("retention.worker_hours must be at least 1");
            }
        }
        if let Some(days) = self.retention.job_days {
            if days < 1 {
                anyhow::bail!("retention.job_days must be at least 1");
            }
        }
        // Zero is already rejected by HumanDuration's deserializer, so we only
        // need to enforce the upper bounds. The cap constants live in
        // `stroem-common::validation` and are shared with the per-step /
        // per-task timeout checks so the two paths can't drift.
        if let Some(ref t) = self.default_step_timeout {
            if t.as_secs() > MAX_STEP_TIMEOUT_SECS {
                anyhow::bail!(
                    "default_step_timeout {}s exceeds maximum of {}s (24h)",
                    t.as_secs(),
                    MAX_STEP_TIMEOUT_SECS
                );
            }
        }
        if let Some(ref t) = self.default_job_timeout {
            if t.as_secs() > MAX_JOB_TIMEOUT_SECS {
                anyhow::bail!(
                    "default_job_timeout {}s exceeds maximum of {}s (7d)",
                    t.as_secs(),
                    MAX_JOB_TIMEOUT_SECS
                );
            }
        }
        if let Some(ref agents) = self.agents {
            for (name, provider) in &agents.providers {
                if !SUPPORTED_AGENT_PROVIDERS.contains(&provider.provider_type.as_str()) {
                    anyhow::bail!(
                        "Agent provider '{}' has unknown type: '{}' (supported: {})",
                        name,
                        provider.provider_type,
                        SUPPORTED_AGENT_PROVIDERS.join(", ")
                    );
                }
                if provider.max_retries > 10 {
                    anyhow::bail!(
                        "Agent provider '{}' max_retries must be at most 10, got {}",
                        name,
                        provider.max_retries
                    );
                }
            }
        }
        Ok(())
    }
}

/// Log HA-relevant diagnostics at startup.
///
/// Emits presence-only information about JWT secrets to confirm they are
/// loaded, without logging any value that could serve as an offline oracle
/// for guessing weak secrets in multi-tenant SIEM environments.
/// No-op when auth isn't configured.
pub fn log_ha_diagnostics(config: &ServerConfig) {
    if let Some(auth) = &config.auth {
        tracing::info!(
            "HA: auth.jwt_secret loaded (len={}). All replicas MUST share this value — verify via your secret-management workflow.",
            auth.jwt_secret.len()
        );
        tracing::info!(
            "HA: auth.refresh_secret loaded (len={}). Same: must match across replicas.",
            auth.refresh_secret.len()
        );
    } else {
        tracing::info!("HA: auth disabled — no JWT secrets to synchronize across replicas");
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

    // ─── Agent provider config tests ────────────────────────────────────────

    #[test]
    fn test_agent_provider_config_type_field() {
        let yaml = r#"
type: anthropic
model: claude-sonnet-4-20250514
api_key: "sk-test"
"#;
        let config: AgentProviderConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.provider_type, "anthropic");
        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(config.max_tokens, 4096); // default
        assert_eq!(config.max_retries, 3); // default
    }

    #[test]
    fn test_agent_provider_config_backward_compat_provider_type() {
        let yaml = r#"
provider_type: openai
model: gpt-4o
api_key: "sk-test"
"#;
        let config: AgentProviderConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.provider_type, "openai");
    }

    #[test]
    fn test_agent_provider_config_optional_fields() {
        let yaml = r#"
type: ollama
model: llama3
api_endpoint: "http://localhost:11434"
"#;
        let config: AgentProviderConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.provider_type, "ollama");
        assert!(config.api_key.is_none());
        assert_eq!(
            config.api_endpoint.as_deref(),
            Some("http://localhost:11434")
        );
    }

    fn make_minimal_config_with_agents(agents_yaml: &str) -> String {
        format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
agents:
{agents_yaml}
"#
        )
    }

    #[test]
    fn test_validate_accepts_all_supported_providers() {
        for &provider in SUPPORTED_AGENT_PROVIDERS {
            let agents_yaml = format!(
                "  providers:\n    test-provider:\n      type: {}\n      model: test\n      api_key: \"key\"",
                provider
            );
            let yaml = make_minimal_config_with_agents(&agents_yaml);
            let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
            assert!(
                config.validate().is_ok(),
                "validate() should accept provider type '{}'",
                provider
            );
        }
    }

    #[test]
    fn test_validate_rejects_unknown_provider_type() {
        let agents_yaml =
            "  providers:\n    bad:\n      type: bedrock\n      model: test\n      api_key: \"key\"";
        let yaml = make_minimal_config_with_agents(agents_yaml);
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("unknown type"),
            "Expected 'unknown type' error, got: {}",
            msg
        );
        assert!(
            msg.contains("bedrock"),
            "Error should mention the bad type, got: {}",
            msg
        );
    }

    // ───── default_step_timeout / default_job_timeout ─────────────────────

    fn make_minimal_config_with_field(extra: &str) -> String {
        format!(
            r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://localhost/stroem"
log_storage:
  local_dir: "./logs"
workspaces:
  default:
    type: folder
    path: "./workspace"
worker_token: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
{extra}
"#
        )
    }

    #[test]
    fn test_default_timeouts_absent_yields_none() {
        let yaml = make_minimal_config_with_field("");
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(config.default_step_timeout.is_none());
        assert!(config.default_job_timeout.is_none());
        assert!(config.validate().is_ok());
        let defaults = JobDefaults::from(&config);
        assert!(defaults.step_timeout_secs.is_none());
        assert!(defaults.job_timeout_secs.is_none());
    }

    #[test]
    fn test_default_timeouts_parsed_from_human_duration() {
        let yaml = make_minimal_config_with_field(
            "default_step_timeout: \"5m\"\ndefault_job_timeout: \"2h\"\n",
        );
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(config.validate().is_ok());
        let defaults = JobDefaults::from(&config);
        assert_eq!(defaults.step_timeout_secs, Some(300));
        assert_eq!(defaults.job_timeout_secs, Some(7200));
    }

    #[test]
    fn test_default_step_timeout_at_max_24h_passes() {
        let yaml = make_minimal_config_with_field("default_step_timeout: \"24h\"\n");
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_default_step_timeout_over_24h_fails() {
        let yaml = make_minimal_config_with_field("default_step_timeout: \"25h\"\n");
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("default_step_timeout"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_default_job_timeout_at_max_7d_passes() {
        // HumanDuration doesn't recognise "d" — operators specify days via h or integer.
        let yaml = make_minimal_config_with_field("default_job_timeout: 604800\n");
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_default_job_timeout_over_7d_fails() {
        let yaml = make_minimal_config_with_field("default_job_timeout: 604801\n");
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("default_job_timeout"),
            "got: {}",
            err
        );
    }

    #[test]
    fn test_default_step_timeout_zero_rejected_at_parse() {
        // 0 is rejected by HumanDuration's deserializer before validate() is even called.
        let yaml = make_minimal_config_with_field("default_step_timeout: 0\n");
        let result: Result<ServerConfig, _> = serde_yaml::from_str(&yaml);
        assert!(result.is_err(), "0 should fail to deserialize");
    }

    #[test]
    fn test_default_job_timeout_zero_rejected_at_parse() {
        // Mirror of the step-side test: HumanDuration rejects 0 on the job
        // field too, before validate() is called. Independent confirmation
        // that the job path is wired through the same deserializer.
        let yaml = make_minimal_config_with_field("default_job_timeout: 0\n");
        let result: Result<ServerConfig, _> = serde_yaml::from_str(&yaml);
        assert!(result.is_err(), "0 should fail to deserialize");
    }

    #[test]
    fn test_only_step_default_converts_correctly() {
        // Asymmetric config: only step default is set. Guards against a future
        // bug where the From impl couples the two fields.
        let yaml = make_minimal_config_with_field("default_step_timeout: \"10m\"\n");
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(config.validate().is_ok());
        let defaults = JobDefaults::from(&config);
        assert_eq!(defaults.step_timeout_secs, Some(600));
        assert_eq!(defaults.job_timeout_secs, None);
    }

    #[test]
    fn test_only_job_default_converts_correctly() {
        // Mirror: only job default is set.
        let yaml = make_minimal_config_with_field("default_job_timeout: \"1h\"\n");
        let config: ServerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(config.validate().is_ok());
        let defaults = JobDefaults::from(&config);
        assert_eq!(defaults.step_timeout_secs, None);
        assert_eq!(defaults.job_timeout_secs, Some(3600));
    }

    #[test]
    fn test_job_defaults_default_is_none() {
        let d = JobDefaults::default();
        assert!(d.step_timeout_secs.is_none());
        assert!(d.job_timeout_secs.is_none());
    }

    /// `log_ha_diagnostics` must not emit secret bytes.
    ///
    /// We set up a tracing subscriber that captures log output, call
    /// `log_ha_diagnostics` with a known JWT secret, and assert:
    ///   1. The output contains "len=" (confirming the length was logged).
    ///   2. The actual secret string does NOT appear in the output.
    ///
    /// This replaces the previously-planned fingerprint test (the fingerprint
    /// was removed as a security fix per the HA review).
    #[test]
    fn log_ha_diagnostics_logs_presence_no_secret_bytes() {
        use std::sync::{Arc, Mutex};
        use tracing::subscriber::with_default;
        use tracing_subscriber::fmt::MakeWriter;

        // A simple writer that captures output into a Vec<u8>.
        #[derive(Clone)]
        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for CaptureWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for CaptureWriter {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let secret = "the-quick-brown-fox-needs-32-chars!";
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter(buf.clone());

        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::INFO)
            .finish();

        with_default(subscriber, || {
            let mut config = minimal_config();
            config.auth = Some(AuthConfig {
                jwt_secret: secret.to_string(),
                refresh_secret: "another-secret-for-refresh-32-ch".to_string(),
                base_url: None,
                initial_user: None,
                providers: std::collections::HashMap::new(),
            });
            log_ha_diagnostics(&config);
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();

        assert!(
            output.contains("len="),
            "log output must contain 'len=' to show secret was loaded; got: {output}"
        );
        assert!(
            !output.contains(secret),
            "log output must NOT contain the raw secret value; got: {output}"
        );
    }

    /// `log_ha_diagnostics` must be a no-op (no panic, no crash) when auth
    /// is not configured.
    #[test]
    fn log_ha_diagnostics_no_op_when_auth_absent() {
        let mut config = minimal_config();
        config.auth = None;
        // Must not panic.
        log_ha_diagnostics(&config);
    }

    /// Minimal valid `ServerConfig` for tests that don't need real workspaces.
    fn minimal_config() -> ServerConfig {
        ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            db: DbConfig {
                url: "postgres://invalid:5432/db".to_string(),
            },
            log_storage: LogStorageConfig {
                local_dir: "/tmp".to_string(),
                s3: None,
                archive: None,
            },
            workspaces: std::collections::HashMap::new(),
            libraries: std::collections::HashMap::new(),
            git_auth: std::collections::HashMap::new(),
            worker_token: "test-token-must-be-long-enough-32".to_string(),
            auth: None,
            recovery: RecoveryConfig::default(),
            retention: RetentionConfig::default(),
            acl: None,
            mcp: None,
            agents: None,
            metrics: None,
            state_storage: None,
            default_step_timeout: None,
            default_job_timeout: None,
        }
    }

    #[test]
    fn metrics_block_absent_yields_none() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://x"
log_storage:
  local_dir: "/tmp"
worker_token: "tok"
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.metrics.is_none());
    }

    #[test]
    fn metrics_public_parses() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://x"
log_storage:
  local_dir: "/tmp"
worker_token: "tok"
metrics:
  public: true
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.metrics.as_ref().map(|m| m.public), Some(true));
    }

    #[test]
    fn metrics_empty_block_yields_some_with_default_public_false() {
        let yaml = r#"
listen: "0.0.0.0:8080"
db:
  url: "postgres://x"
log_storage:
  local_dir: "/tmp"
worker_token: "tok"
metrics: {}
"#;
        let cfg: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.metrics.as_ref().map(|m| m.public), Some(false));
    }
}
