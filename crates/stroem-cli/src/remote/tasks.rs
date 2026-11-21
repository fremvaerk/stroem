use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;
use super::paths::api_url;

pub async fn cmd_tasks(client: &Client, server: &str, workspace: Option<&str>) -> Result<()> {
    match workspace {
        Some(ws) => {
            let resp = client
                .get(api_url(server, &["workspaces", ws, "tasks"])?)
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
            let ws_resp = client
                .get(api_url(server, &["workspaces"])?)
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
                    .get(api_url(server, &["workspaces", ws_name, "tasks"])?)
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
                } else {
                    eprintln!(
                        "  WARN: Failed to fetch tasks for workspace '{}' ({})",
                        ws_name,
                        resp.status()
                    );
                }
            }

            if total == 0 {
                println!("No tasks found.");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A workspace name the server returns is one percent-encoded segment.
    #[tokio::test]
    async fn workspace_names_are_percent_encoded() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/workspaces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"name": "a b?c#d/e"}])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/workspaces/a%20b%3Fc%23d%2Fe/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(2)
            .mount(&server)
            .await;
        cmd_tasks(&Client::new(), &server.uri(), None).await?;
        cmd_tasks(&Client::new(), &server.uri(), Some("a b?c#d/e")).await?;
        Ok(())
    }
}
