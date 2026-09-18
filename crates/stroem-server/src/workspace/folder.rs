use anyhow::Result;
use blake2::{Blake2s256, Digest};
use std::path::{Path, PathBuf};
use stroem_common::budget::{DeadlineExceeded, LoadBudget};
use stroem_common::models::workflow::WorkspaceConfig;

use super::source::{LoadOutcome, Peek};
use super::WorkspaceSource;

/// Folder-based workspace source
pub struct FolderSource {
    path: PathBuf,
}

impl FolderSource {
    pub fn new(path: &str) -> Self {
        Self {
            path: PathBuf::from(path),
        }
    }

    /// Hash every file's relative path + content (spec § 4.3 folder rows).
    fn compute_revision(path: &Path, budget: &LoadBudget) -> Peek {
        if !path.exists() {
            return Peek::LocalInvalid(anyhow::anyhow!(
                "workspace folder {} does not exist",
                path.display()
            ));
        }
        let mut hasher = Blake2s256::new();
        for entry in walkdir::WalkDir::new(path)
            .max_depth(10)
            .follow_links(true)
            .sort_by_file_name()
        {
            if budget.expired() {
                return Peek::Failed(DeadlineExceeded.into());
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => match symlink_walk_error(&e) {
                    Some((p, target)) => {
                        // A dangling symlink or a symlink loop is stable local
                        // state (stale venvs, editor lock files, node_modules/.bin
                        // leftovers) — not a sign the workspace is broken. Hash
                        // the link's relative path plus its target (or "loop")
                        // instead of failing the whole walk.
                        let relative = p.strip_prefix(path).unwrap_or(&p).to_string_lossy();
                        hasher.update(relative.as_bytes());
                        hasher.update(target.as_bytes());
                        continue;
                    }
                    None => {
                        return Peek::Failed(anyhow::Error::new(e).context("walk workspace folder"))
                    }
                },
            };
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
                Err(e) => {
                    return Peek::Failed(
                        anyhow::Error::new(e).context(format!("read {}", entry.path().display())),
                    )
                }
            }
        }
        Peek::Revision(hex::encode(hasher.finalize()))
    }
}

