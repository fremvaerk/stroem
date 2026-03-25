use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::{build_trigger_body, check_response, trigger_url};

pub async fn cmd_trigger(
    client: &Client,
    server: &str,
    workspace: &str,
    task: &str,
    input: Option<&str>,
) -> Result<()> {
    let body: Value = build_trigger_body(input)?;

    let resp = client
        .post(trigger_url(server, workspace, task))
        .json(&body)
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let job_id = body
        .get("job_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    println!("Job created: {}", job_id);
    Ok(())
}
