use crate::job_creator::create_job_for_task;
use crate::state::AppState;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use stroem_common::models::workflow::TriggerDef;
use subtle::ConstantTimeEq;

const DEFAULT_SYNC_TIMEOUT_SECS: u64 = 30;

/// Build the webhook routes: GET and POST on /{name}.
pub fn build_hooks_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/{name}", get(webhook_handler).post(webhook_handler))
        .with_state(state)
}

/// Handle incoming webhook requests.
///
/// 1. Find a matching enabled Webhook trigger by name across all workspaces
/// 2. Validate secret if configured
/// 3. Build input from request body, headers, query params, and trigger defaults
/// 4. Create a job for the trigger's target task
#[tracing::instrument(skip(state, headers, body))]
async fn webhook_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> impl IntoResponse {
    // 1. Find the first matching enabled webhook trigger across all workspaces
    let wh = match find_webhook_trigger(&state, &name).await {
        Some(f) => f,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Webhook not found"})),
            )
                .into_response()
        }
    };

    // 2. Validate secret (constant-time to prevent timing attacks)
    if let Some(ref expected_secret) = wh.secret {
        let provided = extract_secret(&query, &headers);
        let is_valid = provided
            .as_deref()
            .map(|s| s.as_bytes().ct_eq(expected_secret.as_bytes()).into())
            .unwrap_or(false);
        if !is_valid {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid or missing secret"})),
            )
                .into_response();
        }
    }

    // 3. Build input
    let input = build_webhook_input(&method, &headers, &query, &body, &wh.default_input);

    let input_value = serde_json::to_value(&input).unwrap_or_default();
    let source_id = format!("{}/{}", wh.ws_name, wh.trigger_key);

    // 4. Get workspace config and create job
    let config = match state.get_workspace(&wh.ws_name).await {
        Some(c) => c,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Workspace not found"})),
            )
                .into_response()
        }
    };

    let is_sync = wh.mode.as_deref() == Some("sync");
    let timeout_secs = wh.timeout_secs.unwrap_or(DEFAULT_SYNC_TIMEOUT_SECS);

    match create_job_for_task(
        &state.pool,
        &config,
        &wh.ws_name,
        &wh.task,
        input_value,
        "webhook",
        Some(&source_id),
    )
    .await
    {
        Ok(job_id) => {
            tracing::info!(
                "Webhook '{}' created job {} for task '{}'",
                name,
                job_id,
                wh.task
            );

            if is_sync {
                let mut rx = state.job_completion.subscribe(job_id).await;

                // Guard against the (unlikely) race where the job completed
                // between create_job_for_task and subscribe. If already terminal,
                // return immediately without waiting.
                if let Ok(Some(job)) = stroem_db::JobRepo::get(&state.pool, job_id).await {
                    if job.status == "completed" || job.status == "failed" {
                        return Json(json!({
                            "job_id": job_id.to_string(),
                            "trigger": name,
                            "task": wh.task,
                            "status": job.status,
                            "output": job.output,
                        }))
                        .into_response();
                    }
                }

                let timeout = Duration::from_secs(timeout_secs);

                match tokio::time::timeout(timeout, rx.recv()).await {
                    Ok(Ok(event)) => Json(json!({
                        "job_id": job_id.to_string(),
                        "trigger": name,
                        "task": wh.task,
                        "status": event.status,
                        "output": event.output,
                    }))
                    .into_response(),
                    _ => {
                        // Timeout or channel error — return 202 for manual polling
                        (
                            StatusCode::ACCEPTED,
                            Json(json!({
                                "job_id": job_id.to_string(),
                                "trigger": name,
                                "task": wh.task,
                                "status": "running",
                            })),
                        )
                            .into_response()
                    }
                }
            } else {
                Json(json!({
                    "job_id": job_id.to_string(),
                    "trigger": name,
                    "task": wh.task,
                }))
                .into_response()
            }
        }
        Err(e) => {
            tracing::error!("Webhook '{}' failed to create job: {:#}", name, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to create job: {}", e)})),
            )
                .into_response()
        }
    }
}

/// Search result from find_webhook_trigger.
struct WebhookMatch {
    ws_name: String,
    trigger_key: String,
    task: String,
    secret: Option<String>,
    default_input: HashMap<String, serde_json::Value>,
    mode: Option<String>,
    timeout_secs: Option<u64>,
}

