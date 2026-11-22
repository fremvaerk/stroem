//! `stroem-api triggers`: triggers with their last run, result and missed
//! runs (`GET /api/workspaces/{ws}/triggers`, `GET /api/triggers/attention`).

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::fmt::Write;

use super::client::api_url;
use super::client::check_response;

#[derive(clap::Args)]
pub struct TriggersArgs {
    /// Workspace name (all workspaces if not specified)
    #[arg(long, short)]
    pub workspace: Option<String>,
    /// Only the triggers that need attention (missed runs or a failed last run)
    #[arg(long)]
    pub attention: bool,
}

pub async fn run(client: &Client, server: &str, args: TriggersArgs) -> Result<()> {
    print!("{}", triggers_text(client, server, &args).await?);
    Ok(())
}

pub async fn triggers_text(client: &Client, server: &str, args: &TriggersArgs) -> Result<String> {
    let rows: Vec<(String, Value)> = if args.attention {
        get_json(client, api_url(server, &["api", "triggers", "attention"])?)
            .await?
            .as_array()
            .context("Expected array response")?
            .iter()
            .filter(|t| {
                args.workspace
                    .as_deref()
                    .is_none_or(|ws| t["workspace"].as_str() == Some(ws))
            })
            .map(|t| {
                (
                    t["workspace"].as_str().unwrap_or("-").to_string(),
                    t.clone(),
                )
            })
            .collect()
    } else {
        let workspaces = match &args.workspace {
            Some(ws) => vec![ws.clone()],
            None => get_json(client, api_url(server, &["api", "workspaces"])?)
                .await?
                .as_array()
                .context("Expected array response")?
                .iter()
                .filter_map(|w| w["name"].as_str().map(str::to_string))
                .collect(),
        };
        let mut rows = Vec::new();
        for ws in workspaces {
            let url = api_url(server, &["api", "workspaces", &ws, "triggers"])?;
            let list = match get_json_or_not_found(client, url).await? {
                Some(list) => list,
                // A workspace that does not load has no trigger list.
                None if args.workspace.is_none() => continue,
                None => anyhow::bail!("Server returned 404 Not Found: Workspace '{ws}' not found"),
            };
            for t in list.as_array().context("Expected array response")? {
                rows.push((ws.clone(), t.clone()));
            }
        }
        rows
    };

    if rows.is_empty() {
        return Ok(if args.attention {
            "No triggers need attention.\n".to_string()
        } else {
            "No triggers found.\n".to_string()
        });
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:15} {:24} {:12} {:20} {:8} {:22} {:7} LAST RUN",
        "WORKSPACE", "NAME", "TYPE", "TASK", "ENABLED", "SCHEDULE", "MISSED"
    );
    let _ = writeln!(out, "{}", "-".repeat(140));
    for (ws, t) in &rows {
        let _ = writeln!(out, "{}", format_row(ws, t));
    }
    Ok(out)
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

async fn get_json_or_not_found(client: &Client, url: reqwest::Url) -> Result<Option<Value>> {
    let resp = client
        .get(url)
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    check_response(&status, &body)?;
    Ok(Some(body))
}

