//! Job actions: `restart`, `rerun`, `approve`, `reject`. Flattened into
//! [`super::Commands`], so they are top-level `stroem-api` subcommands.

use anyhow::Result;
use clap::Subcommand;
use reqwest::Client;

use super::approve::{ApproveArgs, RejectArgs};
use super::rerun::RerunArgs;
use super::restart::RestartArgs;

#[derive(Subcommand, Debug)]
pub enum JobActionCommands {
    /// Restart a finished job from one step (--from-step) or every failed step (--all-failed)
    Restart(RestartArgs),
    /// Re-run a job with its stored input; --input / --file change fields
    Rerun(RerunArgs),
    /// Approve a suspended step (an approval gate or an agent question)
    Approve(ApproveArgs),
    /// Reject a suspended approval step
    Reject(RejectArgs),
}

pub async fn dispatch(client: &Client, server: &str, command: JobActionCommands) -> Result<()> {
    match command {
        JobActionCommands::Restart(args) => {
            super::restart::cmd_restart(client, server, &args).await
        }
        JobActionCommands::Rerun(args) => super::rerun::cmd_rerun(client, server, &args).await,
        JobActionCommands::Approve(args) => {
            super::approve::cmd_approve(client, server, &args).await
        }
        JobActionCommands::Reject(args) => super::approve::cmd_reject(client, server, &args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    #[command(name = "stroem-api")]
    struct TestCli {
        #[command(subcommand)]
        command: crate::remote::Commands,
    }

    fn parse(args: &[&str]) -> Result<JobActionCommands, clap::Error> {
        match TestCli::try_parse_from(args)?.command {
            crate::remote::Commands::JobActions(c) => Ok(c),
            _ => panic!("not a job action: {args:?}"),
        }
    }

    #[test]
    fn restart_needs_exactly_one_mode() {
        match parse(&["stroem-api", "restart", "j1", "--from-step", "b"]).unwrap() {
            JobActionCommands::Restart(a) => {
                assert_eq!(a.job_id, "j1");
                assert_eq!(a.from_step.as_deref(), Some("b"));
                assert!(!a.all_failed && !a.dry_run);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["stroem-api", "restart", "j1", "--all-failed", "--dry-run"]).unwrap() {
            JobActionCommands::Restart(a) => assert!(a.all_failed && a.dry_run),
            other => panic!("{other:?}"),
        }
        assert!(parse(&["stroem-api", "restart", "j1"]).is_err());
        assert!(parse(&[
            "stroem-api",
            "restart",
            "j1",
            "--from-step",
            "b",
            "--all-failed"
        ])
        .is_err());
        assert!(parse(&["stroem-api", "restart", "--all-failed"]).is_err());
    }

    #[test]
    fn rerun_takes_input_and_files() {
        match parse(&[
            "stroem-api",
            "rerun",
            "j1",
            "--input",
            r#"{"a":1}"#,
            "--file",
            "f=./x",
            "--file",
            "g=./y",
        ])
        .unwrap()
        {
            JobActionCommands::Rerun(a) => {
                assert_eq!(a.job_id, "j1");
                assert_eq!(a.input.as_deref(), Some(r#"{"a":1}"#));
                assert_eq!(a.files, vec!["f=./x", "g=./y"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&["stroem-api", "rerun"]).is_err());
    }

    #[test]
    fn approve_and_reject_take_job_and_step() {
        match parse(&["stroem-api", "approve", "j1", "ask", "--input", "{}"]).unwrap() {
            JobActionCommands::Approve(a) => {
                assert_eq!((a.job_id.as_str(), a.step.as_str()), ("j1", "ask"));
                assert_eq!(a.input.as_deref(), Some("{}"));
            }
            other => panic!("{other:?}"),
        }
        match parse(&["stroem-api", "reject", "j1", "ask", "--reason", "no"]).unwrap() {
            JobActionCommands::Reject(a) => assert_eq!(a.reason.as_deref(), Some("no")),
            other => panic!("{other:?}"),
        }
        assert!(parse(&["stroem-api", "approve", "j1"]).is_err());
        assert!(parse(&["stroem-api", "reject", "j1"]).is_err());
    }
}
