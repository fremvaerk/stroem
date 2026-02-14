use crate::orchestrator;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::{FlowStep, TaskDef};
use stroem_common::template::{render_env_map, render_input_map, render_string_opt};
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
            tracing::error!("Failed to register worker: {}", e);
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
            tracing::error!("Failed to update heartbeat: {}", e);
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
            tracing::error!("Failed to claim job: {}", e);
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
            tracing::error!("Failed to get job: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get job: {}", e)})),
            )
                .into_response();
        }
    };

    // Render template input at claim time using completed step outputs
    let rendered_input = 'render: {
        // Get flow step definition from workspace to find raw template input
        let workspace = match state.get_workspace(&job.workspace).await {
            Some(w) => w,
            None => break 'render step.input.clone(),
        };
        let task = match workspace.tasks.get(&job.task_name) {
            Some(t) => t.clone(),
            None => break 'render step.input.clone(),
        };

        let flow_step = match task.flow.get(&step.step_name) {
            Some(fs) => fs.clone(),
            None => break 'render step.input.clone(),
        };

        // If step has no template input, return stored input as-is
        if flow_step.input.is_empty() {
            break 'render step.input.clone();
        }

        // Build template context: { "input": job.input, "step_name": { "output": ... }, ... }
        let mut context = serde_json::Map::new();
        if let Some(job_input) = &job.input {
            context.insert("input".to_string(), job_input.clone());
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
                tracing::warn!("Failed to render step input template: {}", e);
                step.input.clone()
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
        tracing::warn!("Failed to persist rendered input: {}", e);
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
        if let Some(ws_config) = state.get_workspace(&job.workspace).await {
            if !ws_config.secrets.is_empty() {
                let secrets_value = serde_json::to_value(&ws_config.secrets).unwrap_or_default();
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
                        tracing::warn!("Failed to render action_spec env templates: {}", e);
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
                        tracing::warn!("Failed to render action_spec cmd template: {}", e);
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
                        tracing::warn!("Failed to render action_spec script template: {}", e);
                    }
                }
            }
        }

        Some(serde_json::Value::Object(spec_obj))
    };

    Json(ClaimResponse {
        workspace: Some(job.workspace),
        job_id: Some(step.job_id.to_string()),
        step_name: Some(step.step_name),
        action_name: Some(step.action_name),
        action_type: Some(step.action_type),
        action_image: step.action_image,
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
                tracing::warn!("Failed to transition job to running: {}", e);
            }
            Json(json!({"status": "ok"})).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to mark step as running: {}", e);
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
            tracing::error!("Failed to mark step as failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to mark step as failed: {}", e)})),
            )
                .into_response();
        }
    } else if let Err(e) =
        JobStepRepo::mark_completed(&state.pool, job_id, &step_name, req.output).await
    {
        tracing::error!("Failed to mark step as completed: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to mark step as completed: {}", e)})),
        )
            .into_response();
    }

    // Get the job to retrieve task info
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
            tracing::error!("Failed to get job: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to get job: {}", e)})),
            )
                .into_response();
        }
    };

    // Get task definition from workspace
    let workspace = match state.get_workspace(&job.workspace).await {
        Some(w) => w,
        None => {
            tracing::error!("Workspace '{}' not found", job.workspace);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Workspace not found"})),
            )
                .into_response();
        }
    };
    let task = match workspace.tasks.get(&job.task_name) {
        Some(t) => t.clone(),
        None if job.source_type == "hook" => {
            // Hook jobs use synthetic task names (e.g. "_hook:notify") that don't
            // exist in the workspace. Build a minimal TaskDef so the orchestrator
            // can still detect terminal state and mark the job completed/failed.
            let steps = match JobStepRepo::get_steps_for_job(&state.pool, job_id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to get steps for hook job: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "Failed to get steps for hook job"})),
                    )
                        .into_response();
                }
            };
            let mut flow = HashMap::new();
            for step in &steps {
                flow.insert(
                    step.step_name.clone(),
                    FlowStep {
                        action: step.action_name.clone(),
                        depends_on: vec![],
                        input: HashMap::new(),
                        continue_on_failure: false,
                    },
                );
            }
            TaskDef {
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow,
                on_success: vec![],
                on_error: vec![],
            }
        }
        None => {
            tracing::error!("Task '{}' not found in workspace", job.task_name);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Task not found in workspace"})),
            )
                .into_response();
        }
    };

    // Trigger orchestrator to handle completion
    if let Err(e) = orchestrator::on_step_completed(&state.pool, job_id, &step_name, &task).await {
        tracing::error!("Orchestrator error: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Orchestrator error: {}", e)})),
        )
            .into_response();
    }

    // Handle any newly-promoted type: task steps in the current job
    if let Err(e) =
        crate::job_creator::handle_task_steps(&state.pool, &workspace, &job.workspace, job_id).await
    {
        tracing::error!("Failed to handle task steps for job {}: {}", job_id, e);
    }

    // Spawn background S3 upload if the job reached a terminal state, and fire hooks
    if let Ok(Some(job_after)) = JobRepo::get(&state.pool, job_id).await {
        if job_after.status == "completed" || job_after.status == "failed" {
            let log_storage = state.log_storage.clone();
            tokio::spawn(async move {
                if let Err(e) = log_storage.upload_to_s3(job_id).await {
                    tracing::warn!("Failed to upload logs to S3 for job {}: {}", job_id, e);
                }
            });

            // If this is a child job, propagate completion to the parent step
            if let (Some(parent_job_id), Some(ref parent_step)) =
                (job_after.parent_job_id, &job_after.parent_step_name)
            {
                if let Err(e) =
                    propagate_to_parent(&state, &job_after, parent_job_id, parent_step).await
                {
                    tracing::error!(
                        "Failed to propagate child job {} to parent {}: {}",
                        job_after.job_id,
                        parent_job_id,
                        e
                    );
                }
            }

            // Fire hooks (best-effort, inline since it only does DB writes)
            crate::hooks::fire_hooks(&state.pool, &workspace, &job_after, &task).await;
        }
    }

    Json(json!({"status": "ok"})).into_response()
}

