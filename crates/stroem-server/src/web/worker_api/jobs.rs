use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use stroem_common::template::{
    prepare_action_input, render_env_map, render_input_map, render_json_strings, render_string_opt,
};
use stroem_db::{JobRepo, JobStepRepo, WorkerRepo};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub capabilities: Vec<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub worker_id: String,
}

#[derive(Debug, Deserialize)]
pub struct HeartbeatRequest {
    pub worker_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ClaimRequest {
    pub worker_id: String,
    pub capabilities: Vec<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct ClaimResponse {
    pub workspace: Option<String>,
    pub job_id: Option<String>,
    pub task_name: Option<String>,
    pub step_name: Option<String>,
    pub action_name: Option<String>,
    pub action_type: Option<String>,
    pub action_image: Option<String>,
    pub action_spec: Option<serde_json::Value>,
    pub input: Option<serde_json::Value>,
    pub runner: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StartStepRequest {
    pub worker_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CompleteStepRequest {
    pub output: Option<serde_json::Value>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LogLineEntry {
    pub ts: String,
    pub stream: String,
    pub line: String,
}

#[derive(Debug, Deserialize)]
pub struct AppendLogRequest {
    pub lines: Vec<LogLineEntry>,
    pub step_name: Option<String>,
}

#[derive(Serialize)]
struct LogEntry<'a> {
    ts: &'a str,
    stream: &'a str,
    step: &'a str,
    line: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct CompleteJobRequest {
    pub output: Option<serde_json::Value>,
}

/// POST /worker/register - Register a worker
#[tracing::instrument(skip(state))]
pub async fn register_worker(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> impl IntoResponse {
    let worker_id = Uuid::new_v4();

    let effective_tags = req.tags.as_deref().unwrap_or(&req.capabilities);
    match WorkerRepo::register(
        &state.pool,
        worker_id,
        &req.name,
        &req.capabilities,
        effective_tags,
    )
    .await
    {
        Ok(_) => {
            tracing::info!("Registered worker: {} ({})", req.name, worker_id);
            Json(RegisterResponse {
                worker_id: worker_id.to_string(),
            })
            .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to register worker: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to register worker: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /worker/heartbeat - Update worker heartbeat
#[tracing::instrument(skip(state))]
pub async fn heartbeat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<HeartbeatRequest>,
) -> impl IntoResponse {
    let worker_id = match Uuid::parse_str(&req.worker_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid worker ID"})),
            )
                .into_response()
        }
    };

    match WorkerRepo::heartbeat(&state.pool, worker_id).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(e) => {
            tracing::error!("Failed to update heartbeat: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to update heartbeat: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /worker/jobs/claim - Claim next ready step
#[tracing::instrument(skip(state))]
pub async fn claim_job(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ClaimRequest>,
) -> impl IntoResponse {
    let worker_id = match Uuid::parse_str(&req.worker_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid worker ID"})),
            )
                .into_response()
        }
    };

    let effective_tags = req.tags.as_deref().unwrap_or(&req.capabilities);
    let step = match JobStepRepo::claim_ready_step(&state.pool, effective_tags, worker_id).await {
        Ok(Some(step)) => step,
        Ok(None) => {
            return Json(ClaimResponse {
                workspace: None,
                job_id: None,
                task_name: None,
                step_name: None,
                action_name: None,
                action_type: None,
                action_image: None,
                action_spec: None,
                input: None,
                runner: None,
            })
            .into_response();
        }
        Err(e) => {
            tracing::error!("Failed to claim job: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to claim job: {}", e)})),
            )
                .into_response();
        }
    };

    tracing::info!(
        "Worker {} claimed step {} from job {}",
        worker_id,
        step.step_name,
        step.job_id
    );

    // Get the job to access task_name, workspace, and job input
    let job = match JobRepo::get(&state.pool, step.job_id).await {
        Ok(Some(j)) => j,
        Ok(None) => {
            tracing::error!("Job {} not found", step.job_id);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Job not found"})),
            )
                .into_response();
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

    // Fetch workspace config once — used for template rendering, secrets, and action defaults
    let ws_config = state.get_workspace(&job.workspace).await;

    // Render template input at claim time using completed step outputs
    let rendered_input = 'render: {
        // Get flow step definition from workspace to find raw template input
        let workspace = match ws_config.as_ref() {
            Some(w) => w,
            None => break 'render step.input.clone(),
        };
        let task = match workspace.tasks.get(&job.task_name) {
            Some(t) => t,
            None => break 'render step.input.clone(),
        };

        let flow_step = match task.flow.get(&step.step_name) {
            Some(fs) => fs,
            None => break 'render step.input.clone(),
        };

        // If step has no template input, return stored input as-is
        if flow_step.input.is_empty() {
            break 'render step.input.clone();
        }

        // Build template context: { "input": job.input, "secret": ..., "step_name": { "output": ... }, ... }
        let mut context = serde_json::Map::new();
        if let Some(job_input) = &job.input {
            context.insert("input".to_string(), job_input.clone());
        }

        // Add workspace secrets
        if !workspace.secrets.is_empty() {
            if let Ok(secrets_value) = serde_json::to_value(&workspace.secrets) {
                context.insert("secret".to_string(), secrets_value);
            }
        }

        // Add completed step outputs to context
        // Step names are sanitized (hyphens → underscores) so Tera can resolve
        // dotted paths like {{ step_name.output.key }}
        if let Ok(all_steps) = JobStepRepo::get_steps_for_job(&state.pool, step.job_id).await {
            for s in &all_steps {
                if s.status == "completed" {
                    let mut step_ctx = serde_json::Map::new();
                    if let Some(output) = &s.output {
                        step_ctx.insert("output".to_string(), output.clone());
                    }
                    let safe_name = s.step_name.replace('-', "_");
                    context.insert(safe_name, serde_json::Value::Object(step_ctx));
                }
            }
        }

        let context_value = serde_json::Value::Object(context);

        match render_input_map(&flow_step.input, &context_value) {
            Ok(rendered) => Some(rendered),
            Err(e) => {
                tracing::warn!("Failed to render step input template: {:#}", e);
                step.input.clone()
            }
        }
    };

    // Merge action-level input defaults (e.g. object defaults with secret templates)
    let rendered_input = 'action_defaults: {
        let workspace = match ws_config.as_ref() {
            Some(w) => w,
            None => break 'action_defaults rendered_input,
        };
        let task = match workspace.tasks.get(&job.task_name) {
            Some(t) => t,
            None => break 'action_defaults rendered_input,
        };
        let flow_step = match task.flow.get(&step.step_name) {
            Some(fs) => fs,
            None => break 'action_defaults rendered_input,
        };
        let action = match workspace.actions.get(&flow_step.action) {
            Some(a) => a,
            None => break 'action_defaults rendered_input,
        };
        if action.input.is_empty() {
            break 'action_defaults rendered_input;
        }

        let mut input_val = rendered_input.unwrap_or_else(|| serde_json::json!({}));

        // Merge missing fields from job input that match the action's input schema.
        // This handles the case where a flow step doesn't explicitly map a field
        // (e.g. a connection input), but the job-level input has it resolved.
        merge_missing_action_fields(&mut input_val, job.input.as_ref(), action.input.keys());

        match prepare_action_input(&input_val, &action.input, workspace) {
            Ok(prepared) => Some(prepared),
            Err(e) => {
                tracing::warn!("Failed to prepare action input: {:#}", e);
                Some(input_val)
            }
        }
    };

    // Persist rendered input to DB so the job detail API can return it
    if let Err(e) = JobStepRepo::update_input(
        &state.pool,
        step.job_id,
        &step.step_name,
        rendered_input.clone(),
    )
    .await
    {
        tracing::warn!("Failed to persist rendered input: {:#}", e);
    }

    // Render action_spec env/cmd/script templates at claim time
    let rendered_action_spec = 'render_spec: {
        let original_spec = match &step.action_spec {
            Some(spec) => spec.clone(),
            None => break 'render_spec step.action_spec.clone(),
        };

        // Build rendering context with rendered input + secrets
        let mut spec_context = serde_json::Map::new();
        if let Some(ref input_val) = rendered_input {
            spec_context.insert("input".to_string(), input_val.clone());
        }

        // Add secrets from workspace
        if let Some(ref workspace) = ws_config {
            if !workspace.secrets.is_empty() {
                let secrets_value = serde_json::to_value(&workspace.secrets).unwrap_or_default();
                spec_context.insert("secret".to_string(), secrets_value);
            }
        }

        let context_value = serde_json::Value::Object(spec_context);

        // Render env values if present
        let mut spec_obj = match original_spec.as_object() {
            Some(obj) => obj.clone(),
            None => break 'render_spec Some(original_spec),
        };

        if let Some(env_val) = spec_obj.get("env") {
            if let Some(env_obj) = env_val.as_object() {
                let env_map: std::collections::HashMap<String, String> = env_obj
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect();
                match render_env_map(&env_map, &context_value) {
                    Ok(rendered_env) => {
                        let rendered_env_value: serde_json::Map<String, serde_json::Value> =
                            rendered_env
                                .into_iter()
                                .map(|(k, v)| (k, serde_json::Value::String(v)))
                                .collect();
                        spec_obj.insert(
                            "env".to_string(),
                            serde_json::Value::Object(rendered_env_value),
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to render action_spec env templates: {:#}", e);
                    }
                }
            }
        }

        // Render cmd if present
        if let Some(cmd_val) = spec_obj.get("cmd") {
            if let Some(cmd_str) = cmd_val.as_str() {
                let cmd_opt = Some(cmd_str.to_string());
                match render_string_opt(&cmd_opt, &context_value) {
                    Ok(Some(rendered_cmd)) => {
                        spec_obj.insert("cmd".to_string(), serde_json::Value::String(rendered_cmd));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("Failed to render action_spec cmd template: {:#}", e);
                    }
                }
            }
        }

        // Render script if present
        if let Some(script_val) = spec_obj.get("script") {
            if let Some(script_str) = script_val.as_str() {
                let script_opt = Some(script_str.to_string());
                match render_string_opt(&script_opt, &context_value) {
                    Ok(Some(rendered_script)) => {
                        spec_obj.insert(
                            "script".to_string(),
                            serde_json::Value::String(rendered_script),
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("Failed to render action_spec script template: {:#}", e);
                    }
                }
            }
        }

        // Render manifest string values (e.g. serviceAccountName from input)
        if let Some(manifest_val) = spec_obj.get("manifest") {
            match render_json_strings(manifest_val, &context_value) {
                Ok(rendered_manifest) => {
                    spec_obj.insert("manifest".to_string(), rendered_manifest);
                }
                Err(e) => {
                    tracing::warn!("Failed to render action_spec manifest templates: {:#}", e);
                }
            }
        }

        Some(serde_json::Value::Object(spec_obj))
    };

    // Render action_image templates (e.g. {{ input.image_tag }})
    let rendered_image = 'render_image: {
        let Some(ref image_str) = step.action_image else {
            break 'render_image step.action_image.clone();
        };
        if !image_str.contains("{{") {
            break 'render_image step.action_image.clone();
        }
        // Build context (same as spec rendering above)
        let mut img_context = serde_json::Map::new();
        if let Some(ref input_val) = rendered_input {
            img_context.insert("input".to_string(), input_val.clone());
        }
        if let Some(ref workspace) = ws_config {
            if !workspace.secrets.is_empty() {
                let secrets_value = serde_json::to_value(&workspace.secrets).unwrap_or_default();
                img_context.insert("secret".to_string(), secrets_value);
            }
        }
        let context_value = serde_json::Value::Object(img_context);
        let img_opt = Some(image_str.clone());
        match render_string_opt(&img_opt, &context_value) {
            Ok(rendered) => rendered,
            Err(e) => {
                tracing::warn!("Failed to render action_image template: {:#}", e);
                step.action_image.clone()
            }
        }
    };

    Json(ClaimResponse {
        workspace: Some(job.workspace),
        job_id: Some(step.job_id.to_string()),
        task_name: Some(job.task_name),
        step_name: Some(step.step_name),
        action_name: Some(step.action_name),
        action_type: Some(step.action_type),
        action_image: rendered_image,
        action_spec: rendered_action_spec,
        input: rendered_input,
        runner: Some(step.runner),
    })
    .into_response()
}

/// POST /worker/jobs/:id/steps/:step/start - Mark step as running
#[tracing::instrument(skip(state))]
pub async fn start_step(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(String, String)>,
    Json(req): Json<StartStepRequest>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&job_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid job ID"})),
            )
                .into_response()
        }
    };

