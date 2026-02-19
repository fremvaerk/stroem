use anyhow::{Context, Result};
use async_trait::async_trait;
use blake2::{Blake2s256, Digest};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use stroem_common::models::workflow::{WorkflowConfig, WorkspaceConfig};

use super::WorkspaceSource;

/// Folder-based workspace source
pub struct FolderSource {
    path: PathBuf,
    revision: RwLock<Option<String>>,
}

impl FolderSource {
    pub fn new(path: &str) -> Self {
        Self {
            path: PathBuf::from(path),
            revision: RwLock::new(None),
        }
    }

    fn compute_revision(path: &Path) -> Option<String> {
        let mut hasher = Blake2s256::new();
        let scan_dir = {
            let workflows_dir = path.join(".workflows");
            if workflows_dir.exists() && workflows_dir.is_dir() {
                workflows_dir
            } else {
                path.to_path_buf()
            }
        };

        let entries = match std::fs::read_dir(&scan_dir) {
            Ok(e) => e,
            Err(_) => return None,
        };

        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "yaml" || ext == "yml")
                    .unwrap_or(false)
            })
            .map(|e| e.path())
            .collect();

        paths.sort();

        for p in &paths {
            hasher.update(p.to_string_lossy().as_bytes());
            if let Ok(content) = std::fs::read(p) {
                hasher.update(&content);
            }
        }

        let hash = hasher.finalize();
        Some(hex::encode(hash))
    }
}

