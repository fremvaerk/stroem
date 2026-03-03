use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use serde_json::Value;

fn check_response(status: &reqwest::StatusCode, body: &serde_json::Value) -> anyhow::Result<()> {
    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        anyhow::bail!("Server returned {}: {}", status, err);
    }
    Ok(())
}

#[derive(Parser)]
#[command(name = "stroem", version, about = "Strøm CLI - workflow orchestration")]
struct Cli {
    /// Server URL
    #[arg(long, env = "STROEM_URL", default_value = "http://localhost:8080")]
    server: String,

    /// Authentication token (API key or JWT). Can also be set via STROEM_TOKEN env var.
    #[arg(long, env = "STROEM_TOKEN")]
    token: Option<String>,

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
        /// Workspace name
        #[arg(long, short, default_value = "default")]
        workspace: String,
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
    Tasks {
        /// Workspace name (lists all workspaces if not specified)
        #[arg(long, short)]
        workspace: Option<String>,
    },
    /// List jobs
    Jobs {
        /// Maximum number of jobs to show
        #[arg(long, default_value = "20")]
        limit: i64,
    },
    /// Cancel a running or pending job
    Cancel {
        /// Job ID
        job_id: String,
    },
    /// List workspaces
    Workspaces,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let client = build_client(cli.token.as_deref())?;

    match cli.command {
        Commands::Validate { path } => {
            validate_workflows(&path)?;
        }
        Commands::Trigger {
            task,
            workspace,
            input,
        } => {
            cmd_trigger(&client, &cli.server, &workspace, &task, input.as_deref()).await?;
        }
        Commands::Status { job_id } => {
            cmd_status(&client, &cli.server, &job_id).await?;
        }
        Commands::Logs { job_id } => {
            cmd_logs(&client, &cli.server, &job_id).await?;
        }
        Commands::Tasks { workspace } => {
            cmd_tasks(&client, &cli.server, workspace.as_deref()).await?;
        }
        Commands::Cancel { job_id } => {
            cmd_cancel(&client, &cli.server, &job_id).await?;
        }
        Commands::Jobs { limit } => {
            cmd_jobs(&client, &cli.server, limit).await?;
        }
        Commands::Workspaces => {
            cmd_workspaces(&client, &cli.server).await?;
        }
    }

    Ok(())
}

/// Build HTTP client, optionally with a default Authorization header.
fn build_client(token: Option<&str>) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = token {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))
                .context("Invalid token value")?,
        );
    }
    Client::builder()
        .default_headers(headers)
        .build()
        .context("Failed to build HTTP client")
}

