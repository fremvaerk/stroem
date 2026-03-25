use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;

pub async fn cmd_logs(client: &Client, server: &str, job_id: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs/{}/logs", server, job_id))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let logs = body.get("logs").and_then(|v| v.as_str()).unwrap_or("");

    print!("{}", logs);
    Ok(())
}