    let worker_id = match Uuid::parse_str(&req.worker_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid worker ID"})),
            )
                .into_response()
        }
    };

    match JobStepRepo::mark_running(&state.pool, job_id, &step_name, worker_id).await {
        Ok(_) => {
            // Also transition the job itself to running (idempotent — no-op if already running)
            if let Err(e) = JobRepo::mark_running_if_pending(&state.pool, job_id, worker_id).await {
                tracing::warn!("Failed to transition job to running: {:#}", e);
            }
            Json(json!({"status": "ok"})).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to mark step as running: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to mark step as running: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /worker/jobs/:id/steps/:step/complete - Mark step as completed
#[tracing::instrument(skip(state))]
pub async fn complete_step(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(String, String)>,
    Json(req): Json<CompleteStepRequest>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&job_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid job ID"})),
            )
                .into_response()
        }
    };

    // Determine if this step failed based on exit_code or error
    let step_failed = req.exit_code.unwrap_or(0) != 0 || req.error.is_some();

    if step_failed {
        let error_msg = req
            .error
            .unwrap_or_else(|| format!("Process exited with code {}", req.exit_code.unwrap_or(1)));
        if let Err(e) = JobStepRepo::mark_failed(&state.pool, job_id, &step_name, &error_msg).await
        {
            tracing::error!("Failed to mark step as failed: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to mark step as failed: {}", e)})),
            )
                .into_response();
        }
    } else if let Err(e) =
        JobStepRepo::mark_completed(&state.pool, job_id, &step_name, req.output).await
    {
        tracing::error!("Failed to mark step as completed: {:#}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to mark step as completed: {}", e)})),
        )
            .into_response();
    }

    // Orchestrate: promote steps, skip unreachable, propagate to parent, fire hooks
    if let Err(e) = crate::job_recovery::orchestrate_after_step(&state, job_id, &step_name).await {
        tracing::error!("Orchestration error after step completion: {:#}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Orchestrator error: {}", e)})),
        )
            .into_response();
    }

    Json(json!({"status": "ok"})).into_response()
}