async fn cmd_trigger(
    client: &Client,
    server: &str,
    workspace: &str,
    task: &str,
    input: Option<&str>,
) -> Result<()> {
    let body: Value = build_trigger_body(input)?;

    let resp = client
        .post(trigger_url(server, workspace, task))
        .json(&body)
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let job_id = body
        .get("job_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    println!("Job created: {}", job_id);
    Ok(())
}

async fn cmd_cancel(client: &Client, server: &str, job_id: &str) -> Result<()> {
    let resp = client
        .post(format!("{}/api/jobs/{}/cancel", server, job_id))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let result_status = body
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    println!("Job {}: {}", job_id, result_status);
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

    check_response(&status, &body)?;

    println!(
        "Job:       {}",
        body.get("job_id").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Workspace: {}",
        body.get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );
    println!(
        "Task:      {}",
        body.get("task_name")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );
    println!(
        "Status:    {}",
        body.get("status").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Mode:      {}",
        body.get("mode").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Created:   {}",
        body.get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );

    if let Some(started) = body.get("started_at").and_then(|v| v.as_str()) {
        println!("Started:   {}", started);
    }
    if let Some(completed) = body.get("completed_at").and_then(|v| v.as_str()) {
        println!("Done:      {}", completed);
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

    check_response(&status, &body)?;

    let logs = body.get("logs").and_then(|v| v.as_str()).unwrap_or("");

    print!("{}", logs);
    Ok(())
}

async fn cmd_tasks(client: &Client, server: &str, workspace: Option<&str>) -> Result<()> {
    match workspace {
        Some(ws) => {
            // List tasks for a specific workspace
            let resp = client
                .get(format!("{}/api/workspaces/{}/tasks", server, ws))
                .send()
                .await
                .context("Failed to connect to server")?;

            let status = resp.status();
            let body: Value = resp.json().await.context("Failed to parse response")?;

            check_response(&status, &body)?;

            let tasks = body.as_array().context("Expected array response")?;

            if tasks.is_empty() {
                println!("No tasks found in workspace '{}'.", ws);
                return Ok(());
            }

            println!("{:30} {:15} {:20} MODE", "NAME", "WORKSPACE", "FOLDER");
            println!("{}", "-".repeat(75));
            for task in tasks {
                let name = task.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                let mode = task.get("mode").and_then(|v| v.as_str()).unwrap_or("-");
                let folder = task.get("folder").and_then(|v| v.as_str()).unwrap_or("-");
                println!("{:30} {:15} {:20} {}", name, ws, folder, mode);
            }
        }
        None => {
            // List all workspaces and their tasks
            let ws_resp = client
                .get(format!("{}/api/workspaces", server))
                .send()
                .await
                .context("Failed to connect to server")?;

            let ws_status = ws_resp.status();
            let ws_body: Value = ws_resp.json().await.context("Failed to parse response")?;

            check_response(&ws_status, &ws_body)?;

            let workspaces = ws_body.as_array().context("Expected array response")?;

            println!("{:30} {:15} {:20} MODE", "NAME", "WORKSPACE", "FOLDER");
            println!("{}", "-".repeat(75));

            let mut total = 0;
            for ws in workspaces {
                let ws_name = ws.get("name").and_then(|v| v.as_str()).unwrap_or("-");

                let resp = client
                    .get(format!("{}/api/workspaces/{}/tasks", server, ws_name))
                    .send()
                    .await
                    .context("Failed to connect to server")?;

                if resp.status().is_success() {
                    let body: Value = resp.json().await.context("Failed to parse response")?;
                    if let Some(tasks) = body.as_array() {
                        for task in tasks {
                            let name = task.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                            let mode = task.get("mode").and_then(|v| v.as_str()).unwrap_or("-");
                            let folder = task.get("folder").and_then(|v| v.as_str()).unwrap_or("-");
                            println!("{:30} {:15} {:20} {}", name, ws_name, folder, mode);
                            total += 1;
                        }
                    }
                }
            }

            if total == 0 {
                println!("No tasks found.");
            }
        }
    }

    Ok(())
}

async fn cmd_jobs(client: &Client, server: &str, limit: i64) -> Result<()> {
    let resp = client
        .get(jobs_url(server, limit))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let jobs = body["items"]
        .as_array()
        .context("Expected items array in response")?;

    if jobs.is_empty() {
        println!("No jobs found.");
        return Ok(());
    }

    println!(
        "{:36} {:20} {:15} {:12} CREATED",
        "JOB ID", "TASK", "WORKSPACE", "STATUS"
    );
    println!("{}", "-".repeat(100));
    for job in jobs {
        let id = job.get("job_id").and_then(|v| v.as_str()).unwrap_or("-");
        let task = job.get("task_name").and_then(|v| v.as_str()).unwrap_or("-");
        let ws = job.get("workspace").and_then(|v| v.as_str()).unwrap_or("-");
        let st = job.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let created = job
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        println!("{:36} {:20} {:15} {:12} {}", id, task, ws, st, created);
    }

    Ok(())
}

async fn cmd_workspaces(client: &Client, server: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/workspaces", server))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let workspaces = body.as_array().context("Expected array response")?;

    if workspaces.is_empty() {
        println!("No workspaces found.");
        return Ok(());
    }

    println!("{:20} {:10} ACTIONS", "NAME", "TASKS");
    println!("{}", "-".repeat(40));
    for ws in workspaces {
        let name = ws.get("name").and_then(|v| v.as_str()).unwrap_or("-");
        let tasks = ws.get("tasks_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let actions = ws
            .get("actions_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!("{:20} {:10} {}", name, tasks, actions);
    }

    Ok(())
}

/// Builds the trigger request body from an optional JSON input string.
///
/// Returns `{"input": <parsed_json>}` when input is provided, or `{"input": {}}` otherwise.
fn build_trigger_body(input: Option<&str>) -> Result<serde_json::Value> {
    match input {
        Some(json_str) => {
            let input_val: serde_json::Value =
                serde_json::from_str(json_str).context("Invalid JSON input")?;
            Ok(serde_json::json!({ "input": input_val }))
        }
        None => Ok(serde_json::json!({ "input": {} })),
    }
}

/// Constructs the URL for triggering a task execution.
fn trigger_url(server: &str, workspace: &str, task: &str) -> String {
    format!(
        "{}/api/workspaces/{}/tasks/{}/execute",
        server, workspace, task
    )
}

/// Constructs the URL for fetching jobs with an optional limit.
fn jobs_url(server: &str, limit: i64) -> String {
    format!("{}/api/jobs?limit={}", server, limit)
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
        let content = stroem_common::sops::read_yaml_file(file)?;
        match serde_yaml::from_str::<WorkflowConfig>(&content) {
            Ok(config) => match validate_workflow_config(&config) {
                Ok(warnings) => {
                    println!("[OK] {}", file.display());
                    for w in warnings {
                        println!("  WARN: {}", w);
                    }
                }
                Err(e) => {
                    println!("[FAIL] {}: {:#}", file.display(), e);
                    all_valid = false;
                }
            },
            Err(e) => {
                println!("[FAIL] {}: parse error: {:#}", file.display(), e);
                all_valid = false;
            }
        }
    }

    if !all_valid {
        anyhow::bail!("Validation failed for one or more files");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ---------------------------------------------------------------------------
    // Argument parsing helpers
    // ---------------------------------------------------------------------------

    /// Parse a `Cli` from a slice of string arguments, returning an error if
    /// clap rejects the input.  The first element must be the program name.
    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    // ---------------------------------------------------------------------------
    // Default server URL
    // ---------------------------------------------------------------------------

    #[test]
    fn default_server_url_is_localhost_8080() {
        let cli = parse(&["stroem", "workspaces"]).unwrap();
        assert_eq!(cli.server, "http://localhost:8080");
    }

    #[test]
    fn custom_server_url_is_accepted() {
        let cli = parse(&["stroem", "--server", "http://prod:9000", "workspaces"]).unwrap();
        assert_eq!(cli.server, "http://prod:9000");
    }

    // ---------------------------------------------------------------------------
    // Subcommand: workspaces
    // ---------------------------------------------------------------------------

    #[test]
    fn workspaces_subcommand_is_recognised() {
        let cli = parse(&["stroem", "workspaces"]).unwrap();
        assert!(matches!(cli.command, Commands::Workspaces));
    }

    // ---------------------------------------------------------------------------
    // Subcommand: tasks
    // ---------------------------------------------------------------------------

    #[test]
    fn tasks_subcommand_workspace_defaults_to_none() {
        let cli = parse(&["stroem", "tasks"]).unwrap();
        match cli.command {
            Commands::Tasks { workspace } => assert!(workspace.is_none()),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn tasks_subcommand_accepts_workspace_flag() {
        let cli = parse(&["stroem", "tasks", "--workspace", "my-ws"]).unwrap();
        match cli.command {
            Commands::Tasks { workspace } => assert_eq!(workspace.as_deref(), Some("my-ws")),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn tasks_subcommand_accepts_short_workspace_flag() {
        let cli = parse(&["stroem", "tasks", "-w", "my-ws"]).unwrap();
        match cli.command {
            Commands::Tasks { workspace } => assert_eq!(workspace.as_deref(), Some("my-ws")),
            _ => panic!("unexpected command variant"),
        }
    }

    // ---------------------------------------------------------------------------
    // Subcommand: jobs
    // ---------------------------------------------------------------------------

    #[test]
    fn jobs_subcommand_default_limit_is_20() {
        let cli = parse(&["stroem", "jobs"]).unwrap();
        match cli.command {
            Commands::Jobs { limit } => assert_eq!(limit, 20),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn jobs_subcommand_accepts_custom_limit() {
        let cli = parse(&["stroem", "jobs", "--limit", "50"]).unwrap();
        match cli.command {
            Commands::Jobs { limit } => assert_eq!(limit, 50),
            _ => panic!("unexpected command variant"),
        }
    }

    // ---------------------------------------------------------------------------
    // Subcommand: trigger
    // ---------------------------------------------------------------------------

    #[test]
    fn trigger_subcommand_requires_task_argument() {
        let result = parse(&["stroem", "trigger"]);
        assert!(result.is_err());
    }

    #[test]
    fn trigger_subcommand_default_workspace_is_default() {
        let cli = parse(&["stroem", "trigger", "my-task"]).unwrap();
        match cli.command {
            Commands::Trigger {
                task,
                workspace,
                input,
            } => {
                assert_eq!(task, "my-task");
                assert_eq!(workspace, "default");
                assert!(input.is_none());
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn trigger_subcommand_accepts_workspace_and_input() {
        let cli = parse(&[
            "stroem",
            "trigger",
            "deploy",
            "--workspace",
            "prod",
            "--input",
            r#"{"env":"staging"}"#,
        ])
        .unwrap();
        match cli.command {
            Commands::Trigger {
                task,
                workspace,
                input,
            } => {
                assert_eq!(task, "deploy");
                assert_eq!(workspace, "prod");
                assert_eq!(input.as_deref(), Some(r#"{"env":"staging"}"#));
            }
            _ => panic!("unexpected command variant"),
        }
    }

    // ---------------------------------------------------------------------------
    // Subcommand: cancel
    // ---------------------------------------------------------------------------

    #[test]
    fn cancel_subcommand_captures_job_id() {
        let cli = parse(&["stroem", "cancel", "abc-123"]).unwrap();
        match cli.command {
            Commands::Cancel { job_id } => assert_eq!(job_id, "abc-123"),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn cancel_subcommand_requires_job_id() {
        let result = parse(&["stroem", "cancel"]);
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------------------
    // Subcommand: status
    // ---------------------------------------------------------------------------

    #[test]
    fn status_subcommand_captures_job_id() {
        let cli = parse(&["stroem", "status", "abc-123"]).unwrap();
        match cli.command {
            Commands::Status { job_id } => assert_eq!(job_id, "abc-123"),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn status_subcommand_requires_job_id() {
        let result = parse(&["stroem", "status"]);
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------------------
    // Subcommand: logs
    // ---------------------------------------------------------------------------

    #[test]
    fn logs_subcommand_captures_job_id() {
        let cli = parse(&["stroem", "logs", "job-xyz"]).unwrap();
        match cli.command {
            Commands::Logs { job_id } => assert_eq!(job_id, "job-xyz"),
            _ => panic!("unexpected command variant"),
        }
    }

    // ---------------------------------------------------------------------------
    // Subcommand: validate
    // ---------------------------------------------------------------------------

    #[test]
    fn validate_subcommand_captures_path() {
        let cli = parse(&["stroem", "validate", "/tmp/workflow.yaml"]).unwrap();
        match cli.command {
            Commands::Validate { path } => assert_eq!(path, "/tmp/workflow.yaml"),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn validate_subcommand_requires_path() {
        let result = parse(&["stroem", "validate"]);
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------------------
    // Unknown subcommand
    // ---------------------------------------------------------------------------

    #[test]
    fn unknown_subcommand_is_rejected() {
        let result = parse(&["stroem", "frobulate"]);
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------------------
    // No subcommand
    // ---------------------------------------------------------------------------

    #[test]
    fn no_subcommand_is_rejected() {
        let result = parse(&["stroem"]);
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------------------
    // build_trigger_body
    // ---------------------------------------------------------------------------

    #[test]
    fn build_trigger_body_none_produces_empty_input_object() {
        let body = build_trigger_body(None).unwrap();
        assert_eq!(body, serde_json::json!({ "input": {} }));
    }

    #[test]
    fn build_trigger_body_with_json_wraps_input() {
        let body = build_trigger_body(Some(r#"{"key":"value"}"#)).unwrap();
        assert_eq!(body, serde_json::json!({ "input": { "key": "value" } }));
    }

    #[test]
    fn build_trigger_body_with_array_json_wraps_input() {
        let body = build_trigger_body(Some(r#"[1,2,3]"#)).unwrap();
        assert_eq!(body, serde_json::json!({ "input": [1, 2, 3] }));
    }

    #[test]
    fn build_trigger_body_with_invalid_json_returns_error() {
        let result = build_trigger_body(Some("{not-json}"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid JSON input"));
    }

    // ---------------------------------------------------------------------------
    // trigger_url
    // ---------------------------------------------------------------------------

    #[test]
    fn trigger_url_constructs_correct_path() {
        let url = trigger_url("http://localhost:8080", "default", "my-task");
        assert_eq!(
            url,
            "http://localhost:8080/api/workspaces/default/tasks/my-task/execute"
        );
    }

    #[test]
    fn trigger_url_includes_custom_workspace() {
        let url = trigger_url("http://prod:9000", "production", "deploy");
        assert_eq!(
            url,
            "http://prod:9000/api/workspaces/production/tasks/deploy/execute"
        );
    }

    // ---------------------------------------------------------------------------
    // jobs_url
    // ---------------------------------------------------------------------------

    #[test]
    fn jobs_url_includes_limit_parameter() {
        let url = jobs_url("http://localhost:8080", 20);
        assert_eq!(url, "http://localhost:8080/api/jobs?limit=20");
    }

    #[test]
    fn jobs_url_uses_provided_limit() {
        let url = jobs_url("http://localhost:8080", 100);
        assert_eq!(url, "http://localhost:8080/api/jobs?limit=100");
    }

    // ---------------------------------------------------------------------------
    // check_response
    // ---------------------------------------------------------------------------

    #[test]
    fn check_response_ok_on_success_status() {
        let status = reqwest::StatusCode::OK;
        let body = serde_json::json!({});
        assert!(check_response(&status, &body).is_ok());
    }

    #[test]
    fn check_response_err_on_4xx_with_error_field() {
        let status = reqwest::StatusCode::NOT_FOUND;
        let body = serde_json::json!({ "error": "task not found" });
        let err = check_response(&status, &body).unwrap_err();
        assert!(err.to_string().contains("404"));
        assert!(err.to_string().contains("task not found"));
    }

    #[test]
    fn check_response_err_on_5xx_with_fallback_message() {
        let status = reqwest::StatusCode::INTERNAL_SERVER_ERROR;
        let body = serde_json::json!({});
        let err = check_response(&status, &body).unwrap_err();
        assert!(err.to_string().contains("500"));
        assert!(err.to_string().contains("Unknown error"));
    }

    #[test]
    fn check_response_ok_on_created_status() {
        let status = reqwest::StatusCode::CREATED;
        let body = serde_json::json!({ "job_id": "abc" });
        assert!(check_response(&status, &body).is_ok());
    }

    // ---------------------------------------------------------------------------
    // validate_workflows — file-system integration (no HTTP)
    // ---------------------------------------------------------------------------

    #[test]
    fn validate_workflows_succeeds_on_valid_yaml() {
        let mut file = NamedTempFile::with_suffix(".yaml").unwrap();
        writeln!(
            file,
            r#"
tasks:
  hello:
    flow:
      step1:
        action: greet
actions:
  greet:
    type: shell
    cmd: echo hello
"#
        )
        .unwrap();
        let result = validate_workflows(file.path().to_str().unwrap());
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
    }

    #[test]
    fn validate_workflows_fails_on_invalid_yaml() {
        let mut file = NamedTempFile::with_suffix(".yaml").unwrap();
        writeln!(
            file,
            r#"
tasks:
  broken:
    flow:
      step1:
        action: nonexistent_action
"#
        )
        .unwrap();
        let result = validate_workflows(file.path().to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn validate_workflows_on_empty_directory_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        // No YAML files — should report "No YAML files found" and return Ok
        let result = validate_workflows(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_workflows_skips_non_yaml_files_in_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Write a .txt file that would be invalid YAML if parsed
        let txt = dir.path().join("not-a-workflow.txt");
        std::fs::write(&txt, "{{{{invalid").unwrap();
        // Should ignore the .txt file and see no YAML files → Ok
        let result = validate_workflows(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_workflows_validates_all_yaml_files_in_directory() {
        let dir = tempfile::tempdir().unwrap();

        // One valid workflow
        std::fs::write(
            dir.path().join("valid.yaml"),
            r#"
tasks:
  t1:
    flow:
      s1:
        action: a1
actions:
  a1:
    type: shell
    cmd: echo ok
"#,
        )
        .unwrap();

        // One invalid workflow — references a non-existent action
        std::fs::write(
            dir.path().join("invalid.yaml"),
            r#"
tasks:
  t2:
    flow:
      s1:
        action: missing_action
"#,
        )
        .unwrap();

        let result = validate_workflows(dir.path().to_str().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Validation failed"));
    }

    // ---------------------------------------------------------------------------
    // --token flag
    // ---------------------------------------------------------------------------

    #[test]
    fn token_flag_defaults_to_none() {
        let cli = parse(&["stroem", "workspaces"]).unwrap();
        assert!(cli.token.is_none());
    }

    #[test]
    fn token_flag_is_captured() {
        let cli = parse(&["stroem", "--token", "strm_abc123", "workspaces"]).unwrap();
        assert_eq!(cli.token, Some("strm_abc123".to_string()));
    }

    // ---------------------------------------------------------------------------
    // build_client
    // ---------------------------------------------------------------------------

    #[test]
    fn build_client_without_token_succeeds() {
        let result = build_client(None);
        assert!(result.is_ok());
    }

    #[test]
    fn build_client_with_token_succeeds() {
        let result = build_client(Some("strm_abc123"));
        assert!(result.is_ok());
    }
}