/// Find the first enabled webhook trigger matching the given name.
async fn find_webhook_trigger(state: &AppState, name: &str) -> Option<WebhookMatch> {
    for ws_name in state.workspaces.names() {
        let config = match state.workspaces.get_config_async(ws_name).await {
            Some(c) => c,
            None => continue,
        };
        for (trigger_key, trigger_def) in &config.triggers {
            if let TriggerDef::Webhook {
                name: wh_name,
                task,
                secret,
                input,
                enabled,
                mode,
                timeout_secs,
            } = trigger_def
            {
                if wh_name == name && *enabled {
                    return Some(WebhookMatch {
                        ws_name: ws_name.to_string(),
                        trigger_key: trigger_key.clone(),
                        task: task.clone(),
                        secret: secret.clone(),
                        default_input: input.clone(),
                        mode: mode.clone(),
                        timeout_secs: *timeout_secs,
                    });
                }
            }
        }
    }
    None
}

/// Extract secret from query param `?secret=xxx` or `Authorization: Bearer xxx` header.
fn extract_secret(query: &HashMap<String, String>, headers: &HeaderMap) -> Option<String> {
    // Check query param first
    if let Some(s) = query.get("secret") {
        return Some(s.clone());
    }

    // Check Authorization: Bearer header
    if let Some(auth) = headers.get("authorization") {
        if let Ok(val) = auth.to_str() {
            if let Some(token) = val.strip_prefix("Bearer ") {
                return Some(token.to_string());
            }
        }
    }

    None
}

