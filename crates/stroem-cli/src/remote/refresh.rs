//! `stroem-api refresh <WS>`: reload a workspace from its source now
//! (`POST /api/workspaces/{ws}/refresh`).

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;

use super::client::check_response;
use super::paths::api_url;

pub async fn cmd_refresh(client: &Client, server: &str, workspace: &str) -> Result<()> {
    println!("{}", refresh_text(client, server, workspace).await?);
    Ok(())
}

pub async fn refresh_text(client: &Client, server: &str, workspace: &str) -> Result<String> {
    let resp = client
        .post(api_url(server, &["workspaces", workspace, "refresh"])?)
        .send()
        .await
        .context("Failed to connect to server")?;
    let status = resp.status();
    let body: Value = resp.json().await.context("Failed to parse response")?;
    check_response(&status, &body)?;
    let revision = body["revision"].as_str().unwrap_or("-");
    Ok(format!(
        "Refreshed workspace '{}' (revision {revision})",
        body["workspace"].as_str().unwrap_or(workspace)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn prints_the_new_revision() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/workspaces/data%20team/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"workspace": "data team", "revision": "abc123", "refreshed": true}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let text = refresh_text(&Client::new(), &server.uri(), "data team").await?;
        assert_eq!(text, "Refreshed workspace 'data team' (revision abc123)");
        Ok(())
    }

    #[tokio::test]
    async fn cooldown_is_reported_with_the_wait() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/workspaces/default/refresh"))
            .respond_with(
                ResponseTemplate::new(429).set_body_json(
                    json!({"error": "Workspace refresh rate-limited; retry after 4s"}),
                ),
            )
            .mount(&server)
            .await;
        let err = refresh_text(&Client::new(), &server.uri(), "default")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("429"), "{err}");
        assert!(err.to_string().contains("retry after 4s"), "{err}");
    }
}
