use crate::job_creator::create_job_for_task;
use crate::state::AppState;
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use stroem_common::models::job::JobStatus;
use stroem_common::models::workflow::TriggerDef;
use subtle::ConstantTimeEq;
use uuid::Uuid;

const DEFAULT_SYNC_TIMEOUT_SECS: u64 = 30;
const MAX_WAIT_TIMEOUT_SECS: u64 = 300;

/// Build the webhook routes: GET and POST on /{name}.
pub fn build_hooks_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/{name}", get(webhook_handler).post(webhook_handler))
        .route("/{name}/jobs/{job_id}", get(webhook_job_status))
        .with_state(state)
}

/// Handle incoming webhook requests.
///
/// 1. Find a matching enabled Webhook trigger by name across all workspaces
/// 2. Validate secret if configured
/// 3. Build input from request body, headers, query params, and trigger defaults
/// 4. Create a job for the trigger's target task
#[tracing::instrument(skip(state, query, headers, body))]
async fn webhook_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> axum::response::Response {
    // 1. Find the first matching enabled webhook trigger across all workspaces
    let wh = match find_webhook_trigger(&state, &name).await {
        Some(f) => f,
        None => return AppError::not_found("Webhook").into_response(),
    };

    // 2. Validate secret (constant-time to prevent timing attacks)
    if let Some(err) =
        validate_webhook_secret(&wh, query.get("secret").map(String::as_str), &headers)
    {
        return err.into_response();
    }

    // 3. Build input
    let input = build_webhook_input(&method, &headers, &query, &body, &wh.default_input);

    let input_value = serde_json::to_value(&input).unwrap_or_default();
    let source_id = format!("{}/{}", wh.ws_name, wh.trigger_key);

    // 4. Get workspace config and create job
    let config = match state.get_workspace(&wh.ws_name).await {
        Some(c) => c,
        None => {
            return AppError::Internal(anyhow::anyhow!(
                "Workspace '{}' not found after webhook trigger lookup",
                wh.ws_name
            ))
            .into_response();
        }
    };

    let is_sync = wh.mode.as_deref() == Some("sync");
    let timeout_secs = wh.timeout_secs.unwrap_or(DEFAULT_SYNC_TIMEOUT_SECS);

    let job_id = match create_job_for_task(
        &state.pool,
        &config,
        &wh.ws_name,
        &wh.task,
        input_value,
        "webhook",
        Some(&source_id),
    )
    .await
    .context("create webhook job")
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Webhook '{}' failed to create job: {:#}", name, e);
            return AppError::Internal(e).into_response();
        }
    };

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
            if is_terminal_status(&job.status) {
                return Json(WebhookSyncResponse {
                    job_id: job_id.to_string(),
                    trigger: name,
                    task: wh.task,
                    status: job.status,
                    output: job.output,
                })
                .into_response();
            }
        }

        let timeout = Duration::from_secs(timeout_secs);

        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Ok(event)) => Json(WebhookSyncResponse {
                job_id: job_id.to_string(),
                trigger: name,
                task: wh.task,
                status: event.status,
                output: event.output,
            })
            .into_response(),
            _ => {
                // Timeout or channel error — return 202 for manual polling
                (
                    StatusCode::ACCEPTED,
                    Json(WebhookSyncResponse {
                        job_id: job_id.to_string(),
                        trigger: name,
                        task: wh.task,
                        status: "running".to_string(),
                        output: None,
                    }),
                )
                    .into_response()
            }
        }
    } else {
        Json(WebhookAsyncResponse {
            job_id: job_id.to_string(),
            trigger: name,
            task: wh.task,
        })
        .into_response()
    }
}

/// Validate the webhook secret. Returns `Some(AppError)` if validation fails,
/// or `None` if the request is authorized.
///
/// `provided_secret` is the caller-supplied secret (e.g. from a query param).
/// If absent, the function falls back to the `Authorization: Bearer` header.
fn validate_webhook_secret(
    wh: &WebhookMatch,
    provided_secret: Option<&str>,
    headers: &HeaderMap,
) -> Option<AppError> {
    if let Some(ref expected_secret) = wh.secret {
        // Use the caller-supplied secret, or fall back to Authorization: Bearer header.
        let effective_secret: Option<String> =
            provided_secret.map(|s| s.to_string()).or_else(|| {
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|val| val.strip_prefix("Bearer "))
                    .map(|t| t.to_string())
            });
        let is_valid = effective_secret
            .as_deref()
            .map(|s| {
                let provided_hash = Sha256::digest(s.as_bytes());
                let expected_hash = Sha256::digest(expected_secret.as_bytes());
                provided_hash.ct_eq(&expected_hash).into()
            })
            .unwrap_or(false);
        if !is_valid {
            return Some(AppError::Unauthorized("Invalid or missing secret".into()));
        }
    }
    None
}

