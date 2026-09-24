use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::io::Write;

use stroem_common::format::human_bytes;

use super::client::check_response;

/// The default tail, a tail of `tail_bytes`, or the full stream.
pub fn logs_url(server: &str, job_id: &str, full: bool, tail_bytes: Option<u64>) -> String {
    let base = format!("{server}/api/jobs/{job_id}/logs");
    match (full, tail_bytes) {
        (true, _) => format!("{base}?full=true"),
        (false, Some(n)) => format!("{base}?tail_bytes={n}"),
        (false, None) => base,
    }
}

pub fn truncation_note(returned: u64, total: u64) -> String {
    format!(
        "note: showing the last {} of up to {}; use --full for the whole log",
        human_bytes(returned),
        human_bytes(total)
    )
}

/// Fetch `url` and print the log to `out`. An NDJSON body (`full=true` on a
/// current server) is copied as it arrives; a JSON envelope (a tail, or any
/// answer from an older server) prints `logs` and, when truncated, a note
/// to `err`.
pub async fn write_logs<O: Write, E: Write>(
    client: &Client,
    url: &str,
    out: &mut O,
    err: &mut E,
) -> Result<()> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let ndjson = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-ndjson"));
    if status.is_success() && ndjson {
        while let Some(chunk) = resp
            .chunk()
            .await
            .context("Failed to read the log stream")?
        {
            out.write_all(&chunk)?;
        }
        out.flush()?;
        return Ok(());
    }
    let body: Value = resp.json().await.context("Failed to parse response")?;
    check_response(&status, &body)?;
    let logs = body.get("logs").and_then(Value::as_str).unwrap_or("");
    out.write_all(logs.as_bytes())?;
    out.flush()?;
    if body.get("truncated").and_then(Value::as_bool) == Some(true) {
        let returned = body
            .get("returned_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(logs.len() as u64);
        let total = body
            .get("total_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(returned);
        writeln!(err, "{}", truncation_note(returned, total))?;
    }
    Ok(())
}

pub async fn cmd_logs(
    client: &Client,
    server: &str,
    job_id: &str,
    full: bool,
    tail_bytes: Option<u64>,
) -> Result<()> {
    let url = logs_url(server, job_id, full, tail_bytes);
    write_logs(client, &url, &mut std::io::stdout(), &mut std::io::stderr()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn logs_url_variants() {
        assert_eq!(
            logs_url("http://s", "j1", false, None),
            "http://s/api/jobs/j1/logs"
        );
        assert_eq!(
            logs_url("http://s", "j1", false, Some(1024)),
            "http://s/api/jobs/j1/logs?tail_bytes=1024"
        );
        assert_eq!(
            logs_url("http://s", "j1", true, None),
            "http://s/api/jobs/j1/logs?full=true"
        );
    }

    #[test]
    fn truncation_note_names_both_sizes() {
        assert_eq!(
            truncation_note(262_144, 87_325_871),
            "note: showing the last 256.0 KiB of up to 83.3 MiB; use --full for the whole log"
        );
    }

    async fn run(
        server: &MockServer,
        full: bool,
        tail: Option<u64>,
    ) -> (anyhow::Result<()>, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let url = logs_url(&server.uri(), "j1", full, tail);
        let result = write_logs(&Client::new(), &url, &mut out, &mut err).await;
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[tokio::test]
    async fn a_truncated_tail_prints_the_logs_and_a_note_on_stderr() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "logs": "a\nb\n", "truncated": true, "total_bytes": 87_325_871u64, "returned_bytes": 262_144u64
            })))
            .mount(&server)
            .await;
        let (result, out, err) = run(&server, false, None).await;
        result.unwrap();
        assert_eq!(out, "a\nb\n");
        assert!(err.contains("256.0 KiB of up to 83.3 MiB"), "{err}");
    }

    #[tokio::test]
    async fn a_complete_tail_prints_no_note() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "logs": "a\n", "truncated": false, "total_bytes": 2, "returned_bytes": 2
            })))
            .mount(&server)
            .await;
        let (result, out, err) = run(&server, false, None).await;
        result.unwrap();
        assert_eq!((out.as_str(), err.as_str()), ("a\n", ""));
    }

    #[tokio::test]
    async fn full_streams_the_ndjson_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .and(query_param("full", "true"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"x\ny\n".to_vec(), "application/x-ndjson"),
            )
            .mount(&server)
            .await;
        let (result, out, _) = run(&server, true, None).await;
        result.unwrap();
        assert_eq!(out, "x\ny\n");
    }

    #[tokio::test]
    async fn full_against_an_old_server_prints_the_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"logs": "old\n"})),
            )
            .mount(&server)
            .await;
        let (result, out, err) = run(&server, true, None).await;
        result.unwrap();
        assert_eq!((out.as_str(), err.as_str()), ("old\n", ""));
    }

    #[tokio::test]
    async fn tail_bytes_is_forwarded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .and(query_param("tail_bytes", "1024"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"logs": "t\n"})),
            )
            .mount(&server)
            .await;
        let (result, out, _) = run(&server, false, Some(1024)).await;
        result.unwrap();
        assert_eq!(out, "t\n");
    }

    #[tokio::test]
    async fn server_errors_surface_their_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/j1/logs"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"error": "Job not found"})),
            )
            .mount(&server)
            .await;
        let (result, _, _) = run(&server, false, None).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("Job not found"));
    }
}
