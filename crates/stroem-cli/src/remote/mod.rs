pub mod cancel;
pub mod client;
pub mod jobs;
pub mod logs;
pub mod state;
pub mod status;
pub mod tasks;
pub mod trigger;
pub mod workspaces;

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
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
    /// Manage workspace state snapshots
    State {
        #[command(subcommand)]
        action: StateAction,
    },
}

#[derive(Subcommand)]
pub enum StateAction {
    /// Upload a task state snapshot
    Upload {
        /// Workspace name
        #[arg(long, short, default_value = "default")]
        workspace: String,
        /// Task name
        #[arg(long, short)]
        task: String,
        /// Upload mode: replace (default) or merge
        #[arg(long, default_value = "replace")]
        mode: String,
        /// State values as KEY=VALUE (repeatable)
        #[arg(long = "state", value_parser = parse_kv)]
        state: Vec<(String, String)>,
        /// Files to include in the tarball
        files: Vec<std::path::PathBuf>,
    },
    /// Upload a global workspace state snapshot (admin only)
    UploadGlobal {
        /// Workspace name
        #[arg(long, short, default_value = "default")]
        workspace: String,
        /// Upload mode: replace (default) or merge
        #[arg(long, default_value = "replace")]
        mode: String,
        /// State values as KEY=VALUE (repeatable)
        #[arg(long = "state", value_parser = parse_kv)]
        state: Vec<(String, String)>,
        /// Files to include in the tarball
        files: Vec<std::path::PathBuf>,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => Err(format!("expected KEY=VALUE, got '{}'", s)),
    }
}