/// POST /worker/jobs/:id/logs - Append log chunk
#[tracing::instrument(skip(state))]
pub async fn append_log(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<String>,
    Json(req): Json<AppendLogRequest>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&job_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid job ID"})),
            )
                .into_response()
        }
    };

    // Convert structured log lines to JSONL with step field
    let step = req.step_name.as_deref().unwrap_or("");
    let jsonl_chunk: String = req
        .lines
        .iter()
        .map(|entry| {
            serde_json::to_string(&LogEntry {
                ts: &entry.ts,
                stream: &entry.stream,
                step,
                line: &entry.line,
            })
            .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    match state.log_storage.append_log(job_id, &jsonl_chunk).await {
        Ok(_) => {
            // Update log path in database (idempotent)
            let log_path = state.log_storage.get_log_path(job_id);
            if let Err(e) = JobRepo::set_log_path(&state.pool, job_id, &log_path).await {
                tracing::warn!("Failed to update log path: {:#}", e);
            }

            // Broadcast to WebSocket subscribers
            state
                .log_broadcast
                .broadcast(job_id, jsonl_chunk.clone())
                .await;

            Json(json!({"status": "ok"})).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to append log: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to append log: {}", e)})),
            )
                .into_response()
        }
    }
}

/// POST /worker/jobs/:id/complete - Mark job as completed (for local mode)
#[tracing::instrument(skip(state))]
pub async fn complete_job(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<String>,
    Json(req): Json<CompleteJobRequest>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&job_id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid job ID"})),
            )
                .into_response()
        }
    };

    match JobRepo::mark_completed(&state.pool, job_id, req.output).await {
        Ok(_) => {
            // Handle terminal state: S3 upload, parent propagation, hooks
            if let Err(e) = crate::job_recovery::handle_job_terminal(&state, job_id).await {
                tracing::error!("Failed to handle job terminal state: {:#}", e);
            }

            Json(json!({"status": "ok"})).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to mark job as completed: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to mark job as completed: {}", e)})),
            )
                .into_response()
        }
    }
}

