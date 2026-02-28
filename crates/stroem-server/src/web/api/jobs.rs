use crate::log_storage::JobLogMeta;
use crate::state::AppState;
use crate::web::api::{default_limit, parse_uuid_param};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ListJobsQuery {
    pub workspace: Option<String>,
    pub task_name: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// GET /api/jobs - List jobs
#[tracing::instrument(skip(state))]
pub async fn list_jobs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListJobsQuery>,
) -> impl IntoResponse {
    if query.task_name.is_some() && query.workspace.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "task_name filter requires workspace"})),
        )
            .into_response();
    }

    let (result, total) = match (query.workspace.as_deref(), query.task_name.as_deref()) {
        (Some(ws), Some(task)) => {
            let jobs =
                JobRepo::list_by_task(&state.pool, ws, task, query.limit, query.offset).await;
            let count = JobRepo::count_by_task(&state.pool, ws, task).await;
            (jobs, count)
        }
        _ => {
            let ws = query.workspace.as_deref();
            let jobs = JobRepo::list(&state.pool, ws, query.limit, query.offset).await;
            let count = JobRepo::count(&state.pool, ws).await;
            (jobs, count)
        }
    };

    match (result, total) {
        (Ok(jobs), Ok(total)) => {
            let jobs_json: Vec<serde_json::Value> = jobs
                .iter()
                .map(|job| {
                    json!({
                        "job_id": job.job_id,
                        "workspace": job.workspace,
                        "task_name": job.task_name,
                        "mode": job.mode,
                        "status": job.status,
                        "source_type": job.source_type,
                        "source_id": job.source_id,
                        "worker_id": job.worker_id,
                        "created_at": job.created_at,
                        "started_at": job.started_at,
                        "completed_at": job.completed_at,
                    })
                })
                .collect();

            Json(json!({ "items": jobs_json, "total": total })).into_response()
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!("Failed to list jobs: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list jobs: {}", e)})),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JobDetailResponse {
    pub job_id: Uuid,
    pub workspace: String,
    pub task_name: String,
    pub mode: String,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub status: String,
    pub source_type: String,
    pub source_id: Option<String>,
    pub worker_id: Option<Uuid>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub steps: Vec<serde_json::Value>,
}

/// GET /api/jobs/:id - Get job detail with steps
#[tracing::instrument(skip(state))]
pub async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let job_id = match parse_uuid_param(&id, "job") {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    // Get job
    let job = match JobRepo::get(&state.pool, job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Job not found"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to get job: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get job: {}", e)})),
            )
                .into_response();
        }
    };

    // Get steps
    let steps = match JobStepRepo::get_steps_for_job(&state.pool, job_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to get job steps: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get job steps: {}", e)})),
            )
                .into_response();
        }
    };

    let mut steps_json: Vec<serde_json::Value> = steps
        .iter()
        .map(|step| {
            json!({
                "step_name": step.step_name,
                "action_name": step.action_name,
                "action_type": step.action_type,
                "action_image": step.action_image,
                "runner": step.runner,
                "input": step.input,
                "output": step.output,
                "status": step.status,
                "worker_id": step.worker_id,
                "started_at": step.started_at,
                "completed_at": step.completed_at,
                "error_message": step.error_message,
                "depends_on": serde_json::Value::Array(vec![]),
            })
        })
        .collect();

    // Look up workspace once — used for topo-sort and secret redaction
    let workspace = state.get_workspace(&job.workspace).await;

    // Sort steps by topological order (dependency-first) using the task flow
    if let Some(ref ws) = workspace {
        if let Some(task) = ws.tasks.get(&job.task_name) {
            let mut in_deg: HashMap<&str, usize> = task
                .flow
                .iter()
                .map(|(name, fs)| (name.as_str(), fs.depends_on.len()))
                .collect();

            let mut queue: Vec<&str> = in_deg
                .iter()
                .filter(|(_, &d)| d == 0)
                .map(|(&n, _)| n)
                .collect();
            queue.sort();

            let mut topo_order: Vec<&str> = Vec::new();
            while let Some(node) = queue.first().copied() {
                queue.remove(0);
                topo_order.push(node);
                for (name, fs) in &task.flow {
                    if fs.depends_on.iter().any(|d| d == node) {
                        if let Some(deg) = in_deg.get_mut(name.as_str()) {
                            *deg -= 1;
                            if *deg == 0 {
                                queue.push(name.as_str());
                                queue.sort();
                            }
                        }
                    }
                }
            }

            let pos: HashMap<&str, usize> = topo_order
                .iter()
                .enumerate()
                .map(|(i, &n)| (n, i))
                .collect();
            steps_json.sort_by_key(|s| {
                s["step_name"]
                    .as_str()
                    .and_then(|n| pos.get(n).copied())
                    .unwrap_or(usize::MAX)
            });

            // Enrich steps with depends_on from task flow
            for step_json in &mut steps_json {
                if let Some(step_name) = step_json["step_name"].as_str() {
                    if let Some(flow_step) = task.flow.get(step_name) {
                        step_json["depends_on"] = json!(&flow_step.depends_on);
                    }
                }
            }
        }
    }

    let mut response = JobDetailResponse {
        job_id: job.job_id,
        workspace: job.workspace,
        task_name: job.task_name,
        mode: job.mode,
        input: job.input,
        output: job.output,
        status: job.status,
        source_type: job.source_type,
        source_id: job.source_id,
        worker_id: job.worker_id,
        created_at: job.created_at.to_rfc3339(),
        started_at: job.started_at.map(|dt| dt.to_rfc3339()),
        completed_at: job.completed_at.map(|dt| dt.to_rfc3339()),
        steps: steps_json,
    };

    // Redact workspace secrets and ref+ patterns from response
    let secret_values = workspace
        .map(|ws| collect_secret_values(&ws.secrets))
        .unwrap_or_default();
    redact_response(&mut response, &secret_values);

    Json(response).into_response()
}

