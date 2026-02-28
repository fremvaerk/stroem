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

    /// Compute a content hash over all files in the workspace directory.
    ///
    /// Walks the entire workspace tree (up to 10 levels deep, following symlinks)
    /// and hashes every file's relative path + content. This detects changes to
    /// scripts, configs, and any other files — not just YAML workflow definitions.
    fn compute_revision(path: &Path) -> Option<String> {
        if !path.exists() {
            return None;
        }

        let mut hasher = Blake2s256::new();

        for entry in walkdir::WalkDir::new(path)
            .max_depth(10)
            .follow_links(true)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }

            let relative = entry
                .path()
                .strip_prefix(path)
                .unwrap_or(entry.path())
                .to_string_lossy();
            hasher.update(relative.as_bytes());

            match std::fs::read(entry.path()) {
                Ok(content) => hasher.update(&content),
                Err(e) => hasher.update(format!("error:{}", e).as_bytes()),
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

    // Render connection values (resolve templates + apply type defaults)
    workspace
        .render_connections()
        .context("Failed to render workspace connections")?;

    tracing::debug!(
        "Loaded workspace: {} actions, {} tasks, {} triggers, {} secrets, {} connection_types, {} connections",
        workspace.actions.len(),
        workspace.tasks.len(),
        workspace.triggers.len(),
        workspace.secrets.len(),
        workspace.connection_types.len(),
        workspace.connections.len()
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
    fn test_compute_revision_includes_non_yaml_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            "actions:\n  a:\n    type: shell\n    cmd: echo a\n",
        )
        .unwrap();
        let rev1 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        // Add a non-YAML file — revision SHOULD change (scripts, configs, etc.)
        fs::write(temp_dir.path().join("run.sh"), "#!/bin/bash\necho hello").unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        assert_ne!(
            rev1, rev2,
            "Non-YAML files should affect revision (scripts, configs)"
        );
    }

    #[test]
    fn test_compute_revision_includes_all_workspace_files() {
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

        // Adding a script in root should change revision (all files are hashed)
        fs::write(temp_dir.path().join("deploy.sh"), "#!/bin/bash\ndeploy").unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path());
        assert_ne!(
            rev, rev2,
            "All workspace files should affect revision, not just .workflows/"
        );
    }

    #[test]
    fn test_compute_revision_includes_subdirectory_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            "actions:\n  a:\n    type: shell\n    cmd: echo a\n",
        )
        .unwrap();
        let rev1 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        // Add a file in a subdirectory — revision should change
        let sub_dir = temp_dir.path().join("scripts");
        fs::create_dir(&sub_dir).unwrap();
        fs::write(sub_dir.join("helper.py"), "print('hello')").unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path()).unwrap();

        assert_ne!(rev1, rev2, "Files in subdirectories should affect revision");
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

    #[tokio::test]
    async fn test_load_inline_actions() {
        let temp_dir = TempDir::new().unwrap();
        let workflow_path = temp_dir.path().join("test.yaml");

        let yaml = r#"
tasks:
  hello:
    flow:
      say-hi:
        type: shell
        cmd: "echo Hello"
      say-bye:
        type: docker
        image: alpine:latest
        command: ["echo", "bye"]
        depends_on: [say-hi]
"#;
        fs::write(&workflow_path, yaml).unwrap();

        let workspace = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        // Inline actions should have been hoisted
        assert_eq!(workspace.actions.len(), 2);
        assert!(workspace.actions.contains_key("_inline:hello:say-hi"));
        assert!(workspace.actions.contains_key("_inline:hello:say-bye"));

        // Steps should reference the hoisted actions
        let task = workspace.tasks.get("hello").unwrap();
        assert_eq!(
            task.flow.get("say-hi").unwrap().action,
            "_inline:hello:say-hi"
        );
        assert_eq!(
            task.flow.get("say-bye").unwrap().action,
            "_inline:hello:say-bye"
        );

        // Action content should be correct
        let hi = workspace.actions.get("_inline:hello:say-hi").unwrap();
        assert_eq!(hi.action_type, "shell");
        assert_eq!(hi.cmd.as_deref(), Some("echo Hello"));

        let bye = workspace.actions.get("_inline:hello:say-bye").unwrap();
        assert_eq!(bye.action_type, "docker");
        assert_eq!(bye.image.as_deref(), Some("alpine:latest"));
    }

    #[tokio::test]
    async fn test_load_mixed_inline_and_reference() {
        let temp_dir = TempDir::new().unwrap();
        let workflow_path = temp_dir.path().join("test.yaml");

        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo Hello"

tasks:
  mixed:
    flow:
      ref-step:
        action: greet
      inline-step:
        type: shell
        cmd: "echo Inline"
"#;
        fs::write(&workflow_path, yaml).unwrap();

        let workspace = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        // Named action + hoisted inline action
        assert_eq!(workspace.actions.len(), 2);
        assert!(workspace.actions.contains_key("greet"));
        assert!(workspace.actions.contains_key("_inline:mixed:inline-step"));

        let task = workspace.tasks.get("mixed").unwrap();
        assert_eq!(task.flow.get("ref-step").unwrap().action, "greet");
        assert_eq!(
            task.flow.get("inline-step").unwrap().action,
            "_inline:mixed:inline-step"
        );
    }
}