#[async_trait]
impl WorkspaceSource for FolderSource {
    async fn load(&self) -> Result<WorkspaceConfig> {
        let config = load_folder_workspace(self.path.to_str().unwrap_or("")).await?;
        if let Some(rev) = Self::compute_revision(&self.path) {
            if let Ok(mut lock) = self.revision.write() {
                *lock = Some(rev);
            }
        }
        Ok(config)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn revision(&self) -> Option<String> {
        self.revision.read().ok().and_then(|r| r.clone())
    }

    fn peek_revision(&self) -> Option<String> {
        Self::compute_revision(&self.path)
    }
}

/// Load workspace from a folder containing workflow YAML files
/// Looks for .workflows/ subdirectory, or scans the folder itself if it contains YAML files
pub async fn load_folder_workspace(path: &str) -> Result<WorkspaceConfig> {
    let base_path = Path::new(path);

    if !base_path.exists() {
        anyhow::bail!("Workspace path does not exist: {}", path);
    }

    let mut workspace = WorkspaceConfig::new();

    // Check for .workflows/ subdirectory
    let workflows_dir = base_path.join(".workflows");
    let scan_dir = if workflows_dir.exists() && workflows_dir.is_dir() {
        workflows_dir
    } else {
        base_path.to_path_buf()
    };

    // Scan for YAML files
    let entries = std::fs::read_dir(&scan_dir)
        .with_context(|| format!("Failed to read directory: {:?}", scan_dir))?;

    for entry in entries {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();

        // Skip directories
        if path.is_dir() {
            continue;
        }

        // Check if it's a YAML file
        if let Some(ext) = path.extension() {
            if ext == "yaml" || ext == "yml" {
                if stroem_common::sops::is_sops_file(&path) {
                    tracing::debug!("Loading SOPS-encrypted workflow file: {:?}", path);
                } else {
                    tracing::debug!("Loading workflow file: {:?}", path);
                }

                let content = stroem_common::sops::read_yaml_file(&path)
                    .with_context(|| format!("Failed to read file: {:?}", path))?;

                let config: WorkflowConfig = serde_yaml::from_str(&content)
                    .with_context(|| format!("Failed to parse YAML: {:?}", path))?;

                workspace.merge(config);
            }
        }
    }

    // Render secret values through Tera (resolves {{ 'ref+...' | vals }} at load time)
    workspace
        .render_secrets()
        .context("Failed to render workspace secrets")?;

    tracing::debug!(
        "Loaded workspace: {} actions, {} tasks, {} triggers, {} secrets",
        workspace.actions.len(),
        workspace.tasks.len(),
        workspace.triggers.len(),
        workspace.secrets.len()
    );

    Ok(workspace)
}

/// Hex-encode helper (avoids adding another dep)
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_load_empty_folder() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(workspace.actions.len(), 0);
        assert_eq!(workspace.tasks.len(), 0);
        assert_eq!(workspace.triggers.len(), 0);
    }

    #[tokio::test]
    async fn test_load_single_workflow() {
        let temp_dir = TempDir::new().unwrap();
        let workflow_path = temp_dir.path().join("test.yaml");

        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo Hello"

tasks:
  hello:
    flow:
      step1:
        action: greet
"#;
        fs::write(&workflow_path, yaml).unwrap();

        let workspace = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(workspace.actions.len(), 1);
        assert_eq!(workspace.tasks.len(), 1);
        assert!(workspace.actions.contains_key("greet"));
        assert!(workspace.tasks.contains_key("hello"));
    }

    #[tokio::test]
    async fn test_load_multiple_workflows() {
        let temp_dir = TempDir::new().unwrap();

        let workflow1 = temp_dir.path().join("workflow1.yml");
        fs::write(
            &workflow1,
            r#"
actions:
  action1:
    type: shell
    cmd: "echo 1"
tasks:
  task1:
    flow:
      step1:
        action: action1
"#,
        )
        .unwrap();

        let workflow2 = temp_dir.path().join("workflow2.yaml");
        fs::write(
            &workflow2,
            r#"
actions:
  action2:
    type: shell
    cmd: "echo 2"
tasks:
  task2:
    flow:
      step1:
        action: action2
"#,
        )
        .unwrap();

        let workspace = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(workspace.actions.len(), 2);
        assert_eq!(workspace.tasks.len(), 2);
        assert!(workspace.actions.contains_key("action1"));
        assert!(workspace.actions.contains_key("action2"));
    }

    #[tokio::test]
    async fn test_load_from_workflows_subdir() {
        let temp_dir = TempDir::new().unwrap();
        let workflows_dir = temp_dir.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();

        let workflow_path = workflows_dir.join("test.yaml");
        fs::write(
            &workflow_path,
            r#"
actions:
  test:
    type: shell
    cmd: "echo test"
"#,
        )
        .unwrap();

        let workspace = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(workspace.actions.len(), 1);
        assert!(workspace.actions.contains_key("test"));
    }

    #[tokio::test]
    async fn test_skip_non_yaml_files() {
        let temp_dir = TempDir::new().unwrap();

        fs::write(temp_dir.path().join("readme.txt"), "not yaml").unwrap();
        fs::write(
            temp_dir.path().join("workflow.yaml"),
            r#"
actions:
  test:
    type: shell
    cmd: "echo test"
"#,
        )
        .unwrap();

        let workspace = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(workspace.actions.len(), 1);
    }

    #[tokio::test]
    async fn test_nonexistent_path() {
        let result = load_folder_workspace("/nonexistent/path/12345").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_folder_source_revision() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
"#,
        )
        .unwrap();

        let source = FolderSource::new(temp_dir.path().to_str().unwrap());
        assert!(source.revision().is_none()); // no revision before load

        source.load().await.unwrap();
        assert!(source.revision().is_some()); // revision set after load
    }

    #[test]
    fn test_compute_revision_deterministic() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            "actions:\n  greet:\n    type: shell\n    cmd: echo hi\n",
        )
        .unwrap();

        let rev1 = FolderSource::compute_revision(temp_dir.path());
        let rev2 = FolderSource::compute_revision(temp_dir.path());
        assert!(rev1.is_some());
        assert_eq!(rev1, rev2, "Same content should produce same revision");
    }

    #[test]
    fn test_compute_revision_changes_on_content_change() {
        let temp_dir = TempDir::new().unwrap();
        let yaml_path = temp_dir.path().join("test.yaml");

        fs::write(
            &yaml_path,
            "actions:\n  greet:\n    type: shell\n    cmd: echo v1\n",
        )
        .unwrap();
        let rev1 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        fs::write(
            &yaml_path,
            "actions:\n  greet:\n    type: shell\n    cmd: echo v2\n",
        )
        .unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        assert_ne!(
            rev1, rev2,
            "Different content should produce different revision"
        );
    }

    #[test]
    fn test_compute_revision_changes_on_file_added() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("a.yaml"),
            "actions:\n  a:\n    type: shell\n    cmd: echo a\n",
        )
        .unwrap();
        let rev1 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        fs::write(
            temp_dir.path().join("b.yaml"),
            "actions:\n  b:\n    type: shell\n    cmd: echo b\n",
        )
        .unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        assert_ne!(rev1, rev2, "Adding a file should change revision");
    }

    #[test]
    fn test_compute_revision_changes_on_file_removed() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("a.yaml"),
            "actions:\n  a:\n    type: shell\n    cmd: echo a\n",
        )
        .unwrap();
        let b_path = temp_dir.path().join("b.yaml");
        fs::write(
            &b_path,
            "actions:\n  b:\n    type: shell\n    cmd: echo b\n",
        )
        .unwrap();
        let rev1 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        fs::remove_file(&b_path).unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        assert_ne!(rev1, rev2, "Removing a file should change revision");
    }

    #[test]
    fn test_compute_revision_ignores_non_yaml_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            "actions:\n  a:\n    type: shell\n    cmd: echo a\n",
        )
        .unwrap();
        let rev1 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        // Add a non-YAML file — revision should NOT change
        fs::write(temp_dir.path().join("README.md"), "# Hello").unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        assert_eq!(rev1, rev2, "Non-YAML files should not affect revision");
    }

    #[test]
    fn test_compute_revision_uses_workflows_subdir() {
        let temp_dir = TempDir::new().unwrap();
        let workflows_dir = temp_dir.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("test.yaml"),
            "actions:\n  a:\n    type: shell\n    cmd: echo a\n",
        )
        .unwrap();

        let rev = FolderSource::compute_revision(temp_dir.path());
        assert!(rev.is_some());

        // Revision should be based on .workflows/ content, not root
        // Add a YAML file in root — revision should NOT change
        fs::write(
            temp_dir.path().join("other.yaml"),
            "actions:\n  b:\n    type: shell\n    cmd: echo b\n",
        )
        .unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path());
        assert_eq!(
            rev, rev2,
            "Root YAML should be ignored when .workflows/ exists"
        );
    }

    #[test]
    fn test_compute_revision_empty_dir_returns_some() {
        let temp_dir = TempDir::new().unwrap();
        // No YAML files at all
        let rev = FolderSource::compute_revision(temp_dir.path());
        // Should still return Some (hash of empty input)
        assert!(rev.is_some());
    }

    #[test]
    fn test_compute_revision_nonexistent_dir_returns_none() {
        let rev = FolderSource::compute_revision(Path::new("/nonexistent/path/12345"));
        assert!(rev.is_none());
    }

    #[tokio::test]
    async fn test_folder_source_reload_updates_config_and_revision() {
        let temp_dir = TempDir::new().unwrap();
        let yaml_path = temp_dir.path().join("test.yaml");

        // Initial load with one action
        fs::write(
            &yaml_path,
            "actions:\n  greet:\n    type: shell\n    cmd: echo v1\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        )
        .unwrap();

        let source = FolderSource::new(temp_dir.path().to_str().unwrap());
        let config1 = source.load().await.unwrap();
        let rev1 = source.revision().unwrap();

        assert_eq!(config1.actions.len(), 1);
        assert!(config1.actions.contains_key("greet"));

        // Modify file: add another action
        fs::write(
            &yaml_path,
            "actions:\n  greet:\n    type: shell\n    cmd: echo v2\n  build:\n    type: shell\n    cmd: make\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        )
        .unwrap();

        let config2 = source.load().await.unwrap();
        let rev2 = source.revision().unwrap();

        // Config should reflect the new file
        assert_eq!(config2.actions.len(), 2);
        assert!(config2.actions.contains_key("greet"));
        assert!(config2.actions.contains_key("build"));

        // Revision should have changed
        assert_ne!(rev1, rev2, "Revision should change after reload");
    }

    #[test]
    fn test_compute_revision_is_hex_string() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            "actions:\n  a:\n    type: shell\n    cmd: echo a\n",
        )
        .unwrap();

        let rev = FolderSource::compute_revision(temp_dir.path()).unwrap();
        assert!(!rev.is_empty());
        assert!(
            rev.chars().all(|c| c.is_ascii_hexdigit()),
            "Revision should be hex-encoded: {}",
            rev
        );
        // Blake2s256 produces 32 bytes = 64 hex chars
        assert_eq!(rev.len(), 64, "Blake2s256 should produce 64 hex chars");
    }

    #[tokio::test]
    async fn test_sops_file_triggers_decryption() {
        // A *.sops.yaml file should trigger SOPS decryption, which fails
        // when sops is not installed or the file isn't actually encrypted.
        let temp_dir = TempDir::new().unwrap();

        // Add a regular file so the workspace isn't empty
        fs::write(
            temp_dir.path().join("regular.yaml"),
            "actions:\n  test:\n    type: shell\n    cmd: echo hi\n",
        )
        .unwrap();

        // Add a file named *.sops.yaml — this triggers the SOPS code path
        fs::write(
            temp_dir.path().join("secrets.sops.yaml"),
            "actions:\n  secret:\n    type: shell\n    cmd: echo secret\n",
        )
        .unwrap();

        let result = load_folder_workspace(temp_dir.path().to_str().unwrap()).await;
        // Should fail because sops decryption fails (not installed or file not encrypted)
        assert!(result.is_err());
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("sops"), "Error should mention sops: {}", err);
    }

    #[tokio::test]
    async fn test_sops_config_file_ignored_as_sops() {
        // .sops.yaml (dot-prefixed) is a SOPS config file, not an encrypted file.
        // It should be read as plain YAML (not decrypted).
        let temp_dir = TempDir::new().unwrap();

        // Write .sops.yaml — serde_yaml will parse it; unknown fields are
        // silently ignored, producing an empty WorkflowConfig.
        fs::write(
            temp_dir.path().join(".sops.yaml"),
            "creation_rules:\n  - path_regex: \\.sops\\.yaml$\n    age: age1test\n",
        )
        .unwrap();

        // Also add a real workflow so we can verify it loads
        fs::write(
            temp_dir.path().join("workflow.yaml"),
            "actions:\n  greet:\n    type: shell\n    cmd: echo hi\n",
        )
        .unwrap();

        let workspace = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        // The .sops.yaml config file should not cause an error and should not
        // contribute any actions/tasks (it merges as empty).
        assert_eq!(workspace.actions.len(), 1);
        assert!(workspace.actions.contains_key("greet"));
    }
}
