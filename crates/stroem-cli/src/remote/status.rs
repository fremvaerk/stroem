use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;

pub async fn cmd_status(client: &Client, server: &str, job_id: &str) -> Result<()> {
    let resp = client
        .get(format!("{}/api/jobs/{}", server, job_id))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    println!(
        "Job:       {}",
        body.get("job_id").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Workspace: {}",
        body.get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );
    println!(
        "Task:      {}",
        body.get("task_name")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );
    println!(
        "Status:    {}",
        body.get("status").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Mode:      {}",
        body.get("mode").and_then(|v| v.as_str()).unwrap_or("-")
    );
    println!(
        "Created:   {}",
        body.get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
    );

    if let Some(started) = body.get("started_at").and_then(|v| v.as_str()) {
        println!("Started:   {}", started);
    }
    if let Some(completed) = body.get("completed_at").and_then(|v| v.as_str()) {
        println!("Done:      {}", completed);
    }

    // Print steps
    if let Some(steps) = body.get("steps").and_then(|v| v.as_array()) {
        if !steps.is_empty() {
            println!("\nSteps:");
            for step in steps {
                let name = step
                    .get("step_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                let st = step.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                let action = step
                    .get("action_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                let err = step.get("error_message").and_then(|v| v.as_str());

                print!("  {:20} {:12} (action: {})", name, st, action);
                if let Some(e) = err {
                    print!("  ERROR: {}", e);
                }
                println!();
            }
        }
    }

    Ok(())
}
