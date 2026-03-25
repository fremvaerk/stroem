mod local;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "stroem",
    version,
    about = "Strøm — local workflow tool",
    after_help = "Remote server commands (trigger, status, logs, jobs, cancel) have moved to 'stroem-api'.\nRun: stroem-api --help"
)]
struct Cli {
    /// Path to workspace root (default: current directory)
    #[arg(long, global = true, default_value = ".")]
    path: String,

    #[command(subcommand)]
    command: local::Commands,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    local::dispatch(cli.command, &cli.path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    // --- Subcommand: run ---

    #[test]
    fn run_subcommand_requires_task() {
        let result = parse(&["stroem", "run"]);
        assert!(result.is_err());
    }

    #[test]
    fn run_subcommand_defaults() {
        let cli = parse(&["stroem", "run", "deploy"]).unwrap();
        match cli.command {
            local::Commands::Run { task, input } => {
                assert_eq!(task, "deploy");
                assert!(input.is_none());
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn run_subcommand_uses_global_path() {
        let cli = parse(&["stroem", "--path", "/workspace", "run", "deploy"]).unwrap();
        assert_eq!(cli.path, "/workspace");
        match cli.command {
            local::Commands::Run { task, input } => {
                assert_eq!(task, "deploy");
                assert!(input.is_none());
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn run_subcommand_accepts_input() {
        let cli = parse(&["stroem", "run", "deploy", "--input", r#"{"env":"staging"}"#]).unwrap();
        match cli.command {
            local::Commands::Run { task, input } => {
                assert_eq!(task, "deploy");
                assert_eq!(input.as_deref(), Some(r#"{"env":"staging"}"#));
            }
            _ => panic!("unexpected command variant"),
        }
    }

    // --- Subcommand: validate ---

    #[test]
    fn validate_subcommand_captures_path() {
        let cli = parse(&["stroem", "validate", "/tmp/workflow.yaml"]).unwrap();
        match cli.command {
            local::Commands::Validate { target } => {
                assert_eq!(target, Some("/tmp/workflow.yaml".to_string()))
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn validate_subcommand_defaults_to_none() {
        let cli = parse(&["stroem", "validate"]).unwrap();
        match cli.command {
            local::Commands::Validate { target } => assert!(target.is_none()),
            _ => panic!("unexpected command variant"),
        }
    }

    // --- New local commands ---

    #[test]
    fn tasks_subcommand_is_recognised() {
        let cli = parse(&["stroem", "tasks"]).unwrap();
        assert!(matches!(cli.command, local::Commands::Tasks));
    }

    #[test]
    fn actions_subcommand_is_recognised() {
        let cli = parse(&["stroem", "actions"]).unwrap();
        assert!(matches!(cli.command, local::Commands::Actions));
    }

    #[test]
    fn triggers_subcommand_is_recognised() {
        let cli = parse(&["stroem", "triggers"]).unwrap();
        assert!(matches!(cli.command, local::Commands::Triggers));
    }

    #[test]
    fn inspect_subcommand_requires_task() {
        let result = parse(&["stroem", "inspect"]);
        assert!(result.is_err());
    }

    #[test]
    fn inspect_subcommand_captures_task() {
        let cli = parse(&["stroem", "inspect", "deploy"]).unwrap();
        match cli.command {
            local::Commands::Inspect { task } => assert_eq!(task, "deploy"),
            _ => panic!("unexpected command variant"),
        }
    }

    // --- Global --path flag ---

    #[test]
    fn global_path_flag_default() {
        let cli = parse(&["stroem", "tasks"]).unwrap();
        assert_eq!(cli.path, ".");
    }

    #[test]
    fn global_path_flag_custom() {
        let cli = parse(&["stroem", "--path", "/my/workspace", "tasks"]).unwrap();
        assert_eq!(cli.path, "/my/workspace");
    }

    // --- Unknown subcommands ---

    #[test]
    fn unknown_subcommand_is_rejected() {
        let result = parse(&["stroem", "frobulate"]);
        assert!(result.is_err());
    }

    #[test]
    fn remote_commands_are_not_available() {
        // Remote commands should not be recognized by the local binary
        assert!(parse(&["stroem", "trigger", "foo"]).is_err());
        assert!(parse(&["stroem", "status", "abc"]).is_err());
        assert!(parse(&["stroem", "logs", "abc"]).is_err());
        assert!(parse(&["stroem", "jobs"]).is_err());
        assert!(parse(&["stroem", "cancel", "abc"]).is_err());
        assert!(parse(&["stroem", "workspaces"]).is_err());
    }

    #[test]
    fn no_subcommand_is_rejected() {
        let result = parse(&["stroem"]);
        assert!(result.is_err());
    }

    #[test]
    fn path_flag_after_subcommand() {
        let cli = parse(&["stroem", "tasks", "--path", "/foo"]).unwrap();
        assert_eq!(cli.path, "/foo");
        assert!(matches!(cli.command, local::Commands::Tasks));
    }
}