/// Build the webhook input map from the request.
///
/// Structure:
/// - `body`: JSON-parsed body (if Content-Type: application/json), raw string, or null for GET
/// - `headers`: lowercase key map of request headers
/// - `method`: "GET" or "POST"
/// - `query`: query params (excluding `secret`)
/// - Trigger YAML `input` defaults merge at top level (don't overwrite reserved keys)
fn build_webhook_input(
    method: &Method,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    body: &Bytes,
    default_input: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    let mut input = HashMap::new();

    // Body
    let body_value = if method == Method::GET {
        serde_json::Value::Null
    } else {
        let is_json = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("application/json"))
            .unwrap_or(false);

        if is_json {
            serde_json::from_slice(body).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(body).to_string())
            })
        } else if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(String::from_utf8_lossy(body).to_string())
        }
    };
    input.insert("body".to_string(), body_value);

    // Headers (lowercase keys)
    let headers_map: HashMap<String, serde_json::Value> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str().ok().map(|val| {
                (
                    k.as_str().to_string(),
                    serde_json::Value::String(val.to_string()),
                )
            })
        })
        .collect();
    input.insert(
        "headers".to_string(),
        serde_json::to_value(headers_map).unwrap_or_default(),
    );

    // Method
    input.insert(
        "method".to_string(),
        serde_json::Value::String(method.to_string()),
    );

    // Query params (always exclude `secret` — it's a transport concern, not application data)
    let filtered_query: HashMap<String, serde_json::Value> = query
        .iter()
        .filter(|(k, _)| k.as_str() != "secret")
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    input.insert(
        "query".to_string(),
        serde_json::to_value(filtered_query).unwrap_or_default(),
    );

    // Merge trigger YAML input defaults (don't overwrite reserved keys)
    let reserved = ["body", "headers", "method", "query"];
    for (k, v) in default_input {
        if !reserved.contains(&k.as_str()) {
            input.insert(k.clone(), v.clone());
        }
    }

    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_secret_from_query() {
        let mut query = HashMap::new();
        query.insert("secret".to_string(), "my-secret".to_string());
        let headers = HeaderMap::new();
        assert_eq!(
            extract_secret(&query, &headers),
            Some("my-secret".to_string())
        );
    }

    #[test]
    fn test_extract_secret_from_bearer() {
        let query = HashMap::new();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer tok123".parse().unwrap());
        assert_eq!(extract_secret(&query, &headers), Some("tok123".to_string()));
    }

    #[test]
    fn test_extract_secret_none() {
        let query = HashMap::new();
        let headers = HeaderMap::new();
        assert_eq!(extract_secret(&query, &headers), None);
    }

    #[test]
    fn test_extract_secret_query_takes_precedence() {
        let mut query = HashMap::new();
        query.insert("secret".to_string(), "from-query".to_string());
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer from-header".parse().unwrap());
        assert_eq!(
            extract_secret(&query, &headers),
            Some("from-query".to_string())
        );
    }

    #[test]
    fn test_build_webhook_input_json_body() {
        let method = Method::POST;
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let query = HashMap::new();
        let body = Bytes::from(r#"{"ref":"refs/heads/main"}"#);
        let defaults = HashMap::new();

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        assert_eq!(input["body"]["ref"], "refs/heads/main");
        assert_eq!(input["method"], "POST");
    }

    #[test]
    fn test_build_webhook_input_plaintext_body() {
        let method = Method::POST;
        let headers = HeaderMap::new();
        let query = HashMap::new();
        let body = Bytes::from("hello world");
        let defaults = HashMap::new();

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        assert_eq!(input["body"], "hello world");
    }

    #[test]
    fn test_build_webhook_input_get_null_body() {
        let method = Method::GET;
        let headers = HeaderMap::new();
        let mut query = HashMap::new();
        query.insert("env".to_string(), "prod".to_string());
        let body = Bytes::new();
        let defaults = HashMap::new();

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        assert!(input["body"].is_null());
        assert_eq!(input["method"], "GET");
        assert_eq!(input["query"]["env"], "prod");
    }

    #[test]
    fn test_build_webhook_input_secret_excluded_from_query() {
        let method = Method::POST;
        let headers = HeaderMap::new();
        let mut query = HashMap::new();
        query.insert("secret".to_string(), "my-secret".to_string());
        query.insert("env".to_string(), "staging".to_string());
        let body = Bytes::new();
        let defaults = HashMap::new();

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        let query_map = input["query"].as_object().unwrap();
        assert!(!query_map.contains_key("secret"));
        assert_eq!(query_map["env"], "staging");
    }

    #[test]
    fn test_build_webhook_input_secret_excluded_even_for_public_webhooks() {
        // Even if trigger has no secret, the `secret` query param should not leak into input
        let method = Method::GET;
        let headers = HeaderMap::new();
        let mut query = HashMap::new();
        query.insert("secret".to_string(), "some-value".to_string());
        query.insert("env".to_string(), "prod".to_string());
        let body = Bytes::new();
        let defaults = HashMap::new();

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        let query_map = input["query"].as_object().unwrap();
        assert!(!query_map.contains_key("secret"));
        assert_eq!(query_map["env"], "prod");
    }

    #[test]
    fn test_build_webhook_input_defaults_merge() {
        let method = Method::POST;
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let query = HashMap::new();
        let body = Bytes::from("{}");
        let mut defaults = HashMap::new();
        defaults.insert(
            "environment".to_string(),
            serde_json::Value::String("staging".to_string()),
        );
        // Reserved key should NOT overwrite
        defaults.insert(
            "body".to_string(),
            serde_json::Value::String("should-not-appear".to_string()),
        );

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        assert_eq!(input["environment"], "staging");
        // body should be the actual request body, not the default
        assert_ne!(input["body"], "should-not-appear");
    }

    #[test]
    fn test_build_webhook_input_malformed_json_falls_back_to_string() {
        let method = Method::POST;
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let query = HashMap::new();
        let body = Bytes::from("not valid json {{{");
        let defaults = HashMap::new();

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        // Should fall back to raw string instead of error
        assert_eq!(input["body"], "not valid json {{{");
    }

    #[test]
    fn test_build_webhook_input_empty_post_body() {
        let method = Method::POST;
        let headers = HeaderMap::new();
        let query = HashMap::new();
        let body = Bytes::new();
        let defaults = HashMap::new();

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        // Empty POST body without content-type should be null
        assert!(input["body"].is_null());
    }

    #[test]
    fn test_build_webhook_input_empty_json_post_body() {
        let method = Method::POST;
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let query = HashMap::new();
        let body = Bytes::new();
        let defaults = HashMap::new();

        let input = build_webhook_input(&method, &headers, &query, &body, &defaults);

        // Empty body with application/json falls back to string (serde_json::from_slice fails on empty)
        // This is acceptable — the caller sent an empty JSON body
        assert!(input.contains_key("body"));
    }

    #[test]
    fn test_extract_secret_non_bearer_auth_ignored() {
        let query = HashMap::new();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        // Basic auth should not be extracted as a secret
        assert_eq!(extract_secret(&query, &headers), None);
    }
}
