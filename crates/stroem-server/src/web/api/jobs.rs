use crate::acl::{load_user_acl_context, make_task_path, AllowedScope, TaskPermission};
use crate::log_storage::JobLogMeta;
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::api::{default_limit, parse_uuid_param};
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ListJobsQuery {
    pub workspace: Option<String>,
    pub task_name: Option<String>,
    pub status: Option<String>,
    pub search: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

const VALID_STATUSES: &[&str] = &[
    "pending",
    "running",
    "completed",
    "failed",
    "cancelled",
    "skipped",
];

#[derive(Debug, Serialize)]
pub struct DashboardStats {
    pub pending: i64,
    pub running: i64,
    pub completed: i64,
    pub failed: i64,
    pub cancelled: i64,
    pub skipped: i64,
}

/// GET /api/stats - Accurate job status counts for the dashboard
#[tracing::instrument(skip(state))]
pub async fn get_stats(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
) -> Result<impl IntoResponse, AppError> {
    // Resolve ACL scope when auth is present and ACL is configured
    let acl_pairs = resolve_acl_scope(&state, &auth_user).await?;

    let counts = match acl_pairs {
        Some(ref pairs) => JobRepo::get_status_counts_with_acl(&state.pool, pairs)
            .await
            .context("get status counts with ACL")?,
        None => JobRepo::get_status_counts(&state.pool)
            .await
            .context("get status counts")?,
    };

    let stats = DashboardStats {
        pending: *counts.get("pending").unwrap_or(&0),
        running: *counts.get("running").unwrap_or(&0),
        completed: *counts.get("completed").unwrap_or(&0),
        failed: *counts.get("failed").unwrap_or(&0),
        cancelled: *counts.get("cancelled").unwrap_or(&0),
        skipped: *counts.get("skipped").unwrap_or(&0),
    };

    Ok((StatusCode::OK, Json(stats)))
}

/// GET /api/jobs - List jobs
#[tracing::instrument(skip(state))]
pub async fn list_jobs(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Query(query): Query<ListJobsQuery>,
) -> Result<Response, AppError> {
    if query.task_name.is_some() && query.workspace.is_none() {
        return Err(AppError::BadRequest(
            "task_name filter requires workspace".into(),
        ));
    }

    if query.task_name.is_some() && query.search.is_some() {
        return Err(AppError::BadRequest(
            "Cannot use both task_name and search filters".into(),
        ));
    }

    if let Some(ref s) = query.status {
        if !VALID_STATUSES.contains(&s.as_str()) {
            return Err(AppError::BadRequest(format!(
                "Invalid status filter '{}'. Must be one of: {}",
                s,
                VALID_STATUSES.join(", ")
            )));
        }
    }

    // Trim and validate search parameter
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(s) = search {
        if s.len() > 200 {
            return Err(AppError::BadRequest(
                "Search term must be 200 characters or fewer".into(),
            ));
        }
    }

    // Resolve ACL scope
    let acl_pairs = resolve_acl_scope(&state, &auth_user).await?;

    let status = query.status.as_deref();

    let (result, total) = match acl_pairs {
        // ACL filtering is active — use ACL-aware queries
        Some(ref pairs) => {
            // If the query has a workspace/task filter, intersect with allowed pairs
            let effective_pairs: Vec<(String, String)> =
                match (query.workspace.as_deref(), query.task_name.as_deref()) {
                    (Some(ws), Some(task)) => pairs
                        .iter()
                        .filter(|(p_ws, p_task)| p_ws == ws && p_task == task)
                        .cloned()
                        .collect(),
                    (Some(ws), None) => pairs
                        .iter()
                        .filter(|(p_ws, _)| p_ws == ws)
                        .cloned()
                        .collect(),
                    _ => pairs.clone(),
                };
            let jobs = JobRepo::list_with_acl(
                &state.pool,
                &effective_pairs,
                status,
                search,
                query.limit,
                query.offset,
            )
            .await;
            let count =
                JobRepo::count_with_acl(&state.pool, &effective_pairs, status, search).await;
            (jobs, count)
        }
        // No ACL filtering — use existing queries
        None => match (query.workspace.as_deref(), query.task_name.as_deref()) {
            (Some(ws), Some(task)) => {
                let jobs =
                    JobRepo::list_by_task(&state.pool, ws, task, status, query.limit, query.offset)
                        .await;
                let count = JobRepo::count_by_task(&state.pool, ws, task, status).await;
                (jobs, count)
            }
            _ => {
                let ws = query.workspace.as_deref();
                let jobs =
                    JobRepo::list(&state.pool, ws, status, search, query.limit, query.offset).await;
                let count = JobRepo::count(&state.pool, ws, status, search).await;
                (jobs, count)
            }
        },
    };

    let jobs = result.context("list jobs")?;
    let total = total.context("count jobs")?;

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
                "revision": job.revision,
                "created_at": job.created_at,
                "started_at": job.started_at,
                "completed_at": job.completed_at,
                "retry_of_job_id": job.retry_of_job_id,
                "retry_job_id": job.retry_job_id,
                "retry_attempt": job.retry_attempt,
                "max_retries": job.max_retries,
            })
        })
        .collect();

    Ok(Json(json!({ "items": jobs_json, "total": total })).into_response())
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
    pub revision: Option<String>,
    pub worker_id: Option<Uuid>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub steps: Vec<serde_json::Value>,
    pub retry_of_job_id: Option<Uuid>,
    pub retry_job_id: Option<Uuid>,
    pub retry_attempt: i32,
    pub max_retries: Option<i32>,
}