/// Propagate a child job's terminal state to the parent step and orchestrate the parent.
async fn propagate_to_parent(
    state: &AppState,
    child_job: &stroem_db::JobRow,
    parent_job_id: Uuid,
    parent_step: &str,
) -> Result<(), anyhow::Error> {
    if child_job.status == "completed" {
        JobStepRepo::mark_completed(
            &state.pool,
            parent_job_id,
            parent_step,
            child_job.output.clone(),
        )
        .await?;
    } else {
        let err = format!("Child job {} failed", child_job.job_id);
        JobStepRepo::mark_failed(&state.pool, parent_job_id, parent_step, &err).await?;
    }

    // Get parent job info to orchestrate
    let parent_job = JobRepo::get(&state.pool, parent_job_id).await?;
    if let Some(ref parent_job) = parent_job {
        if let Some(parent_ws) = state.get_workspace(&parent_job.workspace).await {
            // Build task def for parent — may be a real task or a hook
            let parent_task = if let Some(t) = parent_ws.tasks.get(&parent_job.task_name) {
                t.clone()
            } else if parent_job.source_type == "hook" {
                // Hook jobs use synthetic names; build minimal TaskDef
                let steps = JobStepRepo::get_steps_for_job(&state.pool, parent_job_id).await?;
                let mut flow = HashMap::new();
                for step in &steps {
                    flow.insert(
                        step.step_name.clone(),
                        FlowStep {
                            action: step.action_name.clone(),
                            depends_on: vec![],
                            input: HashMap::new(),
                            continue_on_failure: false,
                        },
                    );
                }
                TaskDef {
                    mode: "distributed".to_string(),
                    folder: None,
                    input: HashMap::new(),
                    flow,
                    on_success: vec![],
                    on_error: vec![],
                }
            } else {
                tracing::warn!(
                    "Parent task '{}' not found in workspace '{}'",
                    parent_job.task_name,
                    parent_job.workspace
                );
                return Ok(());
            };

            // Run orchestrator for parent job
            orchestrator::on_step_completed(&state.pool, parent_job_id, parent_step, &parent_task)
                .await?;

            // Handle any newly-promoted task steps in the parent
            crate::job_creator::handle_task_steps(
                &state.pool,
                &parent_ws,
                &parent_job.workspace,
                parent_job_id,
            )
            .await?;

            // Check if parent job is now terminal — propagate recursively
            if let Ok(Some(parent_after)) = JobRepo::get(&state.pool, parent_job_id).await {
                if parent_after.status == "completed" || parent_after.status == "failed" {
                    // S3 upload for parent
                    let log_storage = state.log_storage.clone();
                    let pjid = parent_job_id;
                    tokio::spawn(async move {
                        if let Err(e) = log_storage.upload_to_s3(pjid).await {
                            tracing::warn!(
                                "Failed to upload logs to S3 for parent job {}: {}",
                                pjid,
                                e
                            );
                        }
                    });

                    // Propagate up the chain if parent is also a child
                    if let (Some(grandparent_id), Some(ref grandparent_step)) =
                        (parent_after.parent_job_id, &parent_after.parent_step_name)
                    {
                        Box::pin(propagate_to_parent(
                            state,
                            &parent_after,
                            grandparent_id,
                            grandparent_step,
                        ))
                        .await?;
                    }

                    // Fire hooks for the parent
                    crate::hooks::fire_hooks(&state.pool, &parent_ws, &parent_after, &parent_task)
                        .await;
                }
            }
        }
    }

    Ok(())
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
            serde_json::json!({
                "ts": entry.ts,
                "stream": entry.stream,
                "step": step,
                "line": entry.line,
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    match state.log_storage.append_log(job_id, &jsonl_chunk).await {
        Ok(_) => {
            // Update log path in database (idempotent)
            let log_path = state.log_storage.get_log_path(job_id);
            if let Err(e) = JobRepo::set_log_path(&state.pool, job_id, &log_path).await {
                tracing::warn!("Failed to update log path: {}", e);
            }

            // Broadcast to WebSocket subscribers
            state
                .log_broadcast
                .broadcast(job_id, jsonl_chunk.clone())
                .await;

            Json(json!({"status": "ok"})).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to append log: {}", e);
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
            // Spawn background S3 upload for the completed job
            let log_storage = state.log_storage.clone();
            tokio::spawn(async move {
                if let Err(e) = log_storage.upload_to_s3(job_id).await {
                    tracing::warn!("Failed to upload logs to S3 for job {}: {}", job_id, e);
                }
            });

            // Fire hooks and propagate to parent for the completed job
            if let Ok(Some(job)) = JobRepo::get(&state.pool, job_id).await {
                // If this is a child job, propagate completion to the parent step
                if let (Some(parent_job_id), Some(ref parent_step)) =
                    (job.parent_job_id, &job.parent_step_name)
                {
                    if let Err(e) =
                        propagate_to_parent(&state, &job, parent_job_id, parent_step).await
                    {
                        tracing::error!(
                            "Failed to propagate child job {} to parent {}: {}",
                            job.job_id,
                            parent_job_id,
                            e
                        );
                    }
                }

                if let Some(ws_config) = state.get_workspace(&job.workspace).await {
                    if let Some(task) = ws_config.tasks.get(&job.task_name) {
                        crate::hooks::fire_hooks(&state.pool, &ws_config, &job, task).await;
                    }
                }
            }

            Json(json!({"status": "ok"})).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to mark job as completed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to mark job as completed: {}", e)})),
            )
                .into_response()
        }
    }
}