/// A dangling symlink or a symlink loop found while walking the workspace
/// folder is stable local state (stale venvs, editor lock files,
/// `node_modules/.bin` leftovers) — not a sign the workspace is broken.
/// Returns the path and a stand-in for its "content" (the link target, or the
/// literal `"loop"`) to hash instead of failing the whole walk. Only a real
/// loop, or a symlink whose target is `NotFound` (dangling), qualifies; every
/// other walk error — including a permission or I/O error reached through a
/// symlink — returns `None`, which still fails the walk (`Peek::Failed`).
fn symlink_walk_error(err: &walkdir::Error) -> Option<(PathBuf, String)> {
    let p = err.path()?;
    if err.loop_ancestor().is_some() {
        return Some((p.to_path_buf(), "loop".to_string()));
    }
    let is_symlink = std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    let dangling = err.io_error().map(|e| e.kind()) == Some(std::io::ErrorKind::NotFound);
    if !(is_symlink && dangling) {
        return None;
    }
    let target = std::fs::read_link(p)
        .map(|t| t.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some((p.to_path_buf(), target))
}

impl WorkspaceSource for FolderSource {
    fn load(&self, budget: &LoadBudget) -> Result<LoadOutcome> {
        let (config, warnings) = load_folder_workspace_with(&self.path, budget)?;
        let revision = match Self::compute_revision(&self.path, budget) {
            Peek::Revision(r) => Some(r),
            Peek::Unsupported => None,
            Peek::Failed(e) | Peek::LocalInvalid(e) => {
                return Err(e.context("hash workspace folder"))
            }
        };
        Ok(LoadOutcome {
            config,
            warnings,
            revision,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn peek_revision(&self, budget: &LoadBudget) -> Peek {
        Self::compute_revision(&self.path, budget)
    }
}

/// Load a workspace folder under a [`LoadBudget`] (blocking).
pub fn load_folder_workspace_with(
    path: &Path,
    budget: &LoadBudget,
) -> Result<(WorkspaceConfig, Vec<String>)> {
    let (workspace, warnings) = stroem_common::workspace_loader::load_workspace_with(path, budget)?;
    tracing::debug!(
        "Loaded workspace: {} actions, {} tasks, {} triggers, {} secrets, {} connection_types, {} connections",
        workspace.actions.len(),
        workspace.tasks.len(),
        workspace.triggers.len(),
        workspace.secrets.len(),
        workspace.connection_types.len(),
        workspace.connections.len()
    );
    Ok((workspace, warnings))
}

/// Load workspace from a folder containing workflow YAML files.
/// Looks for .workflows/ subdirectory, or scans the folder itself if it contains YAML files.
/// Returns the config paired with per-file warnings for files that were skipped.
pub async fn load_folder_workspace(path: &str) -> Result<(WorkspaceConfig, Vec<String>)> {
    load_folder_workspace_with(Path::new(path), &LoadBudget::unbounded())
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
        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
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
    type: script
    script: "echo Hello"

tasks:
  hello:
    flow:
      step1:
        action: greet
"#;
        fs::write(&workflow_path, yaml).unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
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
    type: script
    script: "echo 1"
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
    type: script
    script: "echo 2"
tasks:
  task2:
    flow:
      step1:
        action: action2
"#,
        )
        .unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
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
    type: script
    script: "echo test"
"#,
        )
        .unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
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
    type: script
    script: "echo test"
"#,
        )
        .unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(workspace.actions.len(), 1);
    }

    #[tokio::test]
    async fn test_nonexistent_path() {
        let result = load_folder_workspace("/nonexistent/path/12345").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_folder_source_revision() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo hello"
"#,
        )
        .unwrap();

        let source = FolderSource::new(temp_dir.path().to_str().unwrap());
        let out = source.load(&LoadBudget::unbounded()).unwrap();
        assert!(out.revision.is_some()); // revision set after load
    }

    #[test]
    fn test_compute_revision_deterministic() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            "actions:\n  greet:\n    type: script\n    script: echo hi\n",
        )
        .unwrap();

        let rev1 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };
        let rev2 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(rev1, rev2, "Same content should produce same revision");
    }

    #[test]
    fn test_compute_revision_changes_on_content_change() {
        let temp_dir = TempDir::new().unwrap();
        let yaml_path = temp_dir.path().join("test.yaml");

        fs::write(
            &yaml_path,
            "actions:\n  greet:\n    type: script\n    script: echo v1\n",
        )
        .unwrap();
        let rev1 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

        fs::write(
            &yaml_path,
            "actions:\n  greet:\n    type: script\n    script: echo v2\n",
        )
        .unwrap();
        let rev2 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

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
            "actions:\n  a:\n    type: script\n    script: echo a\n",
        )
        .unwrap();
        let rev1 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

        fs::write(
            temp_dir.path().join("b.yaml"),
            "actions:\n  b:\n    type: script\n    script: echo b\n",
        )
        .unwrap();
        let rev2 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

        assert_ne!(rev1, rev2, "Adding a file should change revision");
    }

    #[test]
    fn test_compute_revision_changes_on_file_removed() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("a.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo a\n",
        )
        .unwrap();
        let b_path = temp_dir.path().join("b.yaml");
        fs::write(
            &b_path,
            "actions:\n  b:\n    type: script\n    script: echo b\n",
        )
        .unwrap();
        let rev1 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

        fs::remove_file(&b_path).unwrap();
        let rev2 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

        assert_ne!(rev1, rev2, "Removing a file should change revision");
    }

    #[test]
    fn test_compute_revision_includes_non_yaml_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo a\n",
        )
        .unwrap();
        let rev1 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

        // Add a non-YAML file — revision SHOULD change (scripts, configs, etc.)
        fs::write(temp_dir.path().join("run.sh"), "#!/bin/bash\necho hello").unwrap();
        let rev2 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

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
            "actions:\n  a:\n    type: script\n    script: echo a\n",
        )
        .unwrap();

        let rev = FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded());
        assert!(matches!(rev, Peek::Revision(_)));

        // Adding a script in root should change revision (all files are hashed)
        fs::write(temp_dir.path().join("deploy.sh"), "#!/bin/bash\ndeploy").unwrap();
        let rev2 = FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded());
        let (Peek::Revision(r), Peek::Revision(r2)) = (rev, rev2) else {
            panic!("expected Revision variants");
        };
        assert_ne!(
            r, r2,
            "All workspace files should affect revision, not just .workflows/"
        );
    }

    #[test]
    fn test_compute_revision_includes_subdirectory_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(
            temp_dir.path().join("test.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo a\n",
        )
        .unwrap();
        let rev1 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

        // Add a file in a subdirectory — revision should change
        let sub_dir = temp_dir.path().join("scripts");
        fs::create_dir(&sub_dir).unwrap();
        fs::write(sub_dir.join("helper.py"), "print('hello')").unwrap();
        let rev2 = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };

        assert_ne!(rev1, rev2, "Files in subdirectories should affect revision");
    }

    #[test]
    fn test_compute_revision_empty_dir_returns_some() {
        let temp_dir = TempDir::new().unwrap();
        // No YAML files at all
        let rev = FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded());
        // Should still return a Revision (hash of empty input)
        assert!(matches!(rev, Peek::Revision(_)));
    }

    #[test]
    fn test_compute_revision_nonexistent_dir_returns_local_invalid() {
        let rev = FolderSource::compute_revision(
            Path::new("/nonexistent/path/12345"),
            &LoadBudget::unbounded(),
        );
        assert!(matches!(rev, Peek::LocalInvalid(_)));
    }

    #[tokio::test]
    async fn test_folder_source_reload_updates_config_and_revision() {
        let temp_dir = TempDir::new().unwrap();
        let yaml_path = temp_dir.path().join("test.yaml");

        // Initial load with one action
        fs::write(
            &yaml_path,
            "actions:\n  greet:\n    type: script\n    script: echo v1\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        )
        .unwrap();

        let source = FolderSource::new(temp_dir.path().to_str().unwrap());
        let out1 = source.load(&LoadBudget::unbounded()).unwrap();
        let config1 = out1.config;
        let rev1 = out1.revision.unwrap();

        assert_eq!(config1.actions.len(), 1);
        assert!(config1.actions.contains_key("greet"));

        // Modify file: add another action
        fs::write(
            &yaml_path,
            "actions:\n  greet:\n    type: script\n    script: echo v2\n  build:\n    type: script\n    script: make\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        )
        .unwrap();

        let out2 = source.load(&LoadBudget::unbounded()).unwrap();
        let config2 = out2.config;
        let rev2 = out2.revision.unwrap();

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
            "actions:\n  a:\n    type: script\n    script: echo a\n",
        )
        .unwrap();

        let rev = match FolderSource::compute_revision(temp_dir.path(), &LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("{other:?}"),
        };
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
        // The file is skipped with a warning rather than failing the whole workspace.
        let temp_dir = TempDir::new().unwrap();

        // Add a regular file so the workspace isn't empty
        fs::write(
            temp_dir.path().join("regular.yaml"),
            "actions:\n  test:\n    type: script\n    script: echo hi\n",
        )
        .unwrap();

        // Add a file named *.sops.yaml — this triggers the SOPS code path
        fs::write(
            temp_dir.path().join("secrets.sops.yaml"),
            "actions:\n  secret:\n    type: script\n    script: echo secret\n",
        )
        .unwrap();

        let result = load_folder_workspace(temp_dir.path().to_str().unwrap()).await;
        // Should succeed — the SOPS file is skipped with a warning
        assert!(
            result.is_ok(),
            "Workspace should load despite SOPS failure: {:?}",
            result.err()
        );
        let (workspace, warnings) = result.unwrap();
        // The regular file is loaded; the SOPS file is skipped
        assert_eq!(
            workspace.actions.len(),
            1,
            "Only the regular file's action should be loaded"
        );
        assert!(workspace.actions.contains_key("test"));
        // One warning for the SOPS file that couldn't be decrypted
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert!(
            warning.contains("sops"),
            "Warning should mention sops: {}",
            warning
        );
    }

    #[tokio::test]
    async fn test_sops_config_file_ignored_as_sops() {
        // .sops.yaml (dot-prefixed) is a SOPS config file, not an encrypted file.
        // It should be read as plain YAML (not decrypted).
        let temp_dir = TempDir::new().unwrap();

        // Write .sops.yaml — serde_yaml will parse it; unknown fields are
        // silently ignored, producing an empty WorkspaceConfig.
        fs::write(
            temp_dir.path().join(".sops.yaml"),
            "creation_rules:\n  - path_regex: \\.sops\\.yaml$\n    age: age1test\n",
        )
        .unwrap();

        // Also add a real workflow so we can verify it loads
        fs::write(
            temp_dir.path().join("workflow.yaml"),
            "actions:\n  greet:\n    type: script\n    script: echo hi\n",
        )
        .unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
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
        type: script
        script: "echo Hello"
      say-bye:
        type: docker
        image: alpine:latest
        command: ["echo", "bye"]
        depends_on: [say-hi]
"#;
        fs::write(&workflow_path, yaml).unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
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
        assert_eq!(hi.action_type, "script");
        assert_eq!(hi.script.as_deref(), Some("echo Hello"));

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
    type: script
    script: "echo Hello"

tasks:
  mixed:
    flow:
      ref-step:
        action: greet
      inline-step:
        type: script
        script: "echo Inline"
"#;
        fs::write(&workflow_path, yaml).unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
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

    #[tokio::test]
    async fn test_subdirectory_infers_task_folder() {
        let temp_dir = TempDir::new().unwrap();
        let workflows_dir = temp_dir.path().join(".workflows");
        let data_dir = workflows_dir.join("data");
        fs::create_dir_all(&data_dir).unwrap();

        // Task in root — no folder inferred
        fs::write(
            workflows_dir.join("root.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo hi"
tasks:
  root-task:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        // Task in data/ subdirectory — folder "data" inferred
        fs::write(
            data_dir.join("etl.yaml"),
            r#"
tasks:
  etl-job:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        let root_task = workspace.tasks.get("root-task").unwrap();
        assert_eq!(
            root_task.folder, None,
            "Root-level tasks should have no folder"
        );

        let etl_task = workspace.tasks.get("etl-job").unwrap();
        assert_eq!(
            etl_task.folder,
            Some("data".to_string()),
            "Subdirectory tasks should get folder from path"
        );
    }

    #[tokio::test]
    async fn test_subdirectory_does_not_override_explicit_folder() {
        let temp_dir = TempDir::new().unwrap();
        let workflows_dir = temp_dir.path().join(".workflows");
        let sub_dir = workflows_dir.join("infra");
        fs::create_dir_all(&sub_dir).unwrap();

        fs::write(
            workflows_dir.join("actions.yaml"),
            "actions:\n  deploy:\n    type: script\n    script: echo deploy\n",
        )
        .unwrap();

        // Task with explicit folder in a subdirectory — explicit wins
        fs::write(
            sub_dir.join("deploy.yaml"),
            r#"
tasks:
  deploy-prod:
    folder: production
    flow:
      step1:
        action: deploy
"#,
        )
        .unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        let task = workspace.tasks.get("deploy-prod").unwrap();
        assert_eq!(
            task.folder,
            Some("production".to_string()),
            "Explicit folder should not be overridden by subdirectory path"
        );
    }

    #[tokio::test]
    async fn test_nested_subdirectory_folder() {
        let temp_dir = TempDir::new().unwrap();
        let workflows_dir = temp_dir.path().join(".workflows");
        let nested_dir = workflows_dir.join("deploy").join("staging");
        fs::create_dir_all(&nested_dir).unwrap();

        fs::write(
            workflows_dir.join("actions.yaml"),
            "actions:\n  run:\n    type: script\n    script: echo run\n",
        )
        .unwrap();

        fs::write(
            nested_dir.join("tasks.yaml"),
            r#"
tasks:
  staging-deploy:
    flow:
      step1:
        action: run
"#,
        )
        .unwrap();

        let (workspace, _) = load_folder_workspace(temp_dir.path().to_str().unwrap())
            .await
            .unwrap();

        let task = workspace.tasks.get("staging-deploy").unwrap();
        assert_eq!(
            task.folder,
            Some("deploy/staging".to_string()),
            "Nested subdirectory should produce slash-separated folder path"
        );
    }

    // ─── Task 9: Peek classification ────────────────────────────────────

    #[test]
    fn folder_peek_on_a_missing_root_is_local_invalid() {
        let source = FolderSource::new("/nonexistent/stroem-peek-root");
        assert!(matches!(
            source.peek_revision(&LoadBudget::unbounded()),
            Peek::LocalInvalid(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn folder_peek_on_an_unreadable_file_is_failed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("w.yaml");
        std::fs::write(&file, "actions: {}\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&file).is_ok() {
            return; // running as root: permissions are not enforced
        }
        let source = FolderSource::new(dir.path().to_str().unwrap());
        let peek = source.peek_revision(&LoadBudget::unbounded());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(peek, Peek::Failed(_)), "{peek:?}");
    }

    #[cfg(unix)]
    #[test]
    fn folder_peek_on_a_symlink_to_an_unreadable_dir_is_failed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("w.yaml"), "actions: {}\n").unwrap();
        // The target lives OUTSIDE the workspace root, so the only way the walk
        // reaches it is through the symlink. The link points INTO the mode-000
        // directory, so following it fails with EACCES on the link's own path —
        // a symlink error that is neither dangling nor a loop. (A link straight
        // to `secret` already failed: walkdir's path-less loop-check error.)
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret");
        let inner = secret.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("f.yaml"), "actions: {}\n").unwrap();
        std::os::unix::fs::symlink(&inner, dir.path().join("link")).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&secret).is_ok() {
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();
            return; // running as root: permissions are not enforced
        }
        let source = FolderSource::new(dir.path().to_str().unwrap());
        let peek = source.peek_revision(&LoadBudget::unbounded());
        let load = source.load(&LoadBudget::unbounded());
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(peek, Peek::Failed(_)), "{peek:?}");
        assert!(load.is_err(), "load must fail too");
    }

    #[test]
    fn folder_peek_matches_the_loaded_revision() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("w.yaml"), "actions: {}\n").unwrap();
        let source = FolderSource::new(dir.path().to_str().unwrap());
        let loaded = source
            .load(&LoadBudget::unbounded())
            .unwrap()
            .revision
            .unwrap();
        match source.peek_revision(&LoadBudget::unbounded()) {
            Peek::Revision(r) => assert_eq!(r, loaded),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn folder_with_a_dangling_symlink_loads_and_peeks_stably() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("w.yaml"), "actions: {}\n").unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", dir.path().join(".#lock")).unwrap();

        let source = FolderSource::new(dir.path().to_str().unwrap());
        let loaded = source
            .load(&LoadBudget::unbounded())
            .unwrap()
            .revision
            .unwrap();

        let peeked = match source.peek_revision(&LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("expected Revision, got {other:?}"),
        };
        assert_eq!(
            peeked, loaded,
            "a dangling symlink must not move the revision"
        );

        // Stable across repeated peeks too.
        let peeked_again = match source.peek_revision(&LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("expected Revision, got {other:?}"),
        };
        assert_eq!(peeked_again, loaded);
    }

    #[cfg(unix)]
    #[test]
    fn folder_with_a_symlink_loop_loads() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("w.yaml"), "actions: {}\n").unwrap();
        // A link to its own parent directory — following it loops forever.
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();

        let source = FolderSource::new(dir.path().to_str().unwrap());
        let out = source.load(&LoadBudget::unbounded()).unwrap();
        assert!(out.revision.is_some());

        assert!(matches!(
            source.peek_revision(&LoadBudget::unbounded()),
            Peek::Revision(_)
        ));
    }
}
