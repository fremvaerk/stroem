use anyhow::{Context, Result};
use reqwest::Client;

/// Check if the HTTP response indicates success; bail with the error message otherwise.
pub fn check_response(
    status: &reqwest::StatusCode,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    if !status.is_success() {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        anyhow::bail!("Server returned {}: {}", status, err);
    }
    Ok(())
}

/// Build HTTP client, optionally with a default Authorization header.
pub fn build_client(token: Option<&str>) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = token {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))
                .context("Invalid token value")?,
        );
    }
    Client::builder()
        .default_headers(headers)
        .build()
        .context("Failed to build HTTP client")
}

/// Builds the trigger request body from an optional JSON input string.
pub fn build_trigger_body(input: Option<&str>) -> Result<serde_json::Value> {
    match input {
        Some(json_str) => {
            let input_val: serde_json::Value =
                serde_json::from_str(json_str).context("Invalid JSON input")?;
            Ok(serde_json::json!({ "input": input_val }))
        }
        None => Ok(serde_json::json!({ "input": {} })),
    }
}

/// Constructs the URL for triggering a task execution.
pub fn trigger_url(server: &str, workspace: &str, task: &str) -> String {
    format!(
        "{}/api/workspaces/{}/tasks/{}/execute",
        server, workspace, task
    )
}

/// Constructs the URL for fetching jobs with an optional limit.
pub fn jobs_url(server: &str, limit: i64) -> String {
    format!("{}/api/jobs?limit={}", server, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_response_ok_on_success_status() {
        let status = reqwest::StatusCode::OK;
        let body = serde_json::json!({});
        assert!(check_response(&status, &body).is_ok());
    }

    #[test]
    fn check_response_err_on_4xx_with_error_field() {
        let status = reqwest::StatusCode::NOT_FOUND;
        let body = serde_json::json!({ "error": "task not found" });
        let err = check_response(&status, &body).unwrap_err();
        assert!(err.to_string().contains("404"));
        assert!(err.to_string().contains("task not found"));
    }

    #[test]
    fn check_response_err_on_5xx_with_fallback_message() {
        let status = reqwest::StatusCode::INTERNAL_SERVER_ERROR;
        let body = serde_json::json!({});
        let err = check_response(&status, &body).unwrap_err();
        assert!(err.to_string().contains("500"));
        assert!(err.to_string().contains("Unknown error"));
    }

    #[test]
    fn check_response_ok_on_created_status() {
        let status = reqwest::StatusCode::CREATED;
        let body = serde_json::json!({ "job_id": "abc" });
        assert!(check_response(&status, &body).is_ok());
    }

    #[test]
    fn build_trigger_body_none_produces_empty_input_object() {
        let body = build_trigger_body(None).unwrap();
        assert_eq!(body, serde_json::json!({ "input": {} }));
    }

    #[test]
    fn build_trigger_body_with_json_wraps_input() {
        let body = build_trigger_body(Some(r#"{"key":"value"}"#)).unwrap();
        assert_eq!(body, serde_json::json!({ "input": { "key": "value" } }));
    }

    #[test]
    fn build_trigger_body_with_array_json_wraps_input() {
        let body = build_trigger_body(Some(r#"[1,2,3]"#)).unwrap();
        assert_eq!(body, serde_json::json!({ "input": [1, 2, 3] }));
    }

    #[test]
    fn build_trigger_body_with_invalid_json_returns_error() {
        let result = build_trigger_body(Some("{not-json}"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid JSON input"));
    }

    #[test]
    fn trigger_url_constructs_correct_path() {
        let url = trigger_url("http://localhost:8080", "default", "my-task");
        assert_eq!(
            url,
            "http://localhost:8080/api/workspaces/default/tasks/my-task/execute"
        );
    }

    #[test]
    fn trigger_url_includes_custom_workspace() {
        let url = trigger_url("http://prod:9000", "production", "deploy");
        assert_eq!(
            url,
            "http://prod:9000/api/workspaces/production/tasks/deploy/execute"
        );
    }

    #[test]
    fn jobs_url_includes_limit_parameter() {
        let url = jobs_url("http://localhost:8080", 20);
        assert_eq!(url, "http://localhost:8080/api/jobs?limit=20");
    }

    #[test]
    fn jobs_url_uses_provided_limit() {
        let url = jobs_url("http://localhost:8080", 100);
        assert_eq!(url, "http://localhost:8080/api/jobs?limit=100");
    }

    #[test]
    fn build_client_without_token_succeeds() {
        let result = build_client(None);
        assert!(result.is_ok());
    }

    #[test]
    fn build_client_with_token_succeeds() {
        let result = build_client(Some("strm_abc123"));
        assert!(result.is_ok());
    }

    #[test]
    fn build_client_with_non_ascii_token_returns_error() {
        // HeaderValue::from_str rejects tokens containing control characters
        // (e.g. newline \n). Non-ASCII unicode bytes are accepted as opaque
        // bytes by http v1.x, but control characters are always rejected.
        let result = build_client(Some("token_with_\nnewline"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid token"));
    }
}
