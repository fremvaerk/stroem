use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;

pub async fn cmd_workspaces(client: &Client, server: &str) -> Result<()> {
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
