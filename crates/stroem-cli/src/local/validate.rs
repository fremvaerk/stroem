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
            eprintln!("[FAIL] {}", crate::local::error_report::full_report(&e));
            anyhow::bail!("Validation failed");
        }
    }
}

#[cfg(test)]
mod tests;
