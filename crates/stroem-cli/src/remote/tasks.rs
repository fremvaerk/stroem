use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;

pub async fn cmd_tasks(client: &Client, server: &str, workspace: Option<&str>) -> Result<()> {
    match workspace {
        Some(ws) => {
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
