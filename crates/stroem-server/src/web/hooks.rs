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

/// Handle incoming webhook requests (spec § 7.5).
///
/// 1. Match an enabled webhook by name and authenticate against that cached
///    definition — before any reload, so an unauthenticated caller cannot
///    trigger a git fetch of a `force_refresh` webhook's workspace. A caller
///    holding only a newly rotated-in secret therefore gets 401 until the
///    server has loaded it (watcher poll or another refresh).
/// 2. If `force_refresh`: reload. A workspace left errored (no config) is a
///    500, never a 404. Match AGAIN in the refreshed config (gone → 404) and
///    re-authenticate against the FRESH definition, so a refresh that changed
///    the `ref`, rotated the secret or removed the webhook wins.
/// 3. Resolve the target task (`ws.task` and/or `ref:`) and create the job
///    in the target workspace.
#[tracing::instrument(skip(state, query, headers, body))]
async fn webhook_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> axum::response::Response {
    let query_secret = query.get("secret").map(String::as_str);

    let cached = match find_webhook_trigger(&state, &name).await {
        Some(f) => f,
        None => return AppError::not_found("Webhook").into_response(),
    };
    if let Some(err) = validate_webhook_secret(&cached, query_secret, &headers) {
        return err.into_response();
    }

    // The definition the job is created from, and the config snapshot it was
    // matched in (the target resolves against that same snapshot).
    let (wh, defining_config) = if cached.force_refresh {
        force_refresh(&state, &cached.ws_name, &name).await;
        // A failed reload (or a busy one over an errored snapshot) leaves the
        // workspace without a config: a server condition, so 500 — never the
        // 404 of a webhook that is gone. `triggers: false` is server config,
        // fixed at startup, and the cached match already passed it.
        let Some(config) = state.workspaces.get_config(&cached.ws_name).await else {
            return AppError::Internal(anyhow::anyhow!(
                "Workspace '{}' is unavailable after force_refresh of webhook '{}'",
                cached.ws_name,
                name
            ))
            .into_response();
        };
        let Some(fresh) = match_webhook(&cached.ws_name, &config, &name) else {
            return AppError::not_found("Webhook").into_response();
        };
        if let Some(err) = validate_webhook_secret(&fresh, query_secret, &headers) {
            return err.into_response();
        }
        (fresh, config)
    } else {
        let Some(config) = state.get_workspace(&cached.ws_name).await else {
            return AppError::Internal(anyhow::anyhow!(
                "Workspace '{}' not found after webhook trigger lookup",
                cached.ws_name
            ))
            .into_response();
        };
        (cached, config)
    };

    let input = build_webhook_input(&method, &headers, &query, &body, &wh.default_input);
    let input_value = serde_json::to_value(&input).unwrap_or_default();
    let source_id = format!("{}/{}", wh.ws_name, wh.trigger_key);

    let target = match crate::trigger_target::resolve_trigger_target(
        &state.workspaces,
        &wh.ws_name,
        &defining_config,
        &wh.task,
        wh.git_ref.as_deref(),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("Webhook '{}' could not resolve its target: {:#}", name, e);
            return crate::web::api::classify_execute_error(e).into_response();
        }
    };

    let is_sync = wh.mode.as_deref() == Some("sync");
    let timeout_secs = wh.timeout_secs.unwrap_or(DEFAULT_SYNC_TIMEOUT_SECS);

    let created = match crate::trigger_target::create_target_job(
        &state,
        &target,
        input_value,
        "webhook",
        &source_id,
    )
    .await
    .context("create webhook job")
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Webhook '{}' failed to create job: {:#}", name, e);
            return crate::web::api::classify_execute_error(e).into_response();
        }
    };
    let job_id = created.job_id;

    tracing::info!(
        "Webhook '{}' created job {} for task '{}' in '{}'",
        name,
        job_id,
        target.task_name,
        target.workspace
    );

    // Initial on_suspended hooks come from the created job's own config
    // (spec § 7.4), not from the webhook's defining workspace.
    crate::settlement::dispatch::fire_initial_suspended_hooks(&state, job_id).await;
    state.settlement().job_created(created).await;

    if !is_sync {
        return Json(WebhookAsyncResponse {
            job_id: job_id.to_string(),
            trigger: name,
            task: wh.task,
        })
        .into_response();
    }

    let mut rx = state.job_completion.subscribe(job_id).await;

    // Guard against the (unlikely) race where the job completed between
    // creation and subscribe.
    if let Ok(Some(job)) = stroem_db::JobRepo::get(&state.pool, job_id).await {
        if is_terminal_status(&job.status) {
            let status = job.status.clone();
            let output = job.output.clone();
            return sync_response(&state, &job, &name, &wh.task, status, output).await;
        }
    }

    match tokio::time::timeout(Duration::from_secs(timeout_secs), rx.recv()).await {
        Ok(Ok(event)) => match stroem_db::JobRepo::get(&state.pool, job_id).await {
            Ok(Some(job)) => {
                sync_response(&state, &job, &name, &wh.task, event.status, event.output).await
            }
            _ => AppError::Internal(anyhow::anyhow!(
                "Failed to load job {job_id} after completion"
            ))
            .into_response(),
        },
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
}