/// Merge missing fields from job-level input into step input for fields declared
/// in the action's input schema. Step-level values always take precedence.
/// Null values in job input are skipped to avoid breaking downstream resolution.
fn merge_missing_action_fields<'a>(
    input_val: &mut serde_json::Value,
    job_input: Option<&serde_json::Value>,
    action_field_names: impl Iterator<Item = &'a String>,
) {
    let (Some(job_input), Some(input_obj)) = (job_input, input_val.as_object_mut()) else {
        return;
    };
    let Some(job_map) = job_input.as_object() else {
        return;
    };
    for field_name in action_field_names {
        if !input_obj.contains_key(field_name) {
            if let Some(val) = job_map.get(field_name) {
                if !val.is_null() {
                    input_obj.insert(field_name.clone(), val.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claim_response_serializes_task_name() {
        let resp = ClaimResponse {
            workspace: Some("default".to_string()),
            job_id: Some("abc-123".to_string()),
            task_name: Some("deploy-api".to_string()),
            step_name: Some("build".to_string()),
            action_name: Some("shell".to_string()),
            action_type: Some("shell".to_string()),
            action_image: None,
            action_spec: None,
            input: None,
            runner: Some("local".to_string()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["task_name"], "deploy-api");
    }

    // Helper: create field names vec for merge_missing_action_fields
    fn field_names(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_merge_missing_action_fields_from_job_input() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({
            "sql": "SELECT 1",
            "clickhouse": {
                "host": "ch.example.com",
                "port": 9000,
                "database": "analytics"
            }
        });
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert_eq!(input_val["clickhouse"]["host"], "ch.example.com");
        assert_eq!(input_val["clickhouse"]["port"], 9000);
        assert_eq!(input_val["clickhouse"]["database"], "analytics");
    }

    #[test]
    fn test_merge_step_input_takes_precedence() {
        let mut input_val = json!({"sql": "SELECT 2", "clickhouse": "step-override"});
        let job_input = json!({
            "sql": "SELECT 1",
            "clickhouse": {"host": "ch.example.com"}
        });
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 2");
        assert_eq!(input_val["clickhouse"], "step-override");
    }

    #[test]
    fn test_merge_skipped_when_job_input_is_none() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, None, names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert!(input_val.get("clickhouse").is_none());
    }

    #[test]
    fn test_merge_skipped_when_job_input_is_not_object() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!("some raw string");
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert!(input_val.get("clickhouse").is_none());
    }

    #[test]
    fn test_merge_skipped_when_input_val_is_not_object() {
        let mut input_val = json!("raw");
        let job_input = json!({"clickhouse": {"host": "h"}});
        let names = field_names(&["clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val, json!("raw"));
    }

    #[test]
    fn test_merge_skips_fields_not_in_job_input() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({"sql": "SELECT 1"});
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert!(input_val.get("clickhouse").is_none());
    }

    #[test]
    fn test_merge_skips_null_job_input_fields() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({"sql": "SELECT 1", "clickhouse": null});
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert!(input_val.get("clickhouse").is_none());
    }

    #[test]
    fn test_merge_multiple_missing_fields_all_filled() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({
            "sql": "SELECT 1",
            "clickhouse": {"host": "ch.example.com"},
            "s3_bucket": "my-bucket",
            "redis": {"url": "redis://localhost"}
        });
        let names = field_names(&["sql", "clickhouse", "s3_bucket", "redis"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert_eq!(input_val["clickhouse"]["host"], "ch.example.com");
        assert_eq!(input_val["s3_bucket"], "my-bucket");
        assert_eq!(input_val["redis"]["url"], "redis://localhost");
    }

    #[test]
    fn test_merge_ignores_job_fields_not_in_action_schema() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({
            "sql": "SELECT 1",
            "clickhouse": {"host": "ch.example.com"},
            "extra_field": "should not appear"
        });
        // Action schema only declares "sql" and "clickhouse"
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert_eq!(input_val["clickhouse"]["host"], "ch.example.com");
        assert!(input_val.get("extra_field").is_none());
    }

    #[test]
    fn test_claim_response_empty_serializes_null_task_name() {
        let resp = ClaimResponse {
            workspace: None,
            job_id: None,
            task_name: None,
            step_name: None,
            action_name: None,
            action_type: None,
            action_image: None,
            action_spec: None,
            input: None,
            runner: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["task_name"].is_null());
    }
}