const REDACTED: &str = "••••••";

/// Collect all leaf string values from the secrets map (flattening nested objects).
/// Filters out short values (<=3 chars) to avoid false positives.
fn collect_secret_values(secrets: &HashMap<String, serde_json::Value>) -> Vec<String> {
    let mut values = Vec::new();
    for value in secrets.values() {
        collect_strings(value, &mut values);
    }
    values.retain(|v| v.len() > 3);
    values
}

fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Replace secret values and `ref+` references in a JSON tree with REDACTED.
fn redact_json(value: &mut serde_json::Value, secret_values: &[String]) {
    match value {
        serde_json::Value::String(s) => {
            if s.starts_with("ref+") {
                *s = REDACTED.to_string();
                return;
            }
            for secret in secret_values {
                if s.contains(secret.as_str()) {
                    *s = s.replace(secret.as_str(), REDACTED);
                }
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                redact_json(v, secret_values);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_json(v, secret_values);
            }
        }
        _ => {}
    }
}

/// Redact secrets from a JobDetailResponse (job input/output + step input/output).
fn redact_response(response: &mut JobDetailResponse, secret_values: &[String]) {
    if let Some(ref mut input) = response.input {
        redact_json(input, secret_values);
    }
    if let Some(ref mut output) = response.output {
        redact_json(output, secret_values);
    }
    for step in &mut response.steps {
        if let Some(input) = step.get_mut("input") {
            redact_json(input, secret_values);
        }
        if let Some(output) = step.get_mut("output") {
            redact_json(output, secret_values);
        }
        if let Some(error_message) = step.get_mut("error_message") {
            redact_json(error_message, secret_values);
        }
    }
}

/// GET /api/jobs/:id/steps/:step/logs - Get per-step logs
#[tracing::instrument(skip(state))]
pub async fn get_step_logs(
    State(state): State<Arc<AppState>>,
    Path((id, step_name)): Path<(String, String)>,
) -> impl IntoResponse {
    let job_id = match parse_uuid_param(&id, "job") {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let job = match JobRepo::get(&state.pool, job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Job not found"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to get job: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get job: {}", e)})),
            )
                .into_response();
        }
    };

    let meta = JobLogMeta {
        workspace: job.workspace,
        task_name: job.task_name,
        created_at: job.created_at,
    };

    match state
        .log_storage
        .get_step_log(job_id, &step_name, &meta)
        .await
    {
        Ok(logs) => Json(json!({"logs": logs})).into_response(),
        Err(e) => {
            tracing::error!("Failed to get step logs: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get step logs: {}", e)})),
            )
                .into_response()
        }
    }
}

