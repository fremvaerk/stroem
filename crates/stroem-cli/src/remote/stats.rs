//! `stroem-api stats`: job counts by status (`GET /api/stats`), or a task's
//! run-time statistics (`GET /api/workspaces/{ws}/tasks/{name}/stats`).

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::fmt::Write;

use super::client::api_url;
use super::client::check_response;

#[derive(clap::Args)]
pub struct StatsArgs {
    /// Workspace of the task (with --task)
    #[arg(long, short, requires = "task")]
    pub workspace: Option<String>,
    /// Show the run-time statistics of this task instead of the job counts
    #[arg(long, requires = "workspace")]
    pub task: Option<String>,
    /// Number of most recent completed runs to aggregate (server default 50, max 500)
    #[arg(long, requires = "task")]
    pub limit: Option<i64>,
}

pub async fn run(client: &Client, server: &str, args: StatsArgs) -> Result<()> {
    print!("{}", stats_text(client, server, &args).await?);
    Ok(())
}

pub async fn stats_text(client: &Client, server: &str, args: &StatsArgs) -> Result<String> {
    match (&args.workspace, &args.task) {
        (Some(ws), Some(task)) => {
            let mut url = api_url(server, &["api", "workspaces", ws, "tasks", task, "stats"])?;
            if let Some(limit) = args.limit {
                url.query_pairs_mut()
                    .append_pair("limit", &limit.to_string());
            }
            Ok(render_task_stats(ws, task, &get_json(client, url).await?))
        }
        _ => Ok(render_counts(
            &get_json(client, api_url(server, &["api", "stats"])?).await?,
        )),
    }
}

async fn get_json(client: &Client, url: reqwest::Url) -> Result<Value> {
    let resp = client
        .get(url)
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;
    check_response(&status, &body)?;
    Ok(body)
}

const STATUSES: [&str; 6] = [
    "pending",
    "running",
    "completed",
    "failed",
    "cancelled",
    "skipped",
];

fn render_counts(body: &Value) -> String {
    let mut out = String::new();
    for status in STATUSES {
        let _ = writeln!(out, "{:10} {}", status, body[status].as_i64().unwrap_or(0));
    }
    out
}

/// Milliseconds as `850ms`, `12.3s`, `4m 05s` or `2h 03m`; `-` when absent.
fn duration(ms: &Value) -> String {
    let Some(ms) = ms.as_f64() else {
        return "-".to_string();
    };
    let secs = ms / 1000.0;
    if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        let s = secs.round() as u64;
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        let m = (secs / 60.0).round() as u64;
        format!("{}h {:02}m", m / 60, m % 60)
    }
}

fn figures(v: &Value) -> String {
    format!(
        "avg {}  p50 {}  p95 {}  min {}  max {}",
        duration(&v["avg_ms"]),
        duration(&v["p50_ms"]),
        duration(&v["p95_ms"]),
        duration(&v["min_ms"]),
        duration(&v["max_ms"]),
    )
}

fn render_task_stats(ws: &str, task: &str, body: &Value) -> String {
    let t = &body["task"];
    let n = t["sample_size"].as_i64().unwrap_or(0);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Task {ws}/{task}: {n} completed run(s) of the last {}",
        body["window"].as_i64().unwrap_or(0)
    );
    if n == 0 {
        return out;
    }
    let _ = writeln!(out, "  {}", figures(t));
    if let Some(steps) = body["steps"].as_array().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "\nSteps:");
        for s in steps {
            let _ = writeln!(
                out,
                "  {:20} n={:<5} {}",
                s["step_name"].as_str().unwrap_or("-"),
                s["sample_size"].as_i64().unwrap_or(0),
                figures(s)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn durations_are_human() {
        assert_eq!(duration(&json!(850.0)), "850ms");
        assert_eq!(duration(&json!(12_340.0)), "12.3s");
        assert_eq!(duration(&json!(245_000.0)), "4m 05s");
        assert_eq!(duration(&json!(7_380_000.0)), "2h 03m");
        assert_eq!(duration(&Value::Null), "-");
    }

    #[test]
    fn counts_list_every_status() {
        let text = render_counts(&json!({"pending": 1, "failed": 3}));
        assert_eq!(
            text,
            "pending    1\nrunning    0\ncompleted  0\nfailed     3\ncancelled  0\nskipped    0\n"
        );
    }

    #[test]
    fn task_stats_without_runs_say_so() {
        let text = render_task_stats(
            "default",
            "report",
            &json!({"window": 50, "task": {"sample_size": 0}, "steps": []}),
        );
        assert_eq!(
            text,
            "Task default/report: 0 completed run(s) of the last 50\n"
        );
    }

    #[tokio::test]
    async fn task_stats_pass_the_limit_and_show_steps() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/workspaces/default/tasks/report/stats"))
            .and(query_param("limit", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "window": 10,
                "task": {"sample_size": 3, "avg_ms": 20000.0, "p50_ms": 20000.0,
                         "p95_ms": 29000.0, "min_ms": 10000.0, "max_ms": 30000.0,
                         "recent": []},
                "steps": [{"step_name": "build", "sample_size": 3, "avg_ms": 1500.0,
                           "p50_ms": 1500.0, "p95_ms": 1900.0, "min_ms": 1000.0,
                           "max_ms": 2000.0}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let args = StatsArgs {
            workspace: Some("default".into()),
            task: Some("report".into()),
            limit: Some(10),
        };
        let text = stats_text(&Client::new(), &server.uri(), &args).await?;
        assert!(text.starts_with("Task default/report: 3 completed run(s) of the last 10\n"));
        assert!(
            text.contains("avg 20.0s  p50 20.0s  p95 29.0s  min 10.0s  max 30.0s"),
            "{text}"
        );
        assert!(
            text.contains("build                n=3     avg 1.5s"),
            "{text}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn without_a_task_it_shows_the_counts() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/stats"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"running": 2})))
            .expect(1)
            .mount(&server)
            .await;
        let args = StatsArgs {
            workspace: None,
            task: None,
            limit: None,
        };
        let text = stats_text(&Client::new(), &server.uri(), &args).await?;
        assert!(text.contains("running    2\n"), "{text}");
        Ok(())
    }

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: super::super::Commands,
    }

    #[test]
    fn task_needs_a_workspace_and_limit_needs_a_task() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["stroem-api", "stats"]).is_ok());
        assert!(Cli::try_parse_from(["stroem-api", "stats", "-w", "ops", "--task", "etl"]).is_ok());
        assert!(Cli::try_parse_from(["stroem-api", "stats", "--task", "etl"]).is_err());
        assert!(Cli::try_parse_from(["stroem-api", "stats", "-w", "ops"]).is_err());
        assert!(Cli::try_parse_from(["stroem-api", "stats", "--limit", "5"]).is_err());
    }
}