/// Returns `true` if the job status represents a terminal state.
fn is_terminal_status(status: &str) -> bool {
    status == JobStatus::Completed.as_ref()
        || status == JobStatus::Failed.as_ref()
        || status == JobStatus::Cancelled.as_ref()
}

/// Query params for the webhook job status endpoint.
#[derive(Debug, Deserialize)]
struct StatusQuery {
    #[serde(default)]
    wait: bool,
    #[serde(default)]
    timeout: Option<u64>,
    secret: Option<String>,
}

/// Response for async (fire-and-forget) webhook invocation.
#[derive(Debug, Serialize)]
struct WebhookAsyncResponse {
    job_id: String,
    trigger: String,
    task: String,
}

/// Response for sync webhook invocation (job completed or timed out mid-wait).
#[derive(Debug, Serialize)]
struct WebhookSyncResponse {
    job_id: String,
    trigger: String,
    task: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<serde_json::Value>,
}

/// Response for the webhook job status endpoint.
#[derive(Debug, Serialize)]
struct WebhookJobStatusResponse {
    job_id: String,
    trigger: String,
    task: String,
    status: String,
    output: Option<serde_json::Value>,
    created_at: String,
    completed_at: Option<String>,
}

/// Add `Cache-Control: no-store` to a response to prevent intermediary caching
/// of mutable job status data.
fn with_no_cache(response: axum::response::Response) -> axum::response::Response {
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header value is valid"),
    );
    axum::response::Response::from_parts(parts, body)
}