fn format_row(ws: &str, t: &Value) -> String {
    let s = |key: &str| t.get(key).and_then(|v| v.as_str()).unwrap_or("-");
    let schedule = match (
        t.get("cron").and_then(|v| v.as_str()),
        t.get("timezone").and_then(|v| v.as_str()),
    ) {
        (Some(cron), Some(tz)) => format!("{cron} {tz}"),
        (Some(cron), None) => cron.to_string(),
        _ => "-".to_string(),
    };
    let enabled = if t["enabled"].as_bool().unwrap_or(true) {
        "yes"
    } else {
        "no"
    };
    let missed = match t.get("missed").filter(|m| !m.is_null()) {
        Some(m) if m["capped"].as_bool().unwrap_or(false) => {
            format!("{}+", m["count"].as_u64().unwrap_or(0))
        }
        Some(m) => m["count"].as_u64().unwrap_or(0).to_string(),
        None => "-".to_string(),
    };
    let last_run = match t.get("last_run").filter(|r| !r.is_null()) {
        Some(r) => format!(
            "{} {} ({})",
            r["status"].as_str().unwrap_or("-"),
            r["created_at"].as_str().unwrap_or("-"),
            r["job_id"].as_str().unwrap_or("-"),
        ),
        None if t["last_run_hidden"].as_bool().unwrap_or(false) => "hidden".to_string(),
        None => "never".to_string(),
    };
    format!(
        "{:15} {:24} {:12} {:20} {:8} {:22} {:7} {}",
        ws,
        s("name"),
        s("type"),
        s("task"),
        enabled,
        schedule,
        missed,
        last_run
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn nightly() -> Value {
        json!({"name": "nightly", "type": "scheduler", "cron": "0 2 * * *",
               "timezone": "Europe/Oslo", "task": "report", "enabled": true,
               "last_run": {"job_id": "j1", "status": "failed",
                            "created_at": "2026-11-20T01:00:00Z"},
               "last_run_hidden": false,
               "missed": {"count": 3, "capped": false, "first": "2026-11-21T01:00:00Z"}})
    }

    #[test]
    fn row_shows_schedule_missed_and_last_run() {
        let row = format_row("default", &nightly());
        assert!(row.contains("0 2 * * * Europe/Oslo"), "{row}");
        assert!(row.contains(" 3 "), "{row}");
        assert!(row.ends_with("failed 2026-11-20T01:00:00Z (j1)"), "{row}");

        let hook = json!({"name": "hook", "type": "webhook", "task": "report",
                          "enabled": false, "last_run": null, "last_run_hidden": true,
                          "missed": null});
        let row = format_row("default", &hook);
        assert!(row.contains(" no "), "{row}");
        assert!(row.ends_with("hidden"), "{row}");

        let capped = json!({"name": "x", "missed": {"count": 1000, "capped": true},
                            "last_run": null});
        let row = format_row("ws", &capped);
        assert!(row.contains("1000+"), "{row}");
        assert!(row.ends_with("never"), "{row}");
    }

    #[tokio::test]
    async fn lists_every_workspace_and_skips_one_that_does_not_load() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/workspaces"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"name": "default"}, {"name": "broken"}])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/workspaces/default/triggers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([nightly()])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/workspaces/broken/triggers"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"error": "Workspace 'broken' not found"})),
            )
            .mount(&server)
            .await;
        let args = TriggersArgs {
            workspace: None,
            attention: false,
        };
        let text = triggers_text(&Client::new(), &server.uri(), &args).await?;
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(
            text.lines().nth(2).unwrap().starts_with("default "),
            "{text}"
        );

        // A named workspace that does not load is an error.
        let args = TriggersArgs {
            workspace: Some("broken".into()),
            attention: false,
        };
        let err = triggers_text(&Client::new(), &server.uri(), &args)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Workspace 'broken' not found"),
            "{err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn attention_filters_by_workspace() -> Result<()> {
        let server = MockServer::start().await;
        let mut a = nightly();
        a["workspace"] = json!("default");
        let mut b = nightly();
        b["workspace"] = json!("ops");
        b["name"] = json!("backup");
        Mock::given(method("GET"))
            .and(path("/api/triggers/attention"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([a, b])))
            .mount(&server)
            .await;
        let all = TriggersArgs {
            workspace: None,
            attention: true,
        };
        let text = triggers_text(&Client::new(), &server.uri(), &all).await?;
        assert!(
            text.contains("backup") && text.contains("nightly"),
            "{text}"
        );
        let ops = TriggersArgs {
            workspace: Some("ops".into()),
            attention: true,
        };
        let text = triggers_text(&Client::new(), &server.uri(), &ops).await?;
        assert!(
            text.contains("backup") && !text.contains("nightly"),
            "{text}"
        );
        Ok(())
    }

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: super::super::Commands,
    }

    #[test]
    fn parses_workspace_and_attention_flags() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["stroem-api", "triggers", "-w", "ops", "--attention"]).unwrap();
        match cli.command {
            super::super::Commands::Triggers(a) => {
                assert_eq!(a.workspace.as_deref(), Some("ops"));
                assert!(a.attention);
            }
            _ => panic!("unexpected command variant"),
        }
        let cli = Cli::try_parse_from(["stroem-api", "triggers"]).unwrap();
        match cli.command {
            super::super::Commands::Triggers(a) => {
                assert!(a.workspace.is_none());
                assert!(!a.attention);
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[tokio::test]
    async fn empty_attention_list_says_so() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/triggers/attention"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let args = TriggersArgs {
            workspace: None,
            attention: true,
        };
        let text = triggers_text(&Client::new(), &server.uri(), &args).await?;
        assert_eq!(text, "No triggers need attention.\n");
        Ok(())
    }
}
