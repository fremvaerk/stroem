pub mod actions;
pub mod inspect;
pub mod run;
pub mod tasks;
pub mod triggers;
pub mod validate;

use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::Path;
use stroem_common::workspace_loader;

#[derive(Subcommand)]
pub enum Commands {
    /// Run a task locally without a server
    Run {
        /// Task name
        task: String,
        /// Input as JSON string
        #[arg(long)]
        input: Option<String>,
    },
    /// Validate workflow YAML files
    Validate {
        /// Path to workspace root or specific YAML file (default: uses --path)
        #[arg(name = "target")]
        target: Option<String>,
    },
    /// List tasks in the workspace
    Tasks,
    /// List actions in the workspace
    Actions,
    /// List triggers in the workspace
    Triggers,
    /// Inspect a task (detailed view)
    Inspect {
        /// Task name
        task: String,
    },
}

/// Load workspace config from path, used by tasks/actions/triggers/inspect commands.
fn load_workspace_config(path: &str) -> Result<stroem_common::models::workflow::WorkspaceConfig> {
    let p = Path::new(path);
    let (config, warnings) = workspace_loader::load_workspace(p)
        .with_context(|| format!("Failed to load workspace from {}", p.display()))?;
    for w in &warnings {
        eprintln!("  WARN: {}", w);
    }
    Ok(config)
}

pub async fn dispatch(command: Commands, path: &str) -> Result<()> {
    match command {
        Commands::Run { task, input } => {
            let success = run::cmd_run(&task, path, input.as_deref()).await?;
            if !success {
                std::process::exit(1);
            }
        }
        Commands::Validate { target } => {
            let p = target.as_deref().unwrap_or(path);
            validate::cmd_validate(p)?;
        }
        Commands::Tasks => {
            let config = load_workspace_config(path)?;
            tasks::cmd_tasks(&config)?;
        }
        Commands::Actions => {
            let config = load_workspace_config(path)?;
            actions::cmd_actions(&config)?;
        }
        Commands::Triggers => {
            let config = load_workspace_config(path)?;
            triggers::cmd_triggers(&config)?;
        }
        Commands::Inspect { task } => {
            let config = load_workspace_config(path)?;
            inspect::cmd_inspect(&config, &task)?;
        }
    }
    Ok(())
}