/// GET /api/jobs/:id - Get job detail with steps
#[tracing::instrument(skip(state))]
pub async fn get_job(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;

    // Get job
    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    // ACL check
    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    if matches!(perm, TaskPermission::Deny) {
        return Err(AppError::not_found("Job"));
    }

    // Get steps
    let steps = JobStepRepo::get_steps_for_job(&state.pool, job_id)
        .await
        .context("get job steps")?;

    let mut steps_json: Vec<serde_json::Value> = steps
        .iter()
        .map(|step| {
            let mut step_json = json!({
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
                "suspended_at": step.suspended_at,
                "error_message": step.error_message,
                "when_condition": step.when_condition,
                "for_each_expr": step.for_each_expr,
                "loop_source": step.loop_source,
                "loop_index": step.loop_index,
                "loop_total": step.loop_total,
                "depends_on": serde_json::Value::Array(vec![]),
                "retry_attempt": step.retry_attempt,
                "max_retries": step.max_retries,
                "retry_history": step.retry_history,
                "retry_at": step.retry_at,
            });
            // For approval and agent steps, always surface approval-specific fields so
            // the UI can show the message and input schema after the step leaves the
            // suspended state (e.g. approved, rejected, or timed out). (FIX 7)
            if step.action_type == "approval" || step.action_type == "agent" {
                if let Some(ref output) = step.output {
                    if let Some(msg) = output.get("approval_message") {
                        step_json["approval_message"] = msg.clone();
                    }
                }
                if let Some(ref spec) = step.action_spec {
                    if let Some(input_schema) = spec.get("input") {
                        step_json["approval_fields"] = input_schema.clone();
                    }
                }
            }
            step_json
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
            // Sort by topo position. Loop instance steps (e.g. "process[0]")
            // get the same position as their placeholder, sub-sorted by loop_index.
            steps_json.sort_by(|a, b| {
                let get_sort_key = |s: &serde_json::Value| -> (usize, i64) {
                    let name = s["step_name"].as_str().unwrap_or("");
                    // Check if this is a loop instance step
                    if let Some(bracket_pos) = name.find('[') {
                        let source = &name[..bracket_pos];
                        let topo_pos = pos.get(source).copied().unwrap_or(usize::MAX);
                        let loop_index = s["loop_index"].as_i64().unwrap_or(0);
                        // Instance steps sort after their placeholder (add 1 fractionally via sub-key)
                        (topo_pos, loop_index + 1)
                    } else {
                        let topo_pos = pos.get(name).copied().unwrap_or(usize::MAX);
                        (topo_pos, 0) // Placeholder sorts before instances
                    }
                };
                get_sort_key(a).cmp(&get_sort_key(b))
            });

            // Enrich steps with depends_on from task flow.
            // Loop instance steps inherit depends_on from their placeholder.
            // when_condition is already set from the DB (captured at job creation)
            // and must not be overwritten here.
            for step_json in &mut steps_json {
                if let Some(step_name) = step_json["step_name"].as_str() {
                    if let Some(flow_step) = task.flow.get(step_name) {
                        step_json["depends_on"] = json!(&flow_step.depends_on);
                    } else if let Some(bracket_pos) = step_name.find('[') {
                        // Loop instance — look up by source name
                        let source = &step_name[..bracket_pos];
                        if let Some(flow_step) = task.flow.get(source) {
                            step_json["depends_on"] = json!(&flow_step.depends_on);
                        }
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
        revision: job.revision,
        worker_id: job.worker_id,
        created_at: job.created_at.to_rfc3339(),
        started_at: job.started_at.map(|dt| dt.to_rfc3339()),
        completed_at: job.completed_at.map(|dt| dt.to_rfc3339()),
        steps: steps_json,
        retry_of_job_id: job.retry_of_job_id,
        retry_job_id: job.retry_job_id,
        retry_attempt: job.retry_attempt,
        max_retries: job.max_retries,
    };

    // Redact workspace secrets and ref+ patterns from response
    let secret_values = workspace
        .map(|ws| collect_secret_values(&ws.secrets))
        .unwrap_or_default();
    redact_response(&mut response, &secret_values);

    Ok(Json(response))
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
    auth_user: Option<AuthUser>,
    Path((id, step_name)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;

    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    // ACL check
    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    if matches!(perm, TaskPermission::Deny) {
        return Err(AppError::not_found("Job"));
    }

    let meta = JobLogMeta {
        workspace: job.workspace,
        task_name: job.task_name,
        created_at: job.created_at,
    };

    let logs = state
        .log_storage
        .get_step_log(job_id, &step_name, &meta)
        .await
        .context("get step logs")?;

    Ok(Json(json!({"logs": logs})))
}

/// GET /api/jobs/:id/logs - Get log file contents
#[tracing::instrument(skip(state))]
pub async fn get_job_logs(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;

    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    // ACL check
    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    if matches!(perm, TaskPermission::Deny) {
        return Err(AppError::not_found("Job"));
    }

    let meta = JobLogMeta {
        workspace: job.workspace,
        task_name: job.task_name,
        created_at: job.created_at,
    };

    let logs = state
        .log_storage
        .get_log(job_id, &meta)
        .await
        .context("get job logs")?;

    Ok(Json(json!({"logs": logs})))
}

/// POST /api/jobs/:id/cancel - Cancel a running or pending job
#[tracing::instrument(skip(state))]
pub async fn cancel_job(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;

    // Load job for ACL check (also needed for status check if ACL is off)
    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    // ACL check — cancel requires Run permission
    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    match perm {
        TaskPermission::Deny => {
            return Err(AppError::not_found("Job"));
        }
        TaskPermission::View => {
            return Err(AppError::Forbidden(
                "Insufficient permissions to cancel this job".into(),
            ));
        }
        TaskPermission::Run => {}
    }

    match crate::cancellation::cancel_job(&state, job_id)
        .await
        .context("cancel job")?
    {
        crate::cancellation::CancelResult::Cancelled => {
            Ok(Json(json!({"status": "cancelled"})).into_response())
        }
        crate::cancellation::CancelResult::NotFound => Err(AppError::not_found("Job")),
        crate::cancellation::CancelResult::AlreadyTerminal => Err(AppError::Conflict(
            "Job is already in a terminal state".into(),
        )),
    }
}

/// Request body for the approve/reject endpoint.
#[derive(Debug, Deserialize)]
pub struct ApproveStepRequest {
    pub approved: bool,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    pub rejection_reason: Option<String>,
}

/// Extract the user response text from an agent ask_user approval input.
/// If input has a single "response" key with a non-empty string, use it directly
/// so the LLM sees plain text instead of JSON. Otherwise falls back to JSON serialization.
fn extract_agent_response(input: Option<&serde_json::Value>) -> String {
    match input {
        Some(v) => {
            // Try single "response" key with a non-empty string value
            if let Some(obj) = v.as_object() {
                if obj.len() == 1 {
                    if let Some(s) = obj.get("response").and_then(|r| r.as_str()) {
                        if !s.is_empty() {
                            return s.to_string();
                        }
                    }
                }
            }
            // Fallback: JSON-serialize the whole input
            serde_json::to_string(v).unwrap_or_else(|e| {
                tracing::warn!("failed to serialize agent approval input: {e}");
                "Approved".to_string()
            })
        }
        None => "Approved".to_string(),
    }
}

/// POST /api/jobs/:id/steps/:step/approve — Approve or reject a suspended approval step.
#[tracing::instrument(skip(state))]
pub async fn approve_step(
    State(state): State<Arc<AppState>>,
    auth_user: Option<AuthUser>,
    Path((id, step_name)): Path<(String, String)>,
    Json(req): Json<ApproveStepRequest>,
) -> Result<impl IntoResponse, AppError> {
    let job_id = parse_uuid_param(&id, "job")?;

    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    // ACL check — approve/reject requires Run permission
    let perm = check_job_acl(&state, &auth_user, &job.workspace, &job.task_name).await?;
    match perm {
        TaskPermission::Deny => return Err(AppError::not_found("Job")),
        TaskPermission::View => {
            return Err(AppError::Forbidden(
                "Insufficient permissions to approve this step".into(),
            ))
        }
        TaskPermission::Run => {}
    }

    // Find the step and verify it is suspended
    let steps = JobStepRepo::get_steps_for_job(&state.pool, job_id)
        .await
        .context("get job steps")?;

    let step = steps
        .iter()
        .find(|s| s.step_name == step_name)
        .ok_or_else(|| AppError::not_found("Step"))?;

    if step.status != "suspended" {
        return Err(AppError::Conflict(format!(
            "Step '{}' is not in suspended state (current: {})",
            step_name, step.status
        )));
    }
    if step.action_type != "approval" && step.action_type != "agent" {
        return Err(AppError::Conflict(format!(
            "Step '{}' is not an approval or agent step (type: {})",
            step_name, step.action_type
        )));
    }

    // Resolve approver identity
    let approver = auth_user
        .as_ref()
        .map(|u| u.claims.email.clone())
        .unwrap_or_else(|| "anonymous".to_string());

    // Validate required input fields against the action schema (FIX 4)
    if req.approved {
        if let (Some(ref input), Some(ref spec)) = (&req.input, &step.action_spec) {
            if let Some(input_schema) = spec.get("input").and_then(|v| v.as_object()) {
                for (field_name, field_def) in input_schema {
                    let required = field_def
                        .get("required")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if required {
                        let has_value = input
                            .get(field_name)
                            .map(|v| {
                                !v.is_null() && v.as_str().map(|s| !s.is_empty()).unwrap_or(true)
                            })
                            .unwrap_or(false);
                        if !has_value {
                            return Err(AppError::BadRequest(format!(
                                "Required field '{}' is missing",
                                field_name
                            )));
                        }
                    }
                }
            }
        }
    }

    if req.approved {
        // ── Agent ask_user approval: inject response into agent_state, mark ready ──
        if step.action_type == "agent" {
            let user_response = extract_agent_response(req.input.as_ref());

            // Get the agent conversation state
            if let Some(ref state_val) = step.agent_state {
                if let Ok(mut conv_state) = serde_json::from_value::<
                    stroem_agent::state::AgentConversationState,
                >(state_val.clone())
                {
                    // Get the ask_user tool_call_id
                    let tool_call_id = conv_state
                        .ask_user_call
                        .take()
                        .map(|a| a.tool_call_id)
                        .unwrap_or_default();

                    conv_state.suspended_for_ask_user = false;

                    // Store the user response as a resolved tool result
                    conv_state.resolved_tool_results.push(
                        stroem_agent::state::ResolvedToolResult {
                            tool_call_id,
                            result_text: user_response,
                        },
                    );

                    // Save updated state
                    let updated_state = serde_json::to_value(&conv_state)
                        .context("serialize agent conversation state")?;
                    JobStepRepo::update_agent_state(&state.pool, job_id, &step_name, updated_state)
                        .await
                        .context("save agent state after ask_user approval")?;

                    // Mark step ready for re-claim by a worker
                    sqlx::query(
                        "UPDATE job_step SET status = 'ready', ready_at = NOW(), worker_id = NULL \
                         WHERE job_id = $1 AND step_name = $2 AND status = 'suspended'",
                    )
                    .bind(job_id)
                    .bind(&step_name)
                    .execute(&state.pool)
                    .await
                    .context("mark agent step ready for re-claim")?;

                    state
                        .append_server_log(
                            job_id,
                            &format!(
                                "[agent] Step '{}' ask_user approved by {} — marked ready for re-claim",
                                step_name, approver
                            ),
                        )
                        .await;

                    return Ok(Json(json!({"status": "approved"})));
                }
            }

            // Fallback if agent_state is missing — treat like normal approval
            tracing::warn!(
                "Agent step '{}' missing agent_state during ask_user approval — falling through to normal approval",
                step_name
            );
        }

        // ── Normal approval gate handling ──────────────────────────────
        // FIX 5: merge approval metadata into existing output (preserves approval_message)
        let mut output = step.output.clone().unwrap_or(json!({}));
        if let Some(obj) = output.as_object_mut() {
            obj.insert("approved".to_string(), json!(true));
            obj.insert("approved_by".to_string(), json!(approver));
            obj.insert("approved_at".to_string(), json!(Utc::now().to_rfc3339()));
            if let Some(input) = req.input {
                obj.insert("input".to_string(), input);
            }
        }

        // FIX 1: atomic approve — only succeeds if step is still suspended
        let applied = JobStepRepo::approve_step(&state.pool, job_id, &step_name, Some(output))
            .await
            .context("approve suspended step")?;

        if !applied {
            return Err(AppError::Conflict(
                "Step is no longer in suspended state".to_string(),
            ));
        }

        state
            .append_server_log(
                job_id,
                &format!("[approval] Step '{}' approved by {}", step_name, approver),
            )
            .await;

        if let Err(e) =
            crate::job_recovery::orchestrate_after_step(&state, job_id, &step_name).await
        {
            tracing::error!(
                "Failed to orchestrate after approval of step '{}' in job {}: {:#}",
                step_name,
                job_id,
                e
            );
            state
                .append_server_log(
                    job_id,
                    &format!("[approval] Orchestration error after approval: {:#}", e),
                )
                .await;
        }

        Ok(Json(json!({"status": "approved"})))
    } else {
        // FIX 6: truncate rejection_reason at a UTF-8 char boundary to avoid panics
        let reason = req
            .rejection_reason
            .unwrap_or_else(|| "Approval rejected".to_string());
        let reason = if reason.len() > 4096 {
            let mut end = 4096;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason[..end].to_string()
        } else {
            reason
        };

        // FIX 1: atomic reject — only succeeds if step is still suspended
        let applied = JobStepRepo::reject_step(&state.pool, job_id, &step_name, &reason)
            .await
            .context("reject suspended step")?;

        if !applied {
            return Err(AppError::Conflict(
                "Step is no longer in suspended state".to_string(),
            ));
        }

        state
            .append_server_log(
                job_id,
                &format!(
                    "[approval] Step '{}' rejected by {}: {}",
                    step_name, approver, reason
                ),
            )
            .await;

        if let Err(e) =
            crate::job_recovery::orchestrate_after_step(&state, job_id, &step_name).await
        {
            tracing::error!(
                "Failed to orchestrate after rejection of step '{}' in job {}: {:#}",
                step_name,
                job_id,
                e
            );
            state
                .append_server_log(
                    job_id,
                    &format!("[approval] Orchestration error after rejection: {:#}", e),
                )
                .await;
        }

        Ok(Json(json!({"status": "rejected"})))
    }
}

/// Check ACL permission for a specific job's workspace/task.
///
/// Returns the user's permission level, or an `AppError` on failure.
/// Returns `Ok(TaskPermission::Run)` when ACL is not configured or auth is absent.
async fn check_job_acl(
    state: &AppState,
    auth_user: &Option<AuthUser>,
    workspace: &str,
    task_name: &str,
) -> Result<TaskPermission, AppError> {
    let auth = match auth_user {
        Some(a) => a,
        None => return Ok(TaskPermission::Run),
    };
    if !state.acl.is_configured() {
        return Ok(TaskPermission::Run);
    }
    let user_id = auth.user_id()?;
    let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
        .await
        .context("load ACL context")?;
    let folder = state
        .get_workspace(workspace)
        .await
        .and_then(|ws| ws.tasks.get(task_name).and_then(|t| t.folder.clone()));
    let task_path = make_task_path(folder.as_deref(), task_name);
    Ok(state
        .acl
        .evaluate(workspace, &task_path, &auth.claims.email, &groups, is_admin))
}

/// Build the ACL-filtered list of (workspace, task_name) pairs allowed for this user.
///
/// Returns:
/// - `Ok(None)` when no ACL filtering is needed (no auth user, ACL not configured, or admin)
/// - `Ok(Some(pairs))` when filtering is active
/// - `Err(AppError)` on failure (user_id parse error or DB error)
async fn resolve_acl_scope(
    state: &AppState,
    auth_user: &Option<AuthUser>,
) -> Result<Option<Vec<(String, String)>>, AppError> {
    let auth = match auth_user {
        Some(a) => a,
        None => return Ok(None),
    };
    if !state.acl.is_configured() {
        return Ok(None);
    }

    let user_id = auth.user_id()?;
    let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
        .await
        .context("load ACL context")?;

    // Collect all workspace tasks
    let mut all_tasks = Vec::new();
    for (ws_name, ws_config) in state.workspaces.get_all_configs().await {
        for (task_name, task_def) in &ws_config.tasks {
            all_tasks.push((ws_name.clone(), task_name.clone(), task_def.folder.clone()));
        }
    }

    match state
        .acl
        .allowed_scope(&all_tasks, &auth.claims.email, &groups, is_admin)
    {
        AllowedScope::All => Ok(None),
        AllowedScope::Filtered(items) => {
            let pairs = items
                .into_iter()
                .map(|(ws, task, _perm)| (ws, task))
                .collect();
            Ok(Some(pairs))
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
            revision: None,
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
            retry_of_job_id: None,
            retry_job_id: None,
            retry_attempt: 0,
            max_retries: None,
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
                timeout: None,
                when: None,
                for_each: None,
                sequential: false,
                retry: None,
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
                timeout: None,
                when: None,
                for_each: None,
                sequential: false,
                retry: None,
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
                timeout: None,
                when: None,
                for_each: None,
                sequential: false,
                retry: None,
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

    #[test]
    fn test_dashboard_stats_serialization() {
        // Verify DashboardStats serializes to the expected JSON shape consumed by
        // the frontend getStats() call.
        let stats = DashboardStats {
            pending: 3,
            running: 1,
            completed: 42,
            failed: 5,
            cancelled: 2,
            skipped: 7,
        };

        let value = serde_json::to_value(&stats).expect("serialization should succeed");
        assert_eq!(value["pending"], json!(3));
        assert_eq!(value["running"], json!(1));
        assert_eq!(value["completed"], json!(42));
        assert_eq!(value["failed"], json!(5));
        assert_eq!(value["cancelled"], json!(2));
        assert_eq!(value["skipped"], json!(7));

        // All six fields must be present — no extras, no missing keys.
        let obj = value.as_object().expect("must be an object");
        assert_eq!(obj.len(), 6);
    }

    #[test]
    fn test_dashboard_stats_defaults_to_zero_from_empty_map() {
        // Simulate what get_stats does when the DB returns an empty HashMap
        // (i.e. no jobs exist yet).
        let counts: HashMap<String, i64> = HashMap::new();
        let stats = DashboardStats {
            pending: *counts.get("pending").unwrap_or(&0),
            running: *counts.get("running").unwrap_or(&0),
            completed: *counts.get("completed").unwrap_or(&0),
            failed: *counts.get("failed").unwrap_or(&0),
            cancelled: *counts.get("cancelled").unwrap_or(&0),
            skipped: *counts.get("skipped").unwrap_or(&0),
        };
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.running, 0);
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.cancelled, 0);
        assert_eq!(stats.skipped, 0);
    }

    // --- Rejection-reason truncation (FIX 6) ---

    /// Truncation helper extracted from the approve_step handler for pure unit testing.
    fn truncate_reason(reason: String, max_bytes: usize) -> String {
        if reason.len() > max_bytes {
            let mut end = max_bytes;
            while !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason[..end].to_string()
        } else {
            reason
        }
    }

    #[test]
    fn test_truncate_reason_short_unchanged() {
        let reason = "Rejected".to_string();
        assert_eq!(truncate_reason(reason.clone(), 4096), reason);
    }

    #[test]
    fn test_truncate_reason_exactly_at_limit_unchanged() {
        let reason = "a".repeat(4096);
        assert_eq!(truncate_reason(reason.clone(), 4096), reason);
    }

    #[test]
    fn test_truncate_reason_ascii_long_is_truncated() {
        let reason = "a".repeat(5000);
        let result = truncate_reason(reason, 4096);
        assert_eq!(result.len(), 4096);
    }

    #[test]
    fn test_truncate_reason_multibyte_utf8_does_not_panic() {
        // Each '€' is 3 bytes in UTF-8.  4096 / 3 = 1365 full chars = 4095 bytes.
        // Adding one more '€' pushes len to 4098, which is NOT a char boundary at 4096.
        let reason = "€".repeat(1366); // 4098 bytes
        let result = truncate_reason(reason, 4096);
        // Must be valid UTF-8 and at most 4096 bytes
        assert!(result.len() <= 4096);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    // --- approval_message preservation in output (FIX 5) ---

    #[test]
    fn test_approval_output_preserves_existing_fields() {
        // Simulate the merge logic from approve_step
        let existing_output = json!({
            "approval_message": "Please review the deployment",
        });
        let mut output = existing_output;

        let approver = "alice@example.com";
        if let Some(obj) = output.as_object_mut() {
            obj.insert("approved".to_string(), json!(true));
            obj.insert("approved_by".to_string(), json!(approver));
            obj.insert("approved_at".to_string(), json!("2026-01-01T00:00:00Z"));
            let user_input = json!({"environment": "production"});
            obj.insert("input".to_string(), user_input);
        }

        // approval_message must survive the merge
        assert_eq!(output["approval_message"], "Please review the deployment");
        assert_eq!(output["approved"], true);
        assert_eq!(output["approved_by"], approver);
        assert_eq!(output["input"]["environment"], "production");
    }

    #[test]
    fn test_approval_output_starts_empty_when_no_prior_output() {
        // When step.output is None, we start from json!({}) and fill it.
        let mut output = json!({});
        let approver = "bob@example.com";
        if let Some(obj) = output.as_object_mut() {
            obj.insert("approved".to_string(), json!(true));
            obj.insert("approved_by".to_string(), json!(approver));
        }
        assert_eq!(output["approved"], true);
        assert_eq!(output["approved_by"], approver);
        // approval_message key should not exist (no prior message)
        assert!(output.get("approval_message").is_none());
    }

    // --- approval_fields surfaced for all approval-step statuses (FIX 7) ---

    #[test]
    fn test_approval_fields_surfaced_for_completed_approval_step() {
        // After FIX 7 the condition is `action_type == "approval"` (no status check).
        // Simulate the logic from get_job handler.
        let status = "completed";
        let action_type = "approval";
        let spec = json!({"input": {"environment": {"required": true}}});
        let output_val = json!({
            "approval_message": "Deploy?",
            "approved": true,
        });

        let mut step_json = json!({
            "action_type": action_type,
            "status": status,
            "output": output_val,
            "action_spec": spec,
        });

        // Apply the FIX 7 logic
        if step_json["action_type"] == "approval" {
            if let Some(msg) = step_json["output"]["approval_message"].as_str() {
                step_json["approval_message"] = json!(msg);
            }
            if let Some(input_schema) = step_json["action_spec"]["input"].as_object() {
                step_json["approval_fields"] = json!(input_schema);
            }
        }

        assert_eq!(step_json["approval_message"], "Deploy?");
        assert!(step_json.get("approval_fields").is_some());
    }

    #[test]
    fn test_approval_fields_surfaced_for_suspended_approval_step() {
        let status = "suspended";
        let action_type = "approval";
        let spec = json!({"input": {"ticket": {"required": true}}});
        let output_val = json!({"approval_message": "Please review"});

        let mut step_json = json!({
            "action_type": action_type,
            "status": status,
            "output": output_val,
            "action_spec": spec,
        });

        if step_json["action_type"] == "approval" {
            if let Some(msg) = step_json["output"]["approval_message"].as_str() {
                step_json["approval_message"] = json!(msg);
            }
            if let Some(input_schema) = step_json["action_spec"]["input"].as_object() {
                step_json["approval_fields"] = json!(input_schema);
            }
        }

        assert_eq!(step_json["approval_message"], "Please review");
        assert!(step_json["approval_fields"].get("ticket").is_some());
    }

    #[test]
    fn test_extract_agent_response_none() {
        assert_eq!(extract_agent_response(None), "Approved");
    }

    #[test]
    fn test_extract_agent_response_single_response_key() {
        let input = json!({"response": "hello world"});
        assert_eq!(extract_agent_response(Some(&input)), "hello world");
    }

    #[test]
    fn test_extract_agent_response_empty_string_falls_through() {
        let input = json!({"response": ""});
        // Empty response should fall through to JSON serialization, not send empty string to LLM
        assert_eq!(extract_agent_response(Some(&input)), r#"{"response":""}"#);
    }

    #[test]
    fn test_extract_agent_response_non_string_value() {
        let input = json!({"response": 42});
        assert_eq!(extract_agent_response(Some(&input)), r#"{"response":42}"#);
    }

    #[test]
    fn test_extract_agent_response_multiple_keys() {
        let input = json!({"response": "hi", "extra": "data"});
        let result = extract_agent_response(Some(&input));
        assert!(result.contains("response"));
        assert!(result.contains("extra"));
    }

    #[test]
    fn test_extract_agent_response_bare_string() {
        let input = json!("just a string");
        assert_eq!(extract_agent_response(Some(&input)), r#""just a string""#);
    }

    #[test]
    fn test_extract_agent_response_empty_object() {
        let input = json!({});
        assert_eq!(extract_agent_response(Some(&input)), "{}");
    }

    #[test]
    fn test_extract_agent_response_null_value() {
        let input = json!({"response": null});
        assert_eq!(extract_agent_response(Some(&input)), r#"{"response":null}"#);
    }
}
