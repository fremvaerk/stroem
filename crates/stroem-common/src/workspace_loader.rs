use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::models::workflow::{WorkflowConfig, WorkspaceConfig};

/// Maximum recursion depth for subdirectory scanning (matches `compute_revision`).
const MAX_SCAN_DEPTH: usize = 10;

/// Scans a directory for YAML workflow files, parses them, and merges them into
/// a [`WorkspaceConfig`].
///
/// Recursively scans subdirectories. Files are processed in **sorted path order**
/// for deterministic merges. Non-YAML files are silently skipped.
///
/// Files that fail to read or parse are skipped with a warning rather than
/// aborting the entire workspace load. The returned `Vec<String>` contains one
/// human-readable warning message per skipped file.
///
/// # Parameters
///
/// - `scan_dir` — The directory to scan. Must exist; returns an error otherwise.
/// - `skip_sops` — When `true`, files detected as SOPS-encrypted are skipped
///   entirely (library mode). When `false`, SOPS files are decrypted via
///   `stroem_common::sops::read_yaml_file` (workspace folder mode).
/// - `infer_folders` — When `true`, tasks without an explicit `folder` field
///   inherit a folder from their file's path relative to `scan_dir`
///   (e.g. `data/etl.yaml` → `folder: "data"`). Disabled for libraries.
///
/// # Errors
///
/// Returns an error only if the directory cannot be read (`collect_yaml_files`
/// fails). Individual file read/parse errors are captured as warnings.
pub fn scan_and_merge_yaml_files(
    scan_dir: &Path,
    skip_sops: bool,
    infer_folders: bool,
) -> Result<(WorkspaceConfig, Vec<String>)> {
    // Collect YAML files recursively from scan_dir and all subdirectories.
    let mut entries: Vec<PathBuf> = Vec::new();
    collect_yaml_files(scan_dir, &mut entries, 0)?;

    // Sort by full path for deterministic merge order across platforms
    entries.sort();

    let mut workspace = WorkspaceConfig::new();
    let mut warnings: Vec<String> = Vec::new();

    for file_path in entries {
        let is_sops = crate::sops::is_sops_file(&file_path);

        if is_sops && skip_sops {
            tracing::debug!("Skipping SOPS file in library: {}", file_path.display());
            continue;
        }

        if is_sops {
            tracing::debug!(
                "Loading SOPS-encrypted workflow file: {}",
                file_path.display()
            );
        } else {
            tracing::debug!("Loading workflow file: {}", file_path.display());
        }

        // Compute a display path relative to scan_dir for human-readable warnings.
        let display_path = file_path
            .strip_prefix(scan_dir)
            .unwrap_or(&file_path)
            .display()
            .to_string();

        let content = match crate::sops::read_yaml_file(&file_path) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Skipping '{}': failed to read file: {:#}", display_path, e);
                tracing::warn!("{}", msg);
                warnings.push(msg);
                continue;
            }
        };

        let mut config: WorkflowConfig = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Skipping '{}': failed to parse YAML: {}", display_path, e);
                tracing::warn!("{}", msg);
                warnings.push(msg);
                continue;
            }
        };

        // For files in subdirectories, infer task folder from the relative path
        // (e.g., .workflows/data/etl.yaml → folder "data").
        // Only applies when the task has no explicit folder set.
        if infer_folders {
            if let Some(rel_dir) = file_path
                .parent()
                .and_then(|p| p.strip_prefix(scan_dir).ok())
                .filter(|p| !p.as_os_str().is_empty())
            {
                let folder = rel_dir.to_string_lossy().replace('\\', "/");
                for task in config.tasks.values_mut() {
                    if task.folder.is_none() {
                        task.folder = Some(folder.clone());
                    }
                }
            }
        }

        workspace.merge(config);
    }

    Ok((workspace, warnings))
}

/// Recursively collect YAML files from a directory and its subdirectories.
fn collect_yaml_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    if depth > MAX_SCAN_DEPTH {
        tracing::warn!(
            "Skipping directory (max depth {}): {}",
            MAX_SCAN_DEPTH,
            dir.display()
        );
        return Ok(());
    }

    let read_dir = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory: {}", dir.display()))?;

    for entry in read_dir {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();
        if path.is_dir() {
            collect_yaml_files(&path, out, depth + 1)?;
        } else if path
            .extension()
            .map(|ext| ext == "yaml" || ext == "yml")
            .unwrap_or(false)
        {
            out.push(path);
        }
    }

    Ok(())
}

