//! `stroem-api workers [ID]`: the registered workers (`GET /api/workers`), or
//! one worker with its recent steps (`GET /api/workers/{id}`).

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::fmt::Write;

use super::client::check_response;
use super::paths::api_url;

#[derive(clap::Args)]
pub struct WorkersArgs {
    /// Worker ID: show this worker and its recent steps
    pub id: Option<String>,
    /// Maximum number of workers to list
    #[arg(long, default_value = "50", conflicts_with = "id")]
    pub limit: i64,
    /// Number of workers to skip, for the next page
    #[arg(long, default_value = "0", conflicts_with = "id")]
    pub offset: i64,
}

pub async fn run(client: &Client, server: &str, args: WorkersArgs) -> Result<()> {
    print!("{}", workers_text(client, server, &args).await?);
    Ok(())
}

pub async fn workers_text(client: &Client, server: &str, args: &WorkersArgs) -> Result<String> {
    match &args.id {
        Some(id) => Ok(render_worker(
            &get_json(client, api_url(server, &["workers", id])?).await?,
        )),
        None => {
            let mut url = api_url(server, &["workers"])?;
            url.query_pairs_mut()
                .append_pair("limit", &args.limit.to_string())
                .append_pair("offset", &args.offset.to_string());
            Ok(render_list(&get_json(client, url).await?))
        }
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

fn str_list(v: &Value) -> String {
    match v.as_array() {
        Some(a) if !a.is_empty() => a
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(","),
        _ => "-".to_string(),
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("-")
}

fn render_list(body: &Value) -> String {
    let items = body["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "No workers found.\n".to_string();
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:36} {:20} {:8} {:24} {:16} {:10} LAST HEARTBEAT",
        "WORKER ID", "NAME", "STATUS", "CAPABILITIES", "TAGS", "VERSION"
    );
    let _ = writeln!(out, "{}", "-".repeat(140));
    for w in &items {
        let _ = writeln!(
            out,
            "{:36} {:20} {:8} {:24} {:16} {:10} {}",
            s(w, "worker_id"),
            s(w, "name"),
            s(w, "status"),
            str_list(&w["capabilities"]),
            str_list(&w["tags"]),
            s(w, "version"),
            s(w, "last_heartbeat"),
        );
    }
    let _ = writeln!(
        out,
        "\n{} of {} worker(s)",
        items.len(),
        body["total"].as_i64().unwrap_or(items.len() as i64)
    );
    out
}

fn render_worker(w: &Value) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Worker:       {}", s(w, "worker_id"));
    let _ = writeln!(out, "Name:         {}", s(w, "name"));
    let _ = writeln!(out, "Status:       {}", s(w, "status"));
    let _ = writeln!(out, "Capabilities: {}", str_list(&w["capabilities"]));
    let _ = writeln!(out, "Tags:         {}", str_list(&w["tags"]));
    let _ = writeln!(out, "Version:      {}", s(w, "version"));
    let _ = writeln!(out, "Heartbeat:    {}", s(w, "last_heartbeat"));
    let _ = writeln!(out, "Registered:   {}", s(w, "registered_at"));
    let steps = w["steps"]["items"].as_array().cloned().unwrap_or_default();
    if steps.is_empty() {
        let _ = writeln!(out, "\nNo recent steps.");
        return out;
    }
    let _ = writeln!(out, "\nRecent steps:");
    for st in &steps {
        let _ = write!(
            out,
            "  {:36} {:15} {:20} {:20} {:10}",
            s(st, "job_id"),
            s(st, "workspace"),
            s(st, "task_name"),
            s(st, "step_name"),
            s(st, "status"),
        );
        if let Some(e) = st["error_message"].as_str() {
            let _ = write!(out, "  ERROR: {e}");
        }
        let _ = writeln!(out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn lists_a_page_of_workers() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/workers"))
            .and(query_param("limit", "1"))
            .and(query_param("offset", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"worker_id": "w1", "name": "gpu-1", "status": "active",
                           "capabilities": ["script", "docker"], "tags": [],
                           "version": "0.18.5", "last_heartbeat": "2026-11-21T10:00:00Z"}],
                "total": 3
            })))
            .expect(1)
            .mount(&server)
            .await;
        let args = WorkersArgs {
            id: None,
            limit: 1,
            offset: 2,
        };
        let text = workers_text(&Client::new(), &server.uri(), &args).await?;
        assert!(text.contains("script,docker"), "{text}");
        assert!(text.ends_with("1 of 3 worker(s)\n"), "{text}");
        Ok(())
    }

    #[tokio::test]
    async fn shows_one_worker_with_its_steps() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/workers/w1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "worker_id": "w1", "name": "gpu-1", "status": "active",
                "capabilities": ["script"], "tags": ["gpu"],
                "steps": {"items": [{"job_id": "j1", "workspace": "default",
                                     "task_name": "report", "step_name": "build",
                                     "status": "failed", "error_message": "exit 1"}],
                          "total": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let args = WorkersArgs {
            id: Some("w1".into()),
            limit: 50,
            offset: 0,
        };
        let text = workers_text(&Client::new(), &server.uri(), &args).await?;
        assert!(text.starts_with("Worker:       w1\n"), "{text}");
        assert!(text.contains("Tags:         gpu\n"), "{text}");
        assert!(text.contains("build"), "{text}");
        assert!(text.contains("ERROR: exit 1"), "{text}");
        Ok(())
    }

    #[test]
    fn empty_list_says_so() {
        assert_eq!(
            render_list(&json!({"items": [], "total": 0})),
            "No workers found.\n"
        );
    }

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        command: super::super::Commands,
    }

    #[test]
    fn id_and_paging_flags() {
        use clap::Parser;
        match Cli::try_parse_from(["stroem-api", "workers"])
            .unwrap()
            .command
        {
            super::super::Commands::Workers(a) => {
                assert!(a.id.is_none());
                assert_eq!((a.limit, a.offset), (50, 0));
            }
            _ => panic!("unexpected command variant"),
        }
        match Cli::try_parse_from(["stroem-api", "workers", "w1"])
            .unwrap()
            .command
        {
            super::super::Commands::Workers(a) => assert_eq!(a.id.as_deref(), Some("w1")),
            _ => panic!("unexpected command variant"),
        }
        assert!(Cli::try_parse_from(["stroem-api", "workers", "w1", "--offset", "5"]).is_err());
    }
}