/// `force_refresh`: reload the defining workspace. A busy reload continues
/// with the published snapshot (spec 2026-09-17 § 4.5 (8)); a failed one
/// leaves the workspace errored, which the caller answers with 500.
async fn force_refresh(state: &AppState, ws_name: &str, name: &str) {
    match state.workspaces.reload(ws_name).await {
        Ok(()) => {
            // Notify peer replicas that the workspace has been refreshed so
            // they converge without waiting for their own poll tick.
            state.event_bus.publish_workspace_reloaded(ws_name).await;
        }
        Err(e) if e.downcast_ref::<crate::workspace::ReloadBusy>().is_some() => {
            tracing::info!(
                "Webhook '{}': force_refresh skipped — a reload is already in progress",
                name
            );
        }
        Err(e) => {
            tracing::warn!("Webhook '{}': force_refresh failed: {:#}", name, e);
        }
    }
}

/// Sync webhook response with `output` redacted by the job's per-job set
/// (spec § 7.5); fails closed with 503 + `job_id`.
async fn sync_response(
    state: &AppState,
    job: &stroem_db::JobRow,
    trigger: &str,
    task: &str,
    status: String,
    output: Option<serde_json::Value>,
) -> axum::response::Response {
    match crate::redaction::redact_job_output(state, job, output).await {
        Ok(output) => Json(WebhookSyncResponse {
            job_id: job.job_id.to_string(),
            trigger: trigger.to_string(),
            task: task.to_string(),
            status,
            output,
        })
        .into_response(),
        Err(e) => redaction_failure(job.job_id, e),
    }
}

