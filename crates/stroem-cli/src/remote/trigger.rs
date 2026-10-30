use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::path::PathBuf;
use tokio_util::io::ReaderStream;

use super::client::{build_trigger_body, check_response, trigger_url};

/// Parses one `--file FIELD=PATH` argument: split on the first `=`, both
/// sides non-empty.
pub fn parse_file_arg(s: &str) -> Result<(String, PathBuf)> {
    match s.split_once('=') {
        Some((field, path)) if !field.is_empty() && !path.is_empty() => {
            Ok((field.to_string(), PathBuf::from(path)))
        }
        _ => bail!("invalid --file argument '{s}': expected FIELD=PATH"),
    }
}

/// The upload route of a task; `name` is the original file name.
pub fn upload_url(server: &str, ws: &str, task: &str, name: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("name", name)
        .finish();
    format!("{server}/api/workspaces/{ws}/tasks/{task}/files?{query}")
}

/// Streams one local file to the upload route and returns its `file_id`.
async fn upload_file(
    client: &Client,
    server: &str,
    workspace: &str,
    task: &str,
    field: &str,
    path: &std::path::Path,
) -> Result<String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .with_context(|| format!("--file {field}: '{}' has no file name", path.display()))?;
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("--file {field}: cannot open '{}'", path.display()))?;
    let resp = client
        .post(upload_url(server, workspace, task, &name))
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .with_context(|| format!("--file {field}: failed to parse upload response"))?;
    check_response(&status, &body).with_context(|| format!("--file {field}: upload failed"))?;
    body.get("file_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("--file {field}: upload response has no file_id"))
}

pub async fn cmd_trigger(
    client: &Client,
    server: &str,
    workspace: &str,
    task: &str,
    input: Option<&str>,
    files: &[String],
) -> Result<()> {
    let mut body: Value = build_trigger_body(input)?;

    // Parse every argument and check for key clashes before any upload.
    let parsed = files
        .iter()
        .map(|f| parse_file_arg(f))
        .collect::<Result<Vec<_>>>()?;
    let mut seen = std::collections::HashSet::new();
    for (field, _) in &parsed {
        if body["input"].get(field).is_some() {
            bail!("input field '{field}' is set by both --input and --file");
        }
        if !seen.insert(field.clone()) {
            bail!("input field '{field}' is set by more than one --file");
        }
    }
    for (field, path) in &parsed {
        let id = upload_file(client, server, workspace, task, field, path).await?;
        body["input"]
            .as_object_mut()
            .context("--input must be a JSON object when --file is used")?
            .insert(field.clone(), Value::String(id));
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_bytes, body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ID: &str = "file_0123456789abcdef0123456789abcdef";

    #[test]
    fn parse_file_arg_cases() {
        let (f, p) = parse_file_arg("data=./a=b.csv").unwrap();
        assert_eq!(f, "data");
        assert_eq!(p, PathBuf::from("./a=b.csv"));
        for bad in ["", "data", "=x", "data=", "="] {
            let err = parse_file_arg(bad).unwrap_err().to_string();
            assert!(err.contains(&format!("'{bad}'")), "{err}");
        }
    }

    #[test]
    fn upload_url_encodes_name() {
        let u = upload_url("http://s", "ws", "t", "my file&x=1.csv");
        assert_eq!(
            u,
            "http://s/api/workspaces/ws/tasks/t/files?name=my+file%26x%3D1.csv"
        );
    }

    #[tokio::test]
    async fn trigger_uploads_then_executes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("r.csv");
        std::fs::write(&file, "a,b\n1,2\n").unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/workspaces/default/tasks/t/files"))
            .and(query_param("name", "r.csv"))
            .and(header("content-type", "application/octet-stream"))
            .and(body_bytes("a,b\n1,2\n"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({ "file_id": ID })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/workspaces/default/tasks/t/execute"))
            .and(body_json(
                serde_json::json!({ "input": { "data_file": ID, "n": 1 } }),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "job_id": "j1" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        cmd_trigger(
            &client,
            &server.uri(),
            "default",
            "t",
            Some(r#"{"n":1}"#),
            &[format!("data_file={}", file.display())],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn trigger_file_and_input_same_key_is_error() {
        let server = MockServer::start().await;
        let client = reqwest::Client::new();
        let err = cmd_trigger(
            &client,
            &server.uri(),
            "default",
            "t",
            Some(r#"{"data_file":"x"}"#),
            &["data_file=/nonexistent".to_string()],
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("data_file"), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn trigger_upload_error_names_field_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("r.csv");
        std::fs::write(&file, "x").unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/workspaces/default/tasks/t/files"))
            .respond_with(
                ResponseTemplate::new(413).set_body_json(serde_json::json!({ "error": "too big" })),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let err = cmd_trigger(
            &client,
            &server.uri(),
            "default",
            "t",
            None,
            &[format!("d={}", file.display())],
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--file d") && msg.contains("too big"), "{msg}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