/// GET /api/jobs/:id/logs - Get log file contents
#[tracing::instrument(skip(state))]
pub async fn get_job_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let job_id = match parse_uuid_param(&id, "job") {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let job = match JobRepo::get(&state.pool, job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Job not found"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to get job: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get job: {}", e)})),
            )
                .into_response();
        }
    };

    let meta = JobLogMeta {
        workspace: job.workspace,
        task_name: job.task_name,
        created_at: job.created_at,
    };

    match state.log_storage.get_log(job_id, &meta).await {
        Ok(logs) => Json(json!({"logs": logs})).into_response(),
        Err(e) => {
            tracing::error!("Failed to get logs: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get logs: {}", e)})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_collect_secret_values() {
        let mut secrets = HashMap::new();
        secrets.insert("password".to_string(), json!("s3cr3t-value"));
        secrets.insert("token".to_string(), json!("tok_abc123"));
        // Nested object
        secrets.insert(
            "db".to_string(),
            json!({"host": "db.example.com", "pass": "db-pass-xyz"}),
        );
        // Short value should be filtered out
        secrets.insert("pin".to_string(), json!("12"));

        let values = collect_secret_values(&secrets);
        assert!(values.contains(&"s3cr3t-value".to_string()));
        assert!(values.contains(&"tok_abc123".to_string()));
        assert!(values.contains(&"db.example.com".to_string()));
        assert!(values.contains(&"db-pass-xyz".to_string()));
        // Short value filtered
        assert!(!values.contains(&"12".to_string()));
    }

    #[test]
    fn test_redact_json_exact_match() {
        let secrets = vec!["s3cr3t-value".to_string()];
        let mut value = json!("s3cr3t-value");
        redact_json(&mut value, &secrets);
        assert_eq!(value, json!(REDACTED));
    }

    #[test]
    fn test_redact_json_substring_match() {
        let secrets = vec!["tok_abc123".to_string()];
        let mut value = json!("Bearer tok_abc123");
        redact_json(&mut value, &secrets);
        assert_eq!(value, json!(format!("Bearer {REDACTED}")));
    }

    #[test]
    fn test_redact_json_nested() {
        let secrets = vec!["s3cr3t".to_string()];
        let mut value = json!({
            "url": "https://hooks.slack.com/s3cr3t/path",
            "nested": {
                "key": "s3cr3t"
            },
            "list": ["safe", "s3cr3t", "also-safe"]
        });
        redact_json(&mut value, &secrets);
        assert_eq!(
            value["url"],
            json!(format!("https://hooks.slack.com/{REDACTED}/path"))
        );
        assert_eq!(value["nested"]["key"], json!(REDACTED));
        assert_eq!(value["list"][0], json!("safe"));
        assert_eq!(value["list"][1], json!(REDACTED));
        assert_eq!(value["list"][2], json!("also-safe"));
    }

    #[test]
    fn test_redact_json_no_match() {
        let secrets = vec!["s3cr3t".to_string()];
        let mut value = json!({"safe": "no-secrets-here", "number": 42});
        let original = value.clone();
        redact_json(&mut value, &secrets);
        assert_eq!(value, original);
    }

    #[test]
    fn test_redact_json_vals_reference() {
        let secrets = vec![];
        let mut value = json!({
            "password": "ref+awsssm:///prod/db/password",
            "vault": "ref+vault://secret/data/key",
            "safe": "not-a-ref"
        });
        redact_json(&mut value, &secrets);
        assert_eq!(value["password"], json!(REDACTED));
        assert_eq!(value["vault"], json!(REDACTED));
        assert_eq!(value["safe"], json!("not-a-ref"));
    }

    #[test]
    fn test_redact_json_multiple_secrets_in_one_string() {
        let secrets = vec!["user123".to_string(), "pass456".to_string()];
        let mut value = json!("postgres://user123:pass456@db.host/mydb");
        redact_json(&mut value, &secrets);
        assert_eq!(
            value,
            json!(format!("postgres://{REDACTED}:{REDACTED}@db.host/mydb"))
        );
    }

    #[test]
    fn test_redact_json_ref_plus_no_secret_values() {
        // ref+ patterns must be redacted even when secret_values is empty
        let secrets: Vec<String> = vec![];
        let mut value = json!({"key": "ref+gcpsecrets://project/secret"});
        redact_json(&mut value, &secrets);
        assert_eq!(value["key"], json!(REDACTED));
    }

    #[test]
    fn test_redact_response() {
        let secrets = vec!["my-secret-token".to_string()];
        let mut response = JobDetailResponse {
            job_id: Uuid::nil(),
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            mode: "normal".to_string(),
            input: Some(json!({"token": "my-secret-token"})),
            output: Some(json!({"result": "ok"})),
            status: "completed".to_string(),
            source_type: "api".to_string(),
            source_id: None,
            worker_id: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            started_at: None,
            completed_at: None,
            steps: vec![json!({
                "step_name": "deploy",
                "input": {"webhook": "https://hooks.example.com/my-secret-token"},
                "output": {"status": "done"},
                "error_message": "failed to connect: my-secret-token rejected"
            })],
        };
        redact_response(&mut response, &secrets);
        assert_eq!(response.input.unwrap()["token"], json!(REDACTED));
        assert_eq!(response.output.unwrap()["result"], json!("ok"));
        assert_eq!(
            response.steps[0]["input"]["webhook"],
            json!(format!("https://hooks.example.com/{REDACTED}"))
        );
        assert_eq!(
            response.steps[0]["error_message"],
            json!(format!("failed to connect: {REDACTED} rejected"))
        );
    }

    #[test]
    fn test_depends_on_enriched_from_flow() {
        use stroem_common::models::workflow::FlowStep;

        // Build a task flow with dependencies
        let mut flow = HashMap::new();
        flow.insert(
            "build".to_string(),
            FlowStep {
                action: "shell/bash".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                inline_action: None,
            },
        );
        flow.insert(
            "test".to_string(),
            FlowStep {
                action: "shell/bash".to_string(),
                name: None,
                description: None,
                depends_on: vec!["build".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
                inline_action: None,
            },
        );
        flow.insert(
            "deploy".to_string(),
            FlowStep {
                action: "shell/bash".to_string(),
                name: None,
                description: None,
                depends_on: vec!["build".to_string(), "test".to_string()],
                input: HashMap::new(),
                continue_on_failure: false,
                inline_action: None,
            },
        );

        // Build step JSON with empty depends_on (as created by the handler)
        let mut steps_json: Vec<serde_json::Value> = vec![
            json!({"step_name": "build", "depends_on": []}),
            json!({"step_name": "test", "depends_on": []}),
            json!({"step_name": "deploy", "depends_on": []}),
        ];

        // Enrich with depends_on from flow (same logic as get_job handler)
        for step_json in &mut steps_json {
            if let Some(step_name) = step_json["step_name"].as_str() {
                if let Some(flow_step) = flow.get(step_name) {
                    step_json["depends_on"] = json!(&flow_step.depends_on);
                }
            }
        }

        assert_eq!(steps_json[0]["depends_on"], json!([]));
        assert_eq!(steps_json[1]["depends_on"], json!(["build"]));
        assert_eq!(steps_json[2]["depends_on"], json!(["build", "test"]));
    }

    #[test]
    fn test_depends_on_empty_for_missing_flow() {
        // Steps not found in the flow should keep the default empty array
        let flow: HashMap<String, stroem_common::models::workflow::FlowStep> = HashMap::new();

        let mut steps_json: Vec<serde_json::Value> =
            vec![json!({"step_name": "orphan_step", "depends_on": []})];

        // Enrich — flow is empty, so no matches
        for step_json in &mut steps_json {
            if let Some(step_name) = step_json["step_name"].as_str() {
                if let Some(flow_step) = flow.get(step_name) {
                    step_json["depends_on"] = json!(&flow_step.depends_on);
                }
            }
        }

        assert_eq!(steps_json[0]["depends_on"], json!([]));
    }
}
