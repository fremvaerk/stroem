use anyhow::{Context, Result};
use std::path::Path;
use stroem_common::models::workflow::{WorkflowConfig, WorkspaceConfig};

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
                tracing::info!("Loading workflow file: {:?}", path);

                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read file: {:?}", path))?;

                let config: WorkflowConfig = serde_yml::from_str(&content)
                    .with_context(|| format!("Failed to parse YAML: {:?}", path))?;

                workspace.merge(config);
            }
        }
    }

    tracing::info!(
        "Loaded workspace: {} actions, {} tasks, {} triggers",
        workspace.actions.len(),
        workspace.tasks.len(),
        workspace.triggers.len()
    );

    Ok(workspace)
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
}
