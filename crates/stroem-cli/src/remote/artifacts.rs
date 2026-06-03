//! CLI subcommands for listing and downloading job artifacts.

use super::ArtifactsAction;
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;

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
        .get(format!("{}/api/jobs/{}/artifacts", server, job_id))
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
    let encoded = url_encode(name);
    let resp = client
        .get(format!(
            "{}/api/jobs/{}/artifacts/{}",
            server, job_id, encoded
        ))
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

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_passes_through_safe_chars() {
        assert_eq!(url_encode("report.png"), "report.png");
    }

    #[test]
    fn url_encode_escapes_spaces_and_slashes() {
        let encoded = url_encode("my report/v2.png");
        // form_urlencoded encodes spaces as '+' and '/' as %2F
        assert!(encoded.contains("%2F"));
        assert!(encoded.contains('+') || encoded.contains("%20"));
    }
}
