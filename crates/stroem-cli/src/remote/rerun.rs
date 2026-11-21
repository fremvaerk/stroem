//! `stroem-api rerun`: Re-run a job with its stored input
//! (`POST /api/workspaces/{ws}/tasks/{task}/execute` with `source_job_id`).
//!
//! The source's workspace, task and input field names come from
//! `GET /api/jobs/{id}`. Every field of the source's input that `--input` /
//! `--file` do not set is sent in `replay_fields`, so the server copies the
//! stored value (secrets included) — the CLI never sends a (masked) value back.

use anyhow::{bail, Context, Result};
use clap::Args;
use reqwest::Client;
use serde_json::{json, Value};

use super::client::{build_trigger_body, check_response, trigger_url};
use super::trigger::{parse_file_args, upload_files};

#[derive(Args, Debug)]
pub struct RerunArgs {
    /// Job ID of the source job (a top-level job)
    pub job_id: String,
    /// Input fields to change, as a JSON object; every other field takes
    /// the source job's value
    #[arg(long)]
    pub input: Option<String>,
    /// Upload a local file for a `file` input field (repeatable)
    #[arg(long = "file", value_name = "FIELD=PATH")]
    pub files: Vec<String>,
}

/// The source's input field names that `input` does not set, sorted: the
/// `replay_fields` of the re-run. A source without a stored input object
/// (it predates Re-run prefill) gives none; the server refuses it.
pub fn replay_fields(raw_input: &Value, input: &Value) -> Vec<String> {
    let Some(raw) = raw_input.as_object() else {
        return Vec::new();
    };
    let mut fields: Vec<String> = raw
        .keys()
        .filter(|k| input.get(k.as_str()).is_none())
        .cloned()
        .collect();
    fields.sort();
    fields
}

pub async fn cmd_rerun(client: &Client, server: &str, args: &RerunArgs) -> Result<()> {
    // Check the arguments before any request.
    let mut body = build_trigger_body(args.input.as_deref())?;
    if !body["input"].is_object() {
        bail!("--input must be a JSON object");
    }
    let parsed = parse_file_args(&body["input"], &args.files)?;

    let resp = client
        .get(format!("{}/api/jobs/{}", server, args.job_id))
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let source: Value = resp.json().await.context("Failed to parse response")?;
    check_response(&status, &source)?;
    let workspace = source["workspace"]
        .as_str()
        .context("the job response has no workspace")?;
    let task = source["task_name"]
        .as_str()
        .context("the job response has no task_name")?;

    upload_files(
        client,
        server,
        workspace,
        task,
        Some(&args.job_id),
        &mut body["input"],
        &parsed,
    )
    .await?;
    body["replay_fields"] = json!(replay_fields(&source["raw_input"], &body["input"]));
    body["source_job_id"] = json!(args.job_id);

    let resp = client
        .post(trigger_url(server, workspace, task))
        .json(&body)
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let created: Value = resp.json().await.context("Failed to parse response")?;
    check_response(&status, &created)?;
    println!(
        "Job created: {}",
        created["job_id"].as_str().unwrap_or("unknown")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_bytes, body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const JOB: &str = "8b6cee4c-c807-4936-a708-82800dc45643";
    const FILE_ID: &str = "file_0123456789abcdef0123456789abcdef";

    fn args(input: Option<&str>, files: &[String]) -> RerunArgs {
        RerunArgs {
            job_id: JOB.into(),
            input: input.map(str::to_string),
            files: files.to_vec(),
        }
    }

    async fn source_job(server: &MockServer, raw_input: Value) {
        Mock::given(method("GET"))
            .and(path(format!("/api/jobs/{JOB}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "job_id": JOB, "workspace": "ws", "task_name": "t",
                "raw_input": raw_input,
            })))
            .mount(server)
            .await;
    }

    #[test]
    fn replay_fields_are_the_source_fields_not_given() {
        let raw = json!({"note": "n1", "token": "••••••", "data_file": "file_x"});
        assert_eq!(
            replay_fields(&raw, &json!({"note": "n2"})),
            vec!["data_file", "token"]
        );
        assert!(replay_fields(&raw, &raw).is_empty());
        assert!(replay_fields(&Value::Null, &json!({})).is_empty());
    }

    /// The masked secret is never sent back: it is replayed by name.
    #[tokio::test]
    async fn rerun_replays_the_fields_it_does_not_set() {
        let server = MockServer::start().await;
        source_job(&server, json!({"note": "n1", "token": "••••••"})).await;
        Mock::given(method("POST"))
            .and(path("/api/workspaces/ws/tasks/t/execute"))
            .and(body_json(json!({
                "input": {"note": "n2"},
                "source_job_id": JOB,
                "replay_fields": ["token"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"job_id": "new-1"})))
            .expect(1)
            .mount(&server)
            .await;
        cmd_rerun(
            &Client::new(),
            &server.uri(),
            &args(Some(r#"{"note":"n2"}"#), &[]),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rerun_uploads_a_file_for_the_source_then_executes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("r.csv");
        std::fs::write(&file, "a,b\n").unwrap();
        let server = MockServer::start().await;
        source_job(&server, json!({"note": "n1", "data_file": "file_old"})).await;
        Mock::given(method("POST"))
            .and(path("/api/workspaces/ws/tasks/t/files"))
            .and(query_param("name", "r.csv"))
            .and(query_param("source_job_id", JOB))
            .and(body_bytes("a,b\n"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"file_id": FILE_ID})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/workspaces/ws/tasks/t/execute"))
            .and(body_json(json!({
                "input": {"data_file": FILE_ID},
                "source_job_id": JOB,
                "replay_fields": ["note"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"job_id": "new-1"})))
            .expect(1)
            .mount(&server)
            .await;
        cmd_rerun(
            &Client::new(),
            &server.uri(),
            &args(None, &[format!("data_file={}", file.display())]),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rerun_checks_its_arguments_before_any_request() {
        let server = MockServer::start().await;
        for (input, files, text) in [
            (Some("[1]"), vec![], "--input must be a JSON object"),
            (Some("{nope"), vec![], "Invalid JSON input"),
            (
                Some(r#"{"d": 1}"#),
                vec!["d=/nonexistent".to_string()],
                "input field 'd' is set by both --input and --file",
            ),
        ] {
            let err = cmd_rerun(&Client::new(), &server.uri(), &args(input, &files))
                .await
                .unwrap_err();
            assert!(err.to_string().contains(text), "{input:?}: {err}");
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rerun_error_shows_the_server_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/api/jobs/{JOB}")))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"error": "Job not found"})),
            )
            .mount(&server)
            .await;
        let err = cmd_rerun(&Client::new(), &server.uri(), &args(None, &[]))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Server returned 404 Not Found: Job not found"
        );
    }
}
