use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::{check_response, jobs_url};

pub async fn cmd_jobs(client: &Client, server: &str, limit: i64) -> Result<()> {
    let resp = client
        .get(jobs_url(server, limit))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    let jobs = body["items"]
        .as_array()
        .context("Expected items array in response")?;

    if jobs.is_empty() {
        println!("No jobs found.");
        return Ok(());
    }

    println!(
        "{:36} {:20} {:15} {:12} CREATED",
        "JOB ID", "TASK", "WORKSPACE", "STATUS"
    );
    println!("{}", "-".repeat(100));
    for job in jobs {
        let id = job.get("job_id").and_then(|v| v.as_str()).unwrap_or("-");
        let task = job.get("task_name").and_then(|v| v.as_str()).unwrap_or("-");
        let ws = job.get("workspace").and_then(|v| v.as_str()).unwrap_or("-");
        let st = job.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let created = job
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        println!("{:36} {:20} {:15} {:12} {}", id, task, ws, st, created);
    }

    Ok(())
}
