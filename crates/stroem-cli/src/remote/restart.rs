//! `stroem-api restart`: Restart From Step / Restart failed steps
//! (`POST /api/jobs/{id}/restart`).

use anyhow::{Context, Result};
use clap::{ArgGroup, Args};
use reqwest::Client;
use serde_json::{json, Value};

use super::client::{api_url, check_response};

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("mode").required(true).args(["from_step", "all_failed"])))]
pub struct RestartArgs {
    /// Job ID of the source job (a finished, top-level job)
    pub job_id: String,
    /// Restart from this flow step: it and every step that depends on it run again
    #[arg(long)]
    pub from_step: Option<String>,
    /// Restart every failed or cancelled step and every step that depends on them
    #[arg(long)]
    pub all_failed: bool,
    /// Show the plan only; create no job
    #[arg(long)]
    pub dry_run: bool,
}

/// The request body: exactly one mode (clap's `mode` group), and `dry_run`.
pub fn restart_body(args: &RestartArgs) -> Value {
    match &args.from_step {
        Some(step) => json!({"from_step": step, "dry_run": args.dry_run}),
        None => json!({"all_failed": true, "dry_run": args.dry_run}),
    }
}

/// The plan (and the new job) as text, one line per list.
pub fn format_restart(body: &Value, dry_run: bool) -> String {
    let list = |key: &str| {
        let names: Vec<&str> = body[key]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if names.is_empty() {
            "-".to_string()
        } else {
            names.join(", ")
        }
    };
    let mut out = if dry_run {
        "Dry run: no job created\n".to_string()
    } else {
        format!(
            "Job created: {}\n",
            body["job_id"].as_str().unwrap_or("unknown")
        )
    };
    out.push_str(&format!("{:<16}{}\n", "Start steps:", list("start_steps")));
    out.push_str(&format!(
        "{:<16}{}\n",
        "Restart steps:",
        list("restart_steps")
    ));
    out.push_str(&format!(
        "{:<16}{}\n",
        "Carried over:",
        list("carried_over")
    ));
    if list("carried_failed") != "-" {
        out.push_str(&format!(
            "{:<16}{}",
            "Carried failed:",
            list("carried_failed")
        ));
        if list("carried_failed_tolerated") != "-" {
            out.push_str(&format!(
                " (tolerated: {})",
                list("carried_failed_tolerated")
            ));
        }
        out.push('\n');
    }
    out
}

pub async fn cmd_restart(client: &Client, server: &str, args: &RestartArgs) -> Result<()> {
    let resp = client
        .post(api_url(server, &["api", "jobs", &args.job_id, "restart"])?)
        .json(&restart_body(args))
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;
    check_response(&status, &body)?;
    print!("{}", format_restart(&body, args.dry_run));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn args(from_step: Option<&str>, all_failed: bool, dry_run: bool) -> RestartArgs {
        RestartArgs {
            job_id: "j1".into(),
            from_step: from_step.map(str::to_string),
            all_failed,
            dry_run,
        }
    }

    #[test]
    fn restart_body_names_one_mode() {
        assert_eq!(
            restart_body(&args(Some("b"), false, false)),
            json!({"from_step": "b", "dry_run": false})
        );
        assert_eq!(
            restart_body(&args(None, true, true)),
            json!({"all_failed": true, "dry_run": true})
        );
    }

    #[test]
    fn format_restart_lists_the_plan() {
        let body = json!({
            "job_id": "new-1",
            "start_steps": ["b", "c"],
            "restart_steps": ["b", "c", "d"],
            "carried_over": ["a"],
            "carried_failed": [],
            "carried_failed_tolerated": [],
        });
        assert_eq!(
            format_restart(&body, false),
            "Job created: new-1\n\
             Start steps:    b, c\n\
             Restart steps:  b, c, d\n\
             Carried over:   a\n"
        );
        let plan = json!({
            "start_steps": ["b"],
            "restart_steps": ["b"],
            "carried_over": ["a", "c"],
            "carried_failed": ["c"],
            "carried_failed_tolerated": ["c"],
        });
        assert_eq!(
            format_restart(&plan, true),
            "Dry run: no job created\n\
             Start steps:    b\n\
             Restart steps:  b\n\
             Carried over:   a, c\n\
             Carried failed: c (tolerated: c)\n"
        );
    }

    #[tokio::test]
    async fn restart_posts_the_mode_and_prints_the_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/jobs/j1/restart"))
            .and(body_json(json!({"all_failed": true, "dry_run": false})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "job_id": "new-1", "start_steps": ["b"], "restart_steps": ["b"],
                "carried_over": ["a"], "carried_failed": [], "carried_failed_tolerated": []
            })))
            .expect(1)
            .mount(&server)
            .await;
        cmd_restart(&Client::new(), &server.uri(), &args(None, true, false))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restart_error_shows_the_server_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/jobs/j1/restart"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(json!({"error": "Job is not in a terminal state"})),
            )
            .mount(&server)
            .await;
        let err = cmd_restart(
            &Client::new(),
            &server.uri(),
            &args(Some("a"), false, false),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Server returned 409 Conflict: Job is not in a terminal state"
        );
    }
}