/// 503 + `job_id` when the job's redaction set is (transiently) incomplete —
/// a permanent pin failure never reaches here, `redact_job_output` masks the
/// whole output instead; 500 for any other error.
fn redaction_failure(job_id: Uuid, e: anyhow::Error) -> axum::response::Response {
    if e.downcast_ref::<crate::redaction::RedactionUnavailable>()
        .is_some()
    {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "job_id": job_id.to_string(),
                "error": "redaction set unavailable, retry",
            })),
        )
            .into_response()
    } else {
        AppError::Internal(e.context("redact webhook output")).into_response()
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
/// Re-exports the canonical definition from `stroem-common` so all "is this
/// job terminal?" checks across crates stay in sync.
fn is_terminal_status(status: &str) -> bool {
    stroem_common::models::job::is_terminal_status(status)
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
/// Authenticated with the webhook's own secret, NOT task ACL — an explicit
/// exception to the job-ACL rule (git-refs spec § 7.8): the caller holds the
/// webhook's secret, and only jobs this webhook created are visible. `output`
/// is redacted with the job's per-job set in every branch (spec § 7.5),
/// failing closed.
/// Supports `?wait=true&timeout=30` to wait for job completion.
#[tracing::instrument(skip(state, query, headers))]
async fn webhook_job_status(
    State(state): State<Arc<AppState>>,
    Path((name, job_id_str)): Path<(String, String)>,
    Query(query): Query<StatusQuery>,
    headers: HeaderMap,
) -> axum::response::Response {
    let job_id = match Uuid::parse_str(&job_id_str) {
        Ok(id) => id,
        Err(_) => return AppError::BadRequest("Invalid job ID".into()).into_response(),
    };

    let wh = match find_webhook_trigger(&state, &name).await {
        Some(f) => f,
        None => return AppError::not_found("Webhook").into_response(),
    };
    if let Some(err) = validate_webhook_secret(&wh, query.secret.as_deref(), &headers) {
        return err.into_response();
    }

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

    if !(query.wait && !is_terminal_status(&job.status)) {
        return status_response(&state, &name, &wh.task, job, false).await;
    }

    let timeout_secs = query
        .timeout
        .unwrap_or(DEFAULT_SYNC_TIMEOUT_SECS)
        .min(MAX_WAIT_TIMEOUT_SECS);
    let mut rx = state.job_completion.subscribe(job_id).await;

    // Re-check after subscribing (race guard)
    if let Ok(Some(fresh)) = stroem_db::JobRepo::get(&state.pool, job_id).await {
        if is_terminal_status(&fresh.status) {
            return status_response(&state, &name, &wh.task, fresh, false).await;
        }
    }

    match tokio::time::timeout(Duration::from_secs(timeout_secs), rx.recv()).await {
        // Completed, or the broadcast was missed (lagged): re-query the DB.
        Ok(_) => match stroem_db::JobRepo::get(&state.pool, job_id).await {
            Ok(Some(current)) => status_response(&state, &name, &wh.task, current, false).await,
            _ => AppError::Internal(anyhow::anyhow!("Failed to load job after completion"))
                .into_response(),
        },
        // Genuine timeout — current status with 202 for manual polling.
        Err(_elapsed) => match stroem_db::JobRepo::get(&state.pool, job_id).await {
            Ok(Some(current)) => status_response(&state, &name, &wh.task, current, true).await,
            _ => with_no_cache(
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
            ),
        },
    }
}

/// One status response for `job`, `output` redacted (fail closed), no-store.
async fn status_response(
    state: &AppState,
    trigger: &str,
    task: &str,
    job: stroem_db::JobRow,
    accepted: bool,
) -> axum::response::Response {
    let output = match crate::redaction::redact_job_output(state, &job, job.output.clone()).await {
        Ok(o) => o,
        Err(e) => return with_no_cache(redaction_failure(job.job_id, e)),
    };
    let body = Json(WebhookJobStatusResponse {
        job_id: job.job_id.to_string(),
        trigger: trigger.to_string(),
        task: task.to_string(),
        status: job.status,
        output,
        created_at: job.created_at.to_rfc3339(),
        completed_at: job.completed_at.map(|t| t.to_rfc3339()),
    });
    let resp = if accepted {
        (StatusCode::ACCEPTED, body).into_response()
    } else {
        body.into_response()
    };
    with_no_cache(resp)
}

/// Search result from find_webhook_trigger.
struct WebhookMatch {
    ws_name: String,
    trigger_key: String,
    task: String,
    /// `ref:` of the target task (spec § 4.1), resolved per fire.
    git_ref: Option<String>,
    secret: Option<String>,
    default_input: HashMap<String, serde_json::Value>,
    mode: Option<String>,
    timeout_secs: Option<u64>,
    force_refresh: bool,
}

/// The enabled webhook named `name` in one workspace's config, if any.
fn match_webhook(
    ws_name: &str,
    config: &stroem_common::models::workflow::WorkspaceConfig,
    name: &str,
) -> Option<WebhookMatch> {
    config
        .triggers
        .iter()
        .find_map(|(trigger_key, trigger_def)| match trigger_def {
            TriggerDef::Webhook {
                name: wh_name,
                task,
                secret,
                input,
                enabled,
                mode,
                timeout_secs,
                force_refresh,
                git_ref,
            } if wh_name == name && *enabled => Some(WebhookMatch {
                ws_name: ws_name.to_string(),
                trigger_key: trigger_key.clone(),
                task: task.clone(),
                git_ref: git_ref.clone(),
                secret: secret.clone(),
                default_input: input.clone(),
                mode: mode.clone(),
                timeout_secs: *timeout_secs,
                force_refresh: *force_refresh,
            }),
            _ => None,
        })
}

/// Find the first enabled webhook trigger matching the given name.
async fn find_webhook_trigger(state: &AppState, name: &str) -> Option<WebhookMatch> {
    for ws_name in state.workspaces.names() {
        if let Some(m) = find_webhook_trigger_in(state, ws_name, name).await {
            return Some(m);
        }
    }
    None
}

/// The webhook `name` in ONE workspace's currently published config.
async fn find_webhook_trigger_in(
    state: &AppState,
    ws_name: &str,
    name: &str,
) -> Option<WebhookMatch> {
    // `workspaces.<name>.triggers: false` in the server config: the
    // workspace's webhooks are invisible here, same as `enabled: false`.
    if !state.workspaces.triggers_enabled(ws_name) {
        return None;
    }
    let config = state.workspaces.get_config(ws_name).await?;
    match_webhook(ws_name, &config, name)
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

    fn webhook_config(hook_name: &str) -> stroem_common::models::workflow::WorkspaceConfig {
        let mut config = stroem_common::models::workflow::WorkspaceConfig::new();
        config.triggers.insert(
            "incoming".to_string(),
            TriggerDef::Webhook {
                git_ref: None,
                name: hook_name.to_string(),
                task: "handler".to_string(),
                secret: None,
                input: HashMap::new(),
                enabled: true,
                mode: None,
                timeout_secs: None,
                force_refresh: false,
            },
        );
        config
    }

    #[test]
    fn match_webhook_returns_ref_and_skips_disabled() {
        let mut config = stroem_common::models::workflow::WorkspaceConfig::new();
        config.triggers.insert(
            "on-nightly".to_string(),
            TriggerDef::Webhook {
                name: "nightly-hook".to_string(),
                task: "billing.nightly".to_string(),
                secret: Some("s".to_string()),
                input: HashMap::new(),
                enabled: true,
                mode: None,
                timeout_secs: None,
                force_refresh: true,
                git_ref: Some("release/2.3".to_string()),
            },
        );
        let m = match_webhook("etl", &config, "nightly-hook").expect("matches");
        assert_eq!(m.ws_name, "etl");
        assert_eq!(m.trigger_key, "on-nightly");
        assert_eq!(m.task, "billing.nightly");
        assert_eq!(m.git_ref.as_deref(), Some("release/2.3"));
        assert!(m.force_refresh);

        if let Some(TriggerDef::Webhook { enabled, .. }) = config.triggers.get_mut("on-nightly") {
            *enabled = false;
        }
        assert!(match_webhook("etl", &config, "nightly-hook").is_none());
        assert!(match_webhook("etl", &config, "other").is_none());
    }

    #[tokio::test]
    async fn test_find_webhook_trigger_skips_workspace_with_triggers_disabled() {
        use crate::state::test_app_state_with_workspaces;
        use crate::workspace::WorkspaceManager;

        // Same webhook name in both workspaces: only the enabled one may match.
        let mgr = WorkspaceManager::from_configs(vec![
            ("quiet".to_string(), webhook_config("deploy"), None),
            ("loud".to_string(), webhook_config("deploy"), None),
        ])
        .with_triggers_disabled("quiet");
        let temp = tempfile::TempDir::new().unwrap();
        let state = test_app_state_with_workspaces(mgr, temp.path());

        let found = find_webhook_trigger(&state, "deploy")
            .await
            .expect("enabled workspace's webhook must be found");
        assert_eq!(found.ws_name, "loud");
        assert_eq!(found.trigger_key, "incoming");
    }

    #[tokio::test]
    async fn test_find_webhook_trigger_none_when_only_disabled_workspace_matches() {
        use crate::state::test_app_state_with_workspaces;
        use crate::workspace::WorkspaceManager;

        let mgr = WorkspaceManager::from_configs(vec![
            ("quiet".to_string(), webhook_config("deploy"), None),
            ("loud".to_string(), webhook_config("other"), None),
        ])
        .with_triggers_disabled("quiet");
        let temp = tempfile::TempDir::new().unwrap();
        let state = test_app_state_with_workspaces(mgr, temp.path());

        assert!(find_webhook_trigger(&state, "deploy").await.is_none());
        assert!(find_webhook_trigger(&state, "other").await.is_some());
    }

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
            git_ref: None,
            secret: Some("my-secret".to_string()),
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
            force_refresh: false,
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
            git_ref: None,
            secret: Some("my-secret".to_string()),
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
            force_refresh: false,
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
            git_ref: None,
            secret: Some("my-secret".to_string()),
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
            force_refresh: false,
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
            git_ref: None,
            secret: None,
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
            force_refresh: false,
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
            git_ref: None,
            secret: Some("bearer-secret".to_string()),
            default_input: HashMap::new(),
            mode: None,
            timeout_secs: None,
            force_refresh: false,
        };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer bearer-secret".parse().unwrap());
        // No query-param secret — falls back to the Authorization: Bearer header.
        assert!(validate_webhook_secret(&wh, None, &headers).is_none());
    }
}
