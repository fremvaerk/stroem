use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "stroem", version, about = "Strøm CLI - workflow orchestration")]
struct Cli {
    /// Server URL
    #[arg(long, env = "STROEM_URL", default_value = "http://localhost:8080")]
    server: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate workflow YAML files
    Validate {
        /// Path to workflow file or directory
        path: String,
    },
    /// Trigger a task execution
    Trigger {
        /// Workspace name
        #[arg(long, default_value = "default")]
        workspace: String,
        /// Task name
        task: String,
        /// Input as JSON string
        #[arg(long)]
        input: Option<String>,
    },
    /// Get job status
    Status {
        /// Job ID
        job_id: String,
    },
    /// Stream job logs
    Logs {
        /// Job ID
        job_id: String,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// List tasks
    Tasks {
        /// Workspace name
        #[arg(long, default_value = "default")]
        workspace: String,
    },
    /// List jobs
    Jobs {
        /// Workspace filter
        #[arg(long)]
        workspace: Option<String>,
        /// Maximum number of jobs to show
        #[arg(long, default_value = "20")]
        limit: i64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Validate { path } => {
            validate_workflows(&path)?;
        }
        Commands::Trigger {
            workspace,
            task,
            input,
        } => {
            println!(
                "TODO: Trigger task '{}' in workspace '{}' on {}",
                task, workspace, cli.server
            );
            if let Some(input) = input {
                println!("  Input: {}", input);
            }
        }
        Commands::Status { job_id } => {
            println!("TODO: Get status for job '{}' from {}", job_id, cli.server);
        }
        Commands::Logs { job_id, follow } => {
            println!(
                "TODO: Get logs for job '{}' from {} (follow={})",
                job_id, cli.server, follow
            );
        }
        Commands::Tasks { workspace } => {
            println!(
                "TODO: List tasks in workspace '{}' from {}",
                workspace, cli.server
            );
        }
        Commands::Jobs { workspace, limit } => {
            println!(
                "TODO: List jobs (workspace={:?}, limit={}) from {}",
                workspace, limit, cli.server
            );
        }
    }

    Ok(())
}

fn validate_workflows(path: &str) -> Result<()> {
    use std::path::Path;
    use stroem_common::models::workflow::WorkflowConfig;
    use stroem_common::validation::validate_workflow_config;

    let path = Path::new(path);

    let files: Vec<_> = if path.is_dir() {
        std::fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "yaml" || ext == "yml")
                    .unwrap_or(false)
            })
            .map(|e| e.path())
            .collect()
    } else {
        vec![path.to_path_buf()]
    };

    if files.is_empty() {
        println!("No YAML files found at {}", path.display());
        return Ok(());
    }

    let mut all_valid = true;

    for file in &files {
        let content = std::fs::read_to_string(file)?;
        match serde_yml::from_str::<WorkflowConfig>(&content) {
            Ok(config) => match validate_workflow_config(&config) {
                Ok(warnings) => {
                    println!("[OK] {}", file.display());
                    for w in warnings {
                        println!("  WARN: {}", w);
                    }
                }
                Err(e) => {
                    println!("[FAIL] {}: {}", file.display(), e);
                    all_valid = false;
                }
            },
            Err(e) => {
                println!("[FAIL] {}: parse error: {}", file.display(), e);
                all_valid = false;
            }
        }
    }

    if !all_valid {
        anyhow::bail!("Validation failed for one or more files");
    }

    Ok(())
}
