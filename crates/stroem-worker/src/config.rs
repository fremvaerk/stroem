use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerConfig {
    pub server_url: String,
    pub worker_token: String,
    pub worker_name: String,
    pub max_concurrent: usize,
    pub poll_interval_secs: u64,
    pub workspace_dir: String,
    #[serde(default = "default_capabilities")]
    pub capabilities: Vec<String>,
}

fn default_capabilities() -> Vec<String> {
    vec!["shell".to_string()]
}

pub fn load_config(path: &str) -> Result<WorkerConfig> {
    let content =
        std::fs::read_to_string(path).context(format!("Failed to read config file: {}", path))?;
    let config: WorkerConfig =
        serde_yml::from_str(&content).context("Failed to parse worker config YAML")?;
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
workspace_dir: "/tmp/stroem-workspace"
capabilities:
  - shell
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = load_config(file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.server_url, "http://localhost:8080");
        assert_eq!(config.worker_token, "test-token");
        assert_eq!(config.worker_name, "worker-1");
        assert_eq!(config.max_concurrent, 4);
        assert_eq!(config.poll_interval_secs, 2);
        assert_eq!(config.workspace_dir, "/tmp/stroem-workspace");
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
workspace_dir: "/tmp/stroem-workspace"
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        file.flush().unwrap();

        let config = load_config(file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.capabilities, vec!["shell"]);
    }
}
