use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use serde_json::Value;

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
    /// Get job logs
    Logs {
        /// Job ID
        job_id: String,
    },
    /// List tasks
    Tasks,
    /// List jobs
    Jobs {
        /// Maximum number of jobs to show
        #[arg(long, default_value = "20")]
        limit: i64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let client = Client::new();

    match cli.command {
        Commands::Validate { path } => {
            validate_workflows(&path)?;
        }
        Commands::Trigger { task, input } => {
            cmd_trigger(&client, &cli.server, &task, input.as_deref()).await?;
        }
        Commands::Status { job_id } => {
            cmd_status(&client, &cli.server, &job_id).await?;
        }
        Commands::Logs { job_id } => {
            cmd_logs(&client, &cli.server, &job_id).await?;
        }
        Commands::Tasks => {
            cmd_tasks(&client, &cli.server).await?;
        }
        Commands::Jobs { limit } => {
            cmd_jobs(&client, &cli.server, limit).await?;
        }
    }

    Ok(())
}

async fn cmd_trigger(client: &Client, server: &str, task: &str, input: Option<&str>) -> Result<()> {
    let body: Value = match input {
        Some(json_str) => {
            let input_val: Value = serde_json::from_str(json_str).context("Invalid JSON input")?;
            serde_json::json!({ "input": input_val })
        }
        None => serde_json::json!({ "input": {} }),
    };

    let resp = client
        .post(format!("{}/api/tasks/{}/execute", server, task))
        .json(&body)
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        anyhow::bail!("Server returned {}: {}", status, err);
    }

    let job_id = body
        .get("job_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    println!("Job created: {}", job_id);
    Ok(())
}

async fn cmd_status(client: &Client, server: &str, job_id: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs/{}", server, job_id))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        anyhow::bail!("Server returned {}: {}", status, err);
    }

    println!(
        "Job:    {}",
        body.get("job_id").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Task:   {}",
        body.get("task_name")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );
    println!(
        "Status: {}",
        body.get("status").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Mode:   {}",
        body.get("mode").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Created: {}",
        body.get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );

    if let Some(started) = body.get("started_at").and_then(|v| v.as_str()) {
        println!("Started: {}", started);
    }
    if let Some(completed) = body.get("completed_at").and_then(|v| v.as_str()) {
        println!("Done:    {}", completed);
    }

    // Print steps
    if let Some(steps) = body.get("steps").and_then(|v| v.as_array()) {
        if !steps.is_empty() {
            println!("\nSteps:");
            for step in steps {
                let name = step
                    .get("step_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                let st = step.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                let action = step
                    .get("action_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                let err = step.get("error_message").and_then(|v| v.as_str());

                print!("  {:20} {:12} (action: {})", name, st, action);
                if let Some(e) = err {
                    print!("  ERROR: {}", e);
                }
                println!();
            }
        }
    }

    Ok(())
}

async fn cmd_logs(client: &Client, server: &str, job_id: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs/{}/logs", server, job_id))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        anyhow::bail!("Server returned {}: {}", status, err);
    }

    let logs = body.get("logs").and_then(|v| v.as_str()).unwrap_or("");

    print!("{}", logs);
    Ok(())
}

async fn cmd_tasks(client: &Client, server: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/tasks", server))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        anyhow::bail!("Server returned {}: {}", status, err);
    }

    let tasks = body.as_array().context("Expected array response")?;

    if tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    println!("{:30} MODE", "NAME");
    println!("{}", "-".repeat(45));
    for task in tasks {
        let name = task.get("name").and_then(|v| v.as_str()).unwrap_or("-");
        let mode = task.get("mode").and_then(|v| v.as_str()).unwrap_or("-");
        println!("{:30} {}", name, mode);
    }

    Ok(())
}

async fn cmd_jobs(client: &Client, server: &str, limit: i64) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs?limit={}", server, limit))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        anyhow::bail!("Server returned {}: {}", status, err);
    }

    let jobs = body.as_array().context("Expected array response")?;

    if jobs.is_empty() {
        println!("No jobs found.");
        return Ok(());
    }

    println!("{:36} {:20} {:12} CREATED", "JOB ID", "TASK", "STATUS");
    println!("{}", "-".repeat(85));
    for job in jobs {
        let id = job.get("job_id").and_then(|v| v.as_str()).unwrap_or("-");
        let task = job.get("task_name").and_then(|v| v.as_str()).unwrap_or("-");
        let st = job.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let created = job
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        println!("{:36} {:20} {:12} {}", id, task, st, created);
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
