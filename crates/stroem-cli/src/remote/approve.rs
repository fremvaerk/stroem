//! `stroem-api approve` / `stroem-api reject`: decide a suspended step
//! (`POST /api/jobs/{id}/steps/{step}/approve`).

use anyhow::{bail, Context, Result};
use clap::Args;
use reqwest::Client;
use serde_json::{json, Value};

use super::client::{api_url, check_response};

#[derive(Args, Debug)]
pub struct ApproveArgs {
    /// Job ID
    pub job_id: String,
    /// Name of the suspended step
    pub step: String,
    /// Values for the approval's input fields, as a JSON object. For an agent
    /// step that asked a question: '{"response": "<answer>"}'
    #[arg(long)]
    pub input: Option<String>,
}

#[derive(Args, Debug)]
pub struct RejectArgs {
    /// Job ID
    pub job_id: String,
    /// Name of the suspended step
    pub step: String,
    /// Why the step is rejected (it becomes the step's error)
    #[arg(long)]
    pub reason: Option<String>,
}

/// The approve route of one step; the step name is one encoded path segment.
pub fn approve_url(server: &str, job_id: &str, step: &str) -> Result<reqwest::Url> {
    api_url(server, &["api", "jobs", job_id, "steps", step, "approve"])
}

/// POSTs one decision and prints the result.
async fn decide(
    client: &Client,
    server: &str,
    job_id: &str,
    step: &str,
    body: Value,
) -> Result<()> {
    let resp = client
        .post(approve_url(server, job_id, step)?)
        .json(&body)
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;
    check_response(&status, &body)?;
    let result = body["status"].as_str().unwrap_or("unknown");
    println!("Step '{step}' of job {job_id}: {result}");
    Ok(())
}

pub async fn cmd_approve(client: &Client, server: &str, args: &ApproveArgs) -> Result<()> {
    let mut body = json!({"approved": true});
    if let Some(input) = &args.input {
        let input: Value = serde_json::from_str(input).context("--input is not valid JSON")?;
        if !input.is_object() {
            bail!("--input must be a JSON object");
        }
        body["input"] = input;
    }
    decide(client, server, &args.job_id, &args.step, body).await
}

pub async fn cmd_reject(client: &Client, server: &str, args: &RejectArgs) -> Result<()> {
    let mut body = json!({"approved": false});
    if let Some(reason) = &args.reason {
        body["rejection_reason"] = json!(reason);
    }
    decide(client, server, &args.job_id, &args.step, body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn approve_url_encodes_the_step_name() {
        assert_eq!(
            approve_url("http://s", "j1", "ask").unwrap().as_str(),
            "http://s/api/jobs/j1/steps/ask/approve"
        );
        assert_eq!(
            approve_url("http://s/", "j1", "gate[0]/x")
                .unwrap()
                .as_str(),
            "http://s/api/jobs/j1/steps/gate[0]%2Fx/approve"
        );
    }

    #[tokio::test]
    async fn approve_sends_the_input() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/jobs/j1/steps/ask/approve"))
            .and(body_json(
                json!({"approved": true, "input": {"reason": "ok"}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "approved"})))
            .expect(1)
            .mount(&server)
            .await;
        let args = ApproveArgs {
            job_id: "j1".into(),
            step: "ask".into(),
            input: Some(r#"{"reason": "ok"}"#.into()),
        };
        cmd_approve(&Client::new(), &server.uri(), &args)
            .await
            .unwrap();
    }

    /// No `--input`: the body has no `input` key (the server then skips the
    /// required-field check), not `"input": {}`.
    #[tokio::test]
    async fn approve_without_input_sends_no_input() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/jobs/j1/steps/ask/approve"))
            .and(body_json(json!({"approved": true})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "approved"})))
            .expect(1)
            .mount(&server)
            .await;
        let args = ApproveArgs {
            job_id: "j1".into(),
            step: "ask".into(),
            input: None,
        };
        cmd_approve(&Client::new(), &server.uri(), &args)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn approve_rejects_a_non_object_input_before_any_request() {
        let server = MockServer::start().await;
        for input in ["[1]", "\"x\"", "not json"] {
            let args = ApproveArgs {
                job_id: "j1".into(),
                step: "ask".into(),
                input: Some(input.into()),
            };
            let err = cmd_approve(&Client::new(), &server.uri(), &args)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("--input"), "{input}: {err}");
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reject_sends_the_reason() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/jobs/j1/steps/ask/approve"))
            .and(body_json(
                json!({"approved": false, "rejection_reason": "not now"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "rejected"})))
            .expect(1)
            .mount(&server)
            .await;
        let args = RejectArgs {
            job_id: "j1".into(),
            step: "ask".into(),
            reason: Some("not now".into()),
        };
        cmd_reject(&Client::new(), &server.uri(), &args)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reject_error_shows_the_server_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/jobs/j1/steps/ask/approve"))
            .and(body_json(json!({"approved": false})))
            .respond_with(ResponseTemplate::new(409).set_body_json(
                json!({"error": "Step 'ask' is not in suspended state (current: completed)"}),
            ))
            .mount(&server)
            .await;
        let args = RejectArgs {
            job_id: "j1".into(),
            step: "ask".into(),
            reason: None,
        };
        let err = cmd_reject(&Client::new(), &server.uri(), &args)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Server returned 409 Conflict: Step 'ask' is not in suspended state (current: completed)"
        );
    }
}
