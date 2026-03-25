use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;

pub async fn cmd_cancel(client: &Client, server: &str, job_id: &str) -> Result<()> {
    let resp = client
        .post(format!("{}/api/jobs/{}/cancel", server, job_id))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let result_status = body
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    println!("Job {}: {}", job_id, result_status);
    Ok(())
}