/// Loads a workspace from a directory path.
///
/// If the path contains a `.workflows/` subdirectory, scans that.
/// Otherwise scans the path directly if it contains YAML files.
/// After scanning, renders secrets and connections.
///
/// Returns the merged config and any warnings from skipped files.
pub fn load_workspace(path: &Path) -> Result<(WorkspaceConfig, Vec<String>)> {
    if !path.exists() {
        anyhow::bail!("Workspace path does not exist: {}", path.display());
    }

    let workflows_dir = path.join(".workflows");
    let scan_dir = if workflows_dir.exists() && workflows_dir.is_dir() {
        workflows_dir
    } else {
        path.to_path_buf()
    };

    let (mut workspace, warnings) = scan_and_merge_yaml_files(&scan_dir, false, true)
        .with_context(|| format!("Failed to scan workspace directory: {:?}", scan_dir))?;

    workspace
        .render_secrets()
        .context("Failed to render workspace secrets")?;

    workspace
        .render_connections()
        .context("Failed to render workspace connections")?;

    Ok((workspace, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_scan_and_merge_single_file() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.yaml"),
            "actions:\n  greet:\n    type: script\n    script: echo hello\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        ).unwrap();

        let (config, warnings) = scan_and_merge_yaml_files(dir.path(), false, true).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(config.actions.len(), 1);
        assert_eq!(config.tasks.len(), 1);
    }

    #[test]
    fn test_scan_and_merge_multiple_files() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.yaml"),
            "actions:\n  a1:\n    type: script\n    script: echo a\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.yaml"),
            "actions:\n  b1:\n    type: script\n    script: echo b\ntasks:\n  t1:\n    flow:\n      s1:\n        action: b1\n",
        )
        .unwrap();

        let (config, warnings) = scan_and_merge_yaml_files(dir.path(), false, true).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(config.actions.len(), 2);
        assert!(config.actions.contains_key("a1"));
        assert!(config.actions.contains_key("b1"));
    }

    #[test]
    fn test_scan_skips_bad_yaml_with_warning() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("good.yaml"),
            "actions:\n  a1:\n    type: script\n    script: echo ok\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("bad.yaml"),
            "actions:\n  a:\n    script: \"unterminated\n",
        )
        .unwrap();

        let (config, warnings) = scan_and_merge_yaml_files(dir.path(), false, true).unwrap();
        assert_eq!(config.actions.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad.yaml"));
    }

    #[test]
    fn test_scan_recurses_subdirectories() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(
            sub.join("nested.yaml"),
            "actions:\n  nested:\n    type: script\n    script: echo nested\n",
        )
        .unwrap();

        let (config, _) = scan_and_merge_yaml_files(dir.path(), false, true).unwrap();
        assert_eq!(config.actions.len(), 1);
        assert!(config.actions.contains_key("nested"));
    }

    #[test]
    fn test_scan_infers_folder_from_subdirectory() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("data");
        fs::create_dir(&sub).unwrap();
        fs::write(
            sub.join("etl.yaml"),
            "tasks:\n  etl:\n    flow:\n      s1:\n        action: run\n",
        )
        .unwrap();

        let (config, _) = scan_and_merge_yaml_files(dir.path(), false, true).unwrap();
        let task = config.tasks.get("etl").unwrap();
        assert_eq!(task.folder, Some("data".to_string()));
    }

    #[test]
    fn test_scan_no_folder_inference_when_disabled() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("data");
        fs::create_dir(&sub).unwrap();
        fs::write(
            sub.join("etl.yaml"),
            "tasks:\n  etl:\n    flow:\n      s1:\n        action: run\n",
        )
        .unwrap();

        let (config, _) = scan_and_merge_yaml_files(dir.path(), false, false).unwrap();
        let task = config.tasks.get("etl").unwrap();
        assert_eq!(task.folder, None);
    }

    #[test]
    fn test_load_workspace_with_workflows_subdir() {
        let dir = TempDir::new().unwrap();
        let workflows = dir.path().join(".workflows");
        fs::create_dir(&workflows).unwrap();
        fs::write(
            workflows.join("test.yaml"),
            "actions:\n  greet:\n    type: script\n    script: echo hello\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        ).unwrap();

        let (config, warnings) = load_workspace(dir.path()).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(config.actions.len(), 1);
        assert_eq!(config.tasks.len(), 1);
    }

    #[test]
    fn test_load_workspace_without_workflows_subdir() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.yaml"),
            "actions:\n  greet:\n    type: script\n    script: echo hello\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        ).unwrap();

        let (config, warnings) = load_workspace(dir.path()).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(config.actions.len(), 1);
    }

    #[test]
    fn test_load_workspace_nonexistent_path() {
        let result = load_workspace(Path::new("/nonexistent/path/12345"));
        assert!(result.is_err());
    }

    #[test]
    fn test_collect_yaml_files_includes_yml() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.yml"), "").unwrap();
        fs::write(dir.path().join("b.yaml"), "").unwrap();
        fs::write(dir.path().join("c.json"), "").unwrap();

        let mut files = Vec::new();
        collect_yaml_files(dir.path(), &mut files, 0).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_empty_directory_produces_empty_config() {
        let dir = TempDir::new().unwrap();
        let (config, warnings) = scan_and_merge_yaml_files(dir.path(), false, true).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(config.actions.len(), 0);
        assert_eq!(config.tasks.len(), 0);
    }
}
