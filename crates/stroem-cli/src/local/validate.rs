use anyhow::{Context, Result};
use std::path::Path;
use stroem_common::validation::validate_workflow_config;
use stroem_common::workspace_loader;

/// Pure validation function that returns all warnings (load + validation).
fn validate_workspace(path: &Path) -> Result<Vec<String>> {
    let (config, mut load_warnings) = workspace_loader::load_workspace(path)
        .with_context(|| format!("Failed to load workspace from {}", path.display()))?;

    if config.actions.is_empty() && config.tasks.is_empty() && config.triggers.is_empty() {
        return Ok(load_warnings);
    }

    let validation_warnings = validate_workflow_config(&config)?;
    load_warnings.extend(validation_warnings);
    Ok(load_warnings)
}

pub fn cmd_validate(path: &str) -> Result<()> {
    let path = Path::new(path);

    let warnings = validate_workspace(path)?;
    for w in &warnings {
        println!("  WARN: {}", w);
    }

    let (config, _) = workspace_loader::load_workspace(path)
        .with_context(|| format!("Failed to load workspace from {}", path.display()))?;

    if config.actions.is_empty() && config.tasks.is_empty() && config.triggers.is_empty() {
        println!("No workflow definitions found at {}", path.display());
        return Ok(());
    }

    println!(
        "[OK] Workspace loaded: {} actions, {} tasks, {} triggers",
        config.actions.len(),
        config.tasks.len(),
        config.triggers.len()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_empty_directory_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let result = cmd_validate(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_skips_non_yaml_files() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("not-a-workflow.txt");
        std::fs::write(&txt, "{{{{invalid").unwrap();
        let result = cmd_validate(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_cross_file_references() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("actions.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo hello
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("tasks.yaml"),
            r#"
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        let result = cmd_validate(dir.path().to_str().unwrap());
        assert!(
            result.is_ok(),
            "Cross-file references should validate: {:?}",
            result.err()
        );
    }

    #[test]
    fn validate_detects_missing_action_across_files() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("actions.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo hello
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("tasks.yaml"),
            r#"
tasks:
  broken:
    flow:
      step1:
        action: missing_action
"#,
        )
        .unwrap();

        let result = cmd_validate(dir.path().to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn validate_with_workflows_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join(".workflows");
        std::fs::create_dir(&workflows).unwrap();

        std::fs::write(
            workflows.join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo hello
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        let result = cmd_validate(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_recurses_into_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(
            sub.join("nested.yaml"),
            r#"
tasks:
  t1:
    flow:
      s1:
        action: a1
actions:
  a1:
    type: script
    script: echo ok
"#,
        )
        .unwrap();

        let result = cmd_validate(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_malformed_yaml_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.yaml"), "not: [valid: yaml: {{{").unwrap();
        // Should not panic — either Ok with warnings or Err
        let _ = cmd_validate(dir.path().to_str().unwrap());
    }

    #[test]
    fn validate_warns_on_cross_workspace_task_ref_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let wf = dir.path().join(".workflows");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(
            wf.join("x.yaml"),
            "actions:\n  run-remote:\n    type: task\n    task: B.deploy\ntasks:\n  caller:\n    flow:\n      go:\n        action: run-remote\n",
        )
        .unwrap();

        // Validate workspace and collect warnings
        let warnings = validate_workspace(dir.path()).unwrap();

        // Verify warning is present with full text including " offline"
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("cannot validate cross-workspace task reference offline")),
            "Expected warning about cross-workspace task reference offline, got: {:?}",
            warnings
        );
    }
}
