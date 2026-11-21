//! CLI subcommands for listing and downloading job artifacts.

use super::ArtifactsAction;
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;
use super::paths::api_url;

pub async fn dispatch(client: &Client, server: &str, action: ArtifactsAction) -> Result<()> {
    match action {
        ArtifactsAction::List { job_id } => cmd_list(client, server, &job_id).await,
        ArtifactsAction::Download {
            job_id,
            name,
            output,
        } => cmd_download(client, server, &job_id, &name, output.as_deref()).await,
    }
}

pub async fn cmd_list(client: &Client, server: &str, job_id: &str) -> Result<()> {
    let resp = client
        .get(api_url(server, &["jobs", job_id, "artifacts"])?)
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;

    if !status.is_success() {
        check_response(&status, &body)?;
    }

    let items = body.as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("(no artifacts)");
        return Ok(());
    }
    println!(
        "{:<40} {:>12} {:<24} {:<20}",
        "NAME", "SIZE", "TYPE", "STEP"
    );
    for it in items {
        println!(
            "{:<40} {:>12} {:<24} {:<20}",
            it["name"].as_str().unwrap_or(""),
            it["size_bytes"].as_i64().unwrap_or(0),
            it["content_type"].as_str().unwrap_or(""),
            it["step_name"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

pub async fn cmd_download(
    client: &Client,
    server: &str,
    job_id: &str,
    name: &str,
    output: Option<&str>,
) -> Result<()> {
    let resp = client
        .get(api_url(server, &["jobs", job_id, "artifacts", name])?)
        .send()
        .await
        .context("Failed to connect to server")?;

    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        check_response(&status, &body)?;
        // check_response always bails on non-success status, but be explicit
        anyhow::bail!("Server returned {}", status);
    }

    let bytes = resp.bytes().await.context("read response body")?;
    let out_path = output.unwrap_or(name);
    if let Some(parent) = std::path::Path::new(out_path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    tokio::fs::write(out_path, &bytes).await?;
    eprintln!("Wrote {} bytes to {}", bytes.len(), out_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The artifact name is one percent-encoded path segment: a space is
    /// `%20` (a `+` would reach the server as a literal `+`), `/`, `?` and
    /// `#` cannot change the URL.
    #[tokio::test]
    async fn download_percent_encodes_the_artifact_name() -> Result<()> {
        let server = MockServer::start().await;
        let job_id = "00000000-0000-0000-0000-000000000124";
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/jobs/{job_id}/artifacts/my%20report%2Fv2%3F%23.png"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok".as_slice()))
            .expect(1)
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir()?;
        let out = tmp.path().join("out.png");
        cmd_download(
            &reqwest::Client::new(),
            server.uri().as_str(),
            job_id,
            "my report/v2?#.png",
            Some(out.to_str().unwrap()),
        )
        .await?;
        assert_eq!(tokio::fs::read(&out).await?, b"ok");
        Ok(())
    }

    /// `cmd_download` must create any non-existent parent directories in the
    /// `-o`/`--output` path before writing — `tokio::fs::write` does not
    /// auto-create parents, so without the `create_dir_all` step the call
    /// would fail with `No such file or directory` on a fresh nested path.
    #[tokio::test]
    async fn download_creates_nested_output_parents() -> Result<()> {
        let server = MockServer::start().await;
        let job_id = "00000000-0000-0000-0000-000000000123";
        let name = "report.txt";
        let body = b"hello from the wire";

        Mock::given(method("GET"))
            .and(path(format!("/api/jobs/{job_id}/artifacts/{name}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_bytes(body.as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir()?;
        let nested = tmp
            .path()
            .join("does")
            .join("not")
            .join("exist")
            .join("file.txt");
        assert!(
            !nested.parent().unwrap().exists(),
            "test setup: nested parent dirs must not exist yet"
        );

        let client = reqwest::Client::new();
        cmd_download(
            &client,
            server.uri().as_str(),
            job_id,
            name,
            Some(nested.to_str().unwrap()),
        )
        .await?;

        assert!(nested.exists(), "expected file at {}", nested.display());
        assert!(
            nested.parent().unwrap().exists(),
            "parent dir should have been created"
        );
        let written = tokio::fs::read(&nested).await?;
        assert_eq!(&written[..], body);
        Ok(())
    }

    /// Without `-o`, the artifact name is written to the current working
    /// directory. The CLI must not stamp on whatever lives there; we test
    /// the simpler "no nested parent" path to lock in that `parent() == ""`
    /// is handled (skipping `create_dir_all` when the relative path has no
    /// parent component).
    #[tokio::test]
    async fn download_with_bare_filename_skips_dir_creation() -> Result<()> {
        let server = MockServer::start().await;
        let job_id = "11111111-2222-3333-4444-555555555555";
        let name = "single.txt";
        let body = b"top-level";

        Mock::given(method("GET"))
            .and(path(format!("/api/jobs/{job_id}/artifacts/{name}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_bytes(body.as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir()?;
        let bare = tmp.path().join("flat.txt");

        let client = reqwest::Client::new();
        cmd_download(
            &client,
            server.uri().as_str(),
            job_id,
            name,
            Some(bare.to_str().unwrap()),
        )
        .await?;

        let written = tokio::fs::read(&bare).await?;
        assert_eq!(&written[..], body);
        Ok(())
    }
}
