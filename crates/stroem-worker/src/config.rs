use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerConfig {
    pub server_url: String,
    pub worker_token: String,
    pub worker_name: String,
    pub max_concurrent: usize,
    pub poll_interval_secs: u64,
    /// Base directory for caching workspace tarballs.
    /// Each workspace gets a subdirectory: `{workspace_cache_dir}/{workspace_name}/`
    #[serde(alias = "workspace_dir")]
    pub workspace_cache_dir: String,
    #[serde(default = "default_capabilities")]
    pub capabilities: Vec<String>,
    /// Worker tags for step routing (replaces capabilities). If not set, falls back to capabilities.
    pub tags: Option<Vec<String>>,
    /// Default runner image for Type 2 (shell-in-container) execution
    pub runner_image: Option<String>,
    /// Docker runner configuration (requires `docker` feature)
    pub docker: Option<DockerRunnerConfig>,
    /// Kubernetes runner configuration (requires `kubernetes` feature)
    pub kubernetes: Option<KubeRunnerConfig>,
}

impl WorkerConfig {
    /// Returns tags if set, otherwise falls back to capabilities
    pub fn effective_tags(&self) -> &[String] {
        self.tags.as_deref().unwrap_or(&self.capabilities)
    }
}

/// Configuration for the Docker runner
#[derive(Debug, Clone, Deserialize)]
pub struct DockerRunnerConfig {
    /// Docker host URL (e.g. "tcp://localhost:2376"). If not set, uses default socket.
    pub host: Option<String>,
}

/// Configuration for the Kubernetes runner
#[derive(Debug, Clone, Deserialize)]
pub struct KubeRunnerConfig {
    /// Namespace to create pods in
    pub namespace: String,
    /// Custom init container image (default: curlimages/curl:latest)
    pub init_image: Option<String>,
    /// ConfigMap name containing startup scripts for runner pods
    pub runner_startup_configmap: Option<String>,
}

fn default_capabilities() -> Vec<String> {
    vec!["shell".to_string()]
}

pub fn load_config(path: &str) -> Result<WorkerConfig> {
    let config: WorkerConfig = config::Config::builder()
        .add_source(config::File::new(path, config::FileFormat::Yaml))
        .add_source(
            config::Environment::with_prefix("STROEM")
                .prefix_separator("__")
                .separator("__"),
        )
        .build()
        .context(format!("Failed to build config from: {}", path))?
        .try_deserialize()
        .context("Failed to deserialize worker config")?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_config() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
capabilities:
  - shell
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.server_url, "http://localhost:8080");
        assert_eq!(config.worker_token, "test-token");
        assert_eq!(config.worker_name, "worker-1");
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.poll_interval_secs, 2);
        assert_eq!(config.workspace_cache_dir, "/tmp/stroem-workspace");
        assert_eq!(config.capabilities, vec!["shell"]);
    }

    #[test]
    fn test_default_capabilities() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.capabilities, vec!["shell"]);
    }

    #[test]
    fn test_config_with_docker() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
docker:
  host: "tcp://localhost:2376"
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        let docker = config.docker.unwrap();
        assert_eq!(docker.host, Some("tcp://localhost:2376".to_string()));
    }

    #[test]
    fn test_config_with_kubernetes() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
kubernetes:
  namespace: "stroem-jobs"
  init_image: "curlimages/curl:8.5.0"
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        let kube = config.kubernetes.unwrap();
        assert_eq!(kube.namespace, "stroem-jobs");
        assert_eq!(kube.init_image, Some("curlimages/curl:8.5.0".to_string()));
        assert!(kube.runner_startup_configmap.is_none());
    }

    #[test]
    fn test_config_with_kubernetes_startup_configmap() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
kubernetes:
  namespace: "stroem-jobs"
  runner_startup_configmap: "stroem-runner-startup"
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        let kube = config.kubernetes.unwrap();
        assert_eq!(kube.namespace, "stroem-jobs");
        assert!(kube.init_image.is_none());
        assert_eq!(
            kube.runner_startup_configmap,
            Some("stroem-runner-startup".to_string())
        );
    }

    #[test]
    fn test_effective_tags_with_tags_set() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
capabilities:
  - shell
tags:
  - shell
  - docker
  - node-20
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        // When tags is set, effective_tags() returns tags (not capabilities)
        assert_eq!(config.effective_tags(), &["shell", "docker", "node-20"]);
    }

    #[test]
    fn test_effective_tags_without_tags() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
capabilities:
  - shell
  - docker
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        // When tags is not set, effective_tags() falls back to capabilities
        assert!(config.tags.is_none());
        assert_eq!(config.effective_tags(), &["shell", "docker"]);
    }

    #[test]
    fn test_config_with_tags_and_runner_image() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
tags:
  - shell
  - docker
  - gpu
runner_image: "ghcr.io/myorg/stroem-runner:latest"
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.tags,
            Some(vec![
                "shell".to_string(),
                "docker".to_string(),
                "gpu".to_string()
            ])
        );
        assert_eq!(
            config.runner_image,
            Some("ghcr.io/myorg/stroem-runner:latest".to_string())
        );
        assert_eq!(config.effective_tags(), &["shell", "docker", "gpu"]);
    }

    /// Serialize access to env vars in tests to avoid races between parallel tests
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_env_override_worker_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "yaml-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        // SAFETY: test-only, serialized by ENV_MUTEX
        unsafe {
            std::env::set_var("STROEM__WORKER_TOKEN", "env-token");
            std::env::set_var("STROEM__SERVER_URL", "http://overridden:8080");
        }

        let config = load_config(file.path().to_str().unwrap()).unwrap();

        unsafe {
            std::env::remove_var("STROEM__WORKER_TOKEN");
            std::env::remove_var("STROEM__SERVER_URL");
        }

        assert_eq!(config.worker_token, "env-token");
        assert_eq!(config.server_url, "http://overridden:8080");
        // Non-overridden values preserved from YAML
        assert_eq!(config.worker_name, "worker-1");
        assert_eq!(config.max_concurrent, 4);
    }

    #[test]
    fn test_env_override_worker_name() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "yaml-worker"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        // SAFETY: test-only, serialized by ENV_MUTEX
        unsafe {
            std::env::set_var("STROEM__WORKER_NAME", "k8s-worker-pod-abc");
        }

        let config = load_config(file.path().to_str().unwrap()).unwrap();

        unsafe {
            std::env::remove_var("STROEM__WORKER_NAME");
        }

        assert_eq!(config.worker_name, "k8s-worker-pod-abc");
    }

    #[test]
    fn test_config_no_runners() {
        let yaml = r#"
server_url: "http://localhost:8080"
worker_token: "test-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
"#;

        let config: WorkerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.docker.is_none());
        assert!(config.kubernetes.is_none());
    }
}