pub async fn dispatch(command: Commands, server: &str, token: Option<&str>) -> Result<()> {
    let http_client = client::build_client(token)?;

    match command {
        Commands::Trigger {
            task,
            workspace,
            input,
        } => {
            trigger::cmd_trigger(&http_client, server, &workspace, &task, input.as_deref()).await?;
        }
        Commands::Status { job_id } => {
            status::cmd_status(&http_client, server, &job_id).await?;
        }
        Commands::Logs { job_id } => {
            logs::cmd_logs(&http_client, server, &job_id).await?;
        }
        Commands::Tasks { workspace } => {
            tasks::cmd_tasks(&http_client, server, workspace.as_deref()).await?;
        }
        Commands::Jobs { limit } => {
            jobs::cmd_jobs(&http_client, server, limit).await?;
        }
        Commands::Cancel { job_id } => {
            cancel::cmd_cancel(&http_client, server, &job_id).await?;
        }
        Commands::Workspaces => {
            workspaces::cmd_workspaces(&http_client, server).await?;
        }
        Commands::State { action } => {
            state::dispatch(&http_client, server, action).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    // Re-use the actual Cli struct from stroem_api to test argument parsing
    #[derive(Parser)]
    #[command(name = "stroem-api")]
    struct TestCli {
        #[arg(long, default_value = "http://localhost:8080")]
        server: String,
        #[arg(long)]
        token: Option<String>,
        #[command(subcommand)]
        command: super::Commands,
    }

    fn parse(args: &[&str]) -> Result<TestCli, clap::Error> {
        TestCli::try_parse_from(args)
    }

    #[test]
    fn default_server_url_is_localhost_8080() {
        let cli = parse(&["stroem-api", "workspaces"]).unwrap();
        assert_eq!(cli.server, "http://localhost:8080");
    }

    #[test]
    fn custom_server_url_is_accepted() {
        let cli = parse(&["stroem-api", "--server", "http://prod:9000", "workspaces"]).unwrap();
        assert_eq!(cli.server, "http://prod:9000");
    }

    #[test]
    fn workspaces_subcommand_is_recognised() {
        let cli = parse(&["stroem-api", "workspaces"]).unwrap();
        assert!(matches!(cli.command, super::Commands::Workspaces));
    }

    #[test]
    fn tasks_subcommand_workspace_defaults_to_none() {
        let cli = parse(&["stroem-api", "tasks"]).unwrap();
        match cli.command {
            super::Commands::Tasks { workspace } => assert!(workspace.is_none()),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn tasks_subcommand_accepts_workspace_flag() {
        let cli = parse(&["stroem-api", "tasks", "--workspace", "my-ws"]).unwrap();
        match cli.command {
            super::Commands::Tasks { workspace } => {
                assert_eq!(workspace.as_deref(), Some("my-ws"))
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn tasks_subcommand_accepts_short_workspace_flag() {
        let cli = parse(&["stroem-api", "tasks", "-w", "my-ws"]).unwrap();
        match cli.command {
            super::Commands::Tasks { workspace } => {
                assert_eq!(workspace.as_deref(), Some("my-ws"))
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn jobs_subcommand_default_limit_is_20() {
        let cli = parse(&["stroem-api", "jobs"]).unwrap();
        match cli.command {
            super::Commands::Jobs { limit } => assert_eq!(limit, 20),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn jobs_subcommand_accepts_custom_limit() {
        let cli = parse(&["stroem-api", "jobs", "--limit", "50"]).unwrap();
        match cli.command {
            super::Commands::Jobs { limit } => assert_eq!(limit, 50),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn trigger_subcommand_requires_task_argument() {
        let result = parse(&["stroem-api", "trigger"]);
        assert!(result.is_err());
    }

    #[test]
    fn trigger_subcommand_default_workspace_is_default() {
        let cli = parse(&["stroem-api", "trigger", "my-task"]).unwrap();
        match cli.command {
            super::Commands::Trigger {
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
            "stroem-api",
            "trigger",
            "deploy",
            "--workspace",
            "prod",
            "--input",
            r#"{"env":"staging"}"#,
        ])
        .unwrap();
        match cli.command {
            super::Commands::Trigger {
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

    #[test]
    fn cancel_subcommand_captures_job_id() {
        let cli = parse(&["stroem-api", "cancel", "abc-123"]).unwrap();
        match cli.command {
            super::Commands::Cancel { job_id } => assert_eq!(job_id, "abc-123"),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn cancel_subcommand_requires_job_id() {
        let result = parse(&["stroem-api", "cancel"]);
        assert!(result.is_err());
    }

    #[test]
    fn status_subcommand_captures_job_id() {
        let cli = parse(&["stroem-api", "status", "abc-123"]).unwrap();
        match cli.command {
            super::Commands::Status { job_id } => assert_eq!(job_id, "abc-123"),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn status_subcommand_requires_job_id() {
        let result = parse(&["stroem-api", "status"]);
        assert!(result.is_err());
    }

    #[test]
    fn logs_subcommand_captures_job_id() {
        let cli = parse(&["stroem-api", "logs", "job-xyz"]).unwrap();
        match cli.command {
            super::Commands::Logs { job_id } => assert_eq!(job_id, "job-xyz"),
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn logs_subcommand_requires_job_id() {
        let result = parse(&["stroem-api", "logs"]);
        assert!(result.is_err());
    }

    #[test]
    fn trigger_subcommand_accepts_short_workspace_flag() {
        let cli = parse(&["stroem-api", "trigger", "deploy", "-w", "prod"]).unwrap();
        match cli.command {
            super::Commands::Trigger {
                task,
                workspace,
                input,
            } => {
                assert_eq!(task, "deploy");
                assert_eq!(workspace, "prod");
                assert!(input.is_none());
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn token_flag_defaults_to_none() {
        let cli = parse(&["stroem-api", "workspaces"]).unwrap();
        assert!(cli.token.is_none());
    }

    #[test]
    fn token_flag_is_captured() {
        let cli = parse(&["stroem-api", "--token", "strm_abc123", "workspaces"]).unwrap();
        assert_eq!(cli.token, Some("strm_abc123".to_string()));
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        let result = parse(&["stroem-api", "frobulate"]);
        assert!(result.is_err());
    }

    #[test]
    fn no_subcommand_is_rejected() {
        let result = parse(&["stroem-api"]);
        assert!(result.is_err());
    }

    #[test]
    fn state_upload_subcommand_parses() {
        let cli = parse(&[
            "stroem-api",
            "state",
            "upload",
            "--workspace", "prod",
            "--task", "renew-ssl",
            "--state", "domain=example.com",
            "--state", "expiry=2026-07-21",
            "fullchain.pem",
            "privkey.pem",
        ])
        .unwrap();
        match cli.command {
            super::Commands::State {
                action: super::StateAction::Upload { workspace, task, state, files, mode, .. },
            } => {
                assert_eq!(workspace, "prod");
                assert_eq!(task, "renew-ssl");
                assert_eq!(mode, "replace");
                assert_eq!(state.len(), 2);
                assert_eq!(state[0].0, "domain");
                assert_eq!(state[0].1, "example.com");
                assert_eq!(files.len(), 2);
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn state_upload_global_subcommand_parses() {
        let cli = parse(&[
            "stroem-api",
            "state",
            "upload-global",
            "--workspace", "prod",
            "--mode", "merge",
            "--state", "api_token=secret",
        ])
        .unwrap();
        match cli.command {
            super::Commands::State {
                action: super::StateAction::UploadGlobal { workspace, mode, state, files },
            } => {
                assert_eq!(workspace, "prod");
                assert_eq!(mode, "merge");
                assert_eq!(state.len(), 1);
                assert!(files.is_empty());
            }
            _ => panic!("unexpected command variant"),
        }
    }
}
