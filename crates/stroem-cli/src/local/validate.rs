use anyhow::{Context, Result};
use std::path::Path;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_common::validation::validate_workflow_config;
use stroem_common::workspace_loader;

/// A loaded workspace and the outcome of validating it.
struct Checked {
    config: WorkspaceConfig,
    load_warnings: Vec<String>,
    /// `None` when the workspace defines nothing to validate.
    validation: Option<Result<Vec<String>>>,
}

impl Checked {
    fn is_empty(&self) -> bool {
        self.validation.is_none()
    }
}

/// Load and validate. Only a LOAD failure is an `Err` here — it keeps its own
/// context — so the caller can still report the load warnings when validation fails.
fn check_workspace(path: &Path) -> Result<Checked> {
    let (config, load_warnings) = workspace_loader::load_workspace(path)
        .with_context(|| format!("Failed to load workspace from {}", path.display()))?;

    let empty = config.actions.is_empty() && config.tasks.is_empty() && config.triggers.is_empty();
    let validation = (!empty).then(|| validate_workflow_config(&config));

    Ok(Checked {
        config,
        load_warnings,
        validation,
    })
}

pub fn cmd_validate(path: &str) -> Result<()> {
    let path = Path::new(path);

    let checked = check_workspace(path)?;

    for w in &checked.load_warnings {
        println!("  WARN: {}", w);
    }

    if checked.is_empty() {
        println!("No workflow definitions found at {}", path.display());
        return Ok(());
    }

    match checked
        .validation
        .expect("non-empty workspace is validated")
    {
        Ok(warnings) => {
            for w in &warnings {
                println!("  WARN: {}", w);
            }
            println!(
                "[OK] Workspace loaded: {} actions, {} tasks, {} triggers",
                checked.config.actions.len(),
                checked.config.tasks.len(),
                checked.config.triggers.len()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("[FAIL] {:#}", e);
            anyhow::bail!("Validation failed");
        }
    }
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

        let checked = check_workspace(dir.path()).unwrap();
        let warnings = checked.validation.unwrap().unwrap();

        // Verify warning is present with full text including " offline"
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("cannot validate cross-workspace task reference offline")),
            "Expected warning about cross-workspace task reference offline, got: {:?}",
            warnings
        );
        assert!(cmd_validate(dir.path().to_str().unwrap()).is_ok());
    }

    #[test]
    fn validate_load_failure_keeps_its_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-workspace");

        let err = cmd_validate(missing.to_str().unwrap()).unwrap_err();
        let chain = format!("{err:#}");

        assert!(
            err.to_string().contains("Failed to load workspace from"),
            "{chain}"
        );
        assert!(chain.contains("does not exist"), "{chain}");
        assert!(!chain.contains("Validation failed"), "{chain}");
    }

    #[test]
    fn validate_failure_still_reports_load_warnings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.yaml"), "not: [valid: yaml: {{{").unwrap();
        std::fs::write(
            dir.path().join("tasks.yaml"),
            "tasks:\n  broken:\n    flow:\n      step1:\n        action: missing_action\n",
        )
        .unwrap();

        let checked = check_workspace(dir.path()).unwrap();

        assert!(
            checked
                .load_warnings
                .iter()
                .any(|w| w.contains("failed to parse YAML")),
            "{:?}",
            checked.load_warnings
        );
        assert!(checked.validation.unwrap().is_err());

        let err = cmd_validate(dir.path().to_str().unwrap()).unwrap_err();
        assert_eq!(err.to_string(), "Validation failed");
    }
}
