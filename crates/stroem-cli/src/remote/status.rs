use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::fmt::Write;

use super::client::check_response;

pub async fn cmd_status(client: &Client, server: &str, job_id: &str, json: bool) -> Result<()> {
    print!("{}", status_text(client, server, job_id, json).await?);
    Ok(())
}

/// What `stroem-api status` prints: the raw job detail with `--json`, else
/// a summary with the job's input and output and each step's output.
pub async fn status_text(
    client: &Client,
    server: &str,
    job_id: &str,
    json: bool,
) -> Result<String> {
    let resp = client
        .get(format!("{}/api/jobs/{}", server, job_id))
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    check_response(&status, &body)?;

    if json {
        let mut text = serde_json::to_string_pretty(&body)?;
        text.push('\n');
        return Ok(text);
    }
    Ok(render_status(&body))
}

fn render_status(body: &Value) -> String {
    let field = |key: &str| body.get(key).and_then(|v| v.as_str()).unwrap_or("-");
    let mut out = String::new();
    let _ = writeln!(out, "Job:       {}", field("job_id"));
    let _ = writeln!(out, "Workspace: {}", field("workspace"));
    let _ = writeln!(out, "Task:      {}", field("task_name"));
    let _ = writeln!(out, "Status:    {}", field("status"));
    let _ = writeln!(out, "Mode:      {}", field("mode"));
    let _ = writeln!(out, "Created:   {}", field("created_at"));

    if let Some(started) = body.get("started_at").and_then(|v| v.as_str()) {
        let _ = writeln!(out, "Started:   {}", started);
    }
    if let Some(completed) = body.get("completed_at").and_then(|v| v.as_str()) {
        let _ = writeln!(out, "Done:      {}", completed);
    }

    for (label, key) in [("Input", "input"), ("Output", "output")] {
        if let Some(v) = body.get(key).filter(|v| !v.is_null()) {
            let _ = writeln!(out, "\n{label}:");
            let _ = writeln!(out, "{}", indented(v, "  "));
        }
    }

    if let Some(steps) = body.get("steps").and_then(|v| v.as_array()) {
        if !steps.is_empty() {
            let _ = writeln!(out, "\nSteps:");
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

                let _ = write!(out, "  {:20} {:12} (action: {})", name, st, action);
                if let Some(e) = err {
                    let _ = write!(out, "  ERROR: {}", e);
                }
                let _ = writeln!(out);
                if let Some(v) = step.get("output").filter(|v| !v.is_null()) {
                    let _ = writeln!(out, "    output:");
                    let _ = writeln!(out, "{}", indented(v, "      "));
                }
            }
        }
    }
    out
}

/// `v` as pretty JSON, every line prefixed with `prefix`.
fn indented(v: &Value, prefix: &str) -> String {
    let pretty = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    pretty
        .lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn detail() -> Value {
        json!({
            "job_id": "j1",
            "workspace": "default",
            "task_name": "report",
            "status": "completed",
            "mode": "distributed",
            "created_at": "2026-11-21T10:00:00+00:00",
            "input": {"name": "world"},
            "output": {"greeting": "hello"},
            "steps": [
                {"step_name": "build", "status": "completed", "action_name": "noop",
                 "output": {"rows": 3}},
                {"step_name": "notify", "status": "failed", "action_name": "send",
                 "error_message": "exit 1", "output": null}
            ]
        })
    }

    #[test]
    fn render_prints_input_output_and_step_outputs() {
        let text = render_status(&detail());
        assert!(
            text.contains("Input:\n  {\n    \"name\": \"world\"\n  }"),
            "{text}"
        );
        assert!(
            text.contains("Output:\n  {\n    \"greeting\": \"hello\"\n  }"),
            "{text}"
        );
        assert!(
            text.contains("    output:\n      {\n        \"rows\": 3\n      }"),
            "{text}"
        );
        assert!(text.contains("ERROR: exit 1"), "{text}");
        // A null output prints nothing.
        assert_eq!(text.matches("output:").count(), 1, "{text}");
    }

    #[test]
    fn render_skips_absent_input_and_output() {
        let text = render_status(&json!({"job_id": "j", "status": "pending", "steps": []}));
        assert!(!text.contains("Input:"), "{text}");
        assert!(!text.contains("Output:"), "{text}");
        assert!(!text.contains("Steps:"), "{text}");
    }

    #[tokio::test]
    async fn json_flag_prints_the_raw_detail() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail()))
            .mount(&server)
            .await;
        let client = Client::new();
        let text = status_text(&client, &server.uri(), "j1", true).await?;
        let parsed: Value = serde_json::from_str(&text)?;
        assert_eq!(parsed, detail());
        let text = status_text(&client, &server.uri(), "j1", false).await?;
        assert!(text.starts_with("Job:       j1\n"), "{text}");
        Ok(())
    }

    #[tokio::test]
    async fn server_error_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"error": "Job not found"})),
            )
            .mount(&server)
            .await;
        let err = status_text(&Client::new(), &server.uri(), "j1", true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Job not found"), "{err}");
    }
}