/// Check the status of a job created by a webhook trigger.
///
/// Uses the same authentication as the webhook itself (secret or open).
/// Only returns jobs that were created by this specific webhook trigger.
/// Supports `?wait=true&timeout=30` to wait for job completion.
#[tracing::instrument(skip(state, query, headers))]
async fn webhook_job_status(
    State(state): State<Arc<AppState>>,
    Path((name, job_id_str)): Path<(String, String)>,
    Query(query): Query<StatusQuery>,
    headers: HeaderMap,
) -> axum::response::Response {
    // Parse job_id
    let job_id = match Uuid::parse_str(&job_id_str) {
        Ok(id) => id,
        Err(_) => return AppError::BadRequest("Invalid job ID".into()).into_response(),
    };

    // Find webhook trigger
    let wh = match find_webhook_trigger(&state, &name).await {
        Some(f) => f,
        None => return AppError::not_found("Webhook").into_response(),
    };

    // Validate secret
    if let Some(err) = validate_webhook_secret(&wh, query.secret.as_deref(), &headers) {
        return err.into_response();
    }

    // Load job from DB
    let job = match stroem_db::JobRepo::get(&state.pool, job_id).await {
        Ok(Some(job)) => job,
        Ok(None) => return AppError::not_found("Job").into_response(),
        Err(e) => {
            tracing::error!("Failed to load job {}: {:#}", job_id, e);
            return AppError::Internal(anyhow::anyhow!(e).context("load job")).into_response();
        }
    };

    // Verify job belongs to this webhook trigger
    let expected_source_id = format!("{}/{}", wh.ws_name, wh.trigger_key);
    if job.source_type != "webhook" || job.source_id.as_deref() != Some(&expected_source_id) {
        return AppError::not_found("Job").into_response();
    }

    let is_terminal = is_terminal_status(&job.status);

    // If wait=true and job is not terminal, wait for completion
    if query.wait && !is_terminal {
        let timeout_secs = query
            .timeout
            .unwrap_or(DEFAULT_SYNC_TIMEOUT_SECS)
            .min(MAX_WAIT_TIMEOUT_SECS);
        let mut rx = state.job_completion.subscribe(job_id).await;

        // Re-check after subscribing (race guard)
        if let Ok(Some(fresh_job)) = stroem_db::JobRepo::get(&state.pool, job_id).await {
            if is_terminal_status(&fresh_job.status) {
                return with_no_cache(
                    Json(WebhookJobStatusResponse {
                        job_id: job_id.to_string(),
                        trigger: name,
                        task: wh.task,
                        status: fresh_job.status,
                        output: fresh_job.output,
                        created_at: fresh_job.created_at.to_rfc3339(),
                        completed_at: fresh_job.completed_at.map(|t| t.to_rfc3339()),
                    })
                    .into_response(),
                );
            }
        }

        let timeout = Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Ok(_event)) => {
                // Job completed — re-query DB to get accurate timestamps.
                if let Ok(Some(current)) = stroem_db::JobRepo::get(&state.pool, job_id).await {
                    return with_no_cache(
                        Json(WebhookJobStatusResponse {
                            job_id: job_id.to_string(),
                            trigger: name,
                            task: wh.task,
                            status: current.status,
                            output: current.output,
                            created_at: current.created_at.to_rfc3339(),
                            completed_at: current.completed_at.map(|t| t.to_rfc3339()),
                        })
                        .into_response(),
                    );
                }
                // DB error after completion
                return AppError::Internal(anyhow::anyhow!("Failed to load job after completion"))
                    .into_response();
            }
            Ok(Err(_lagged)) => {
                // Broadcast message was missed — job is likely already terminal. Re-query DB.
                if let Ok(Some(current)) = stroem_db::JobRepo::get(&state.pool, job_id).await {
                    return with_no_cache(
                        Json(WebhookJobStatusResponse {
                            job_id: job_id.to_string(),
                            trigger: name,
                            task: wh.task,
                            status: current.status,
                            output: current.output,
                            created_at: current.created_at.to_rfc3339(),
                            completed_at: current.completed_at.map(|t| t.to_rfc3339()),
                        })
                        .into_response(),
                    );
                }
                return AppError::Internal(anyhow::anyhow!("Failed to load job after lag"))
                    .into_response();
            }
            Err(_elapsed) => {
                // Genuine timeout — return current status with 202 for manual polling
                if let Ok(Some(current)) = stroem_db::JobRepo::get(&state.pool, job_id).await {
                    return with_no_cache(
                        (
                            StatusCode::ACCEPTED,
                            Json(WebhookJobStatusResponse {
                                job_id: job_id.to_string(),
                                trigger: name,
                                task: wh.task,
                                status: current.status,
                                output: current.output,
                                created_at: current.created_at.to_rfc3339(),
                                completed_at: current.completed_at.map(|t| t.to_rfc3339()),
                            }),
                        )
                            .into_response(),
                    );
                }
                return with_no_cache(
                    (
                        StatusCode::ACCEPTED,
                        Json(WebhookJobStatusResponse {
                            job_id: job_id.to_string(),
                            trigger: name,
                            task: wh.task,
                            status: "running".to_string(),
                            output: None,
                            created_at: job.created_at.to_rfc3339(),
                            completed_at: None,
                        }),
                    )
                        .into_response(),
                );
            }
        }
    }

    with_no_cache(
        Json(WebhookJobStatusResponse {
            job_id: job_id.to_string(),
            trigger: name,
            task: wh.task,
            status: job.status,
            output: job.output,
            created_at: job.created_at.to_rfc3339(),
            completed_at: job.completed_at.map(|t| t.to_rfc3339()),
        })
        .into_response(),
    )
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
        let config = match state.workspaces.get_config(ws_name).await {
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
#[cfg(test)]
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

    #[test]
    fn test_validate_webhook_secret_with_valid_secret() {
        let wh = WebhookMatch {
            ws_name: "default".to_string(),
            trigger_key: "test-trigger".to_string(),
            task: "deploy".to_string(),
            secret: Some("my-secret".to_string()),
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
        };
        let headers = HeaderMap::new();
        assert!(validate_webhook_secret(&wh, Some("my-secret"), &headers).is_none());
    }

    #[test]
    fn test_validate_webhook_secret_with_invalid_secret() {
        let wh = WebhookMatch {
            ws_name: "default".to_string(),
            trigger_key: "test-trigger".to_string(),
            task: "deploy".to_string(),
            secret: Some("my-secret".to_string()),
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
        };
        let headers = HeaderMap::new();
        let result = validate_webhook_secret(&wh, Some("wrong-secret"), &headers);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), AppError::Unauthorized(_)));
    }

    #[test]
    fn test_validate_webhook_secret_missing_when_required() {
        let wh = WebhookMatch {
            ws_name: "default".to_string(),
            trigger_key: "test-trigger".to_string(),
            task: "deploy".to_string(),
            secret: Some("my-secret".to_string()),
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
        };
        let headers = HeaderMap::new();
        let result = validate_webhook_secret(&wh, None, &headers);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), AppError::Unauthorized(_)));
    }

    #[test]
    fn test_validate_webhook_secret_open_webhook() {
        let wh = WebhookMatch {
            ws_name: "default".to_string(),
            trigger_key: "test-trigger".to_string(),
            task: "deploy".to_string(),
            secret: None,
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
        };
        let headers = HeaderMap::new();
        // Open webhook — no secret configured, should always allow
        assert!(validate_webhook_secret(&wh, None, &headers).is_none());
    }

    #[test]
    fn test_validate_webhook_secret_via_bearer_header() {
        let wh = WebhookMatch {
            ws_name: "default".to_string(),
            trigger_key: "test-trigger".to_string(),
            task: "deploy".to_string(),
            secret: Some("bearer-secret".to_string()),
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
        };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer bearer-secret".parse().unwrap());
        // No query-param secret — falls back to the Authorization: Bearer header.
        assert!(validate_webhook_secret(&wh, None, &headers).is_none());
    }
}
