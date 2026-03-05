use super::rendering;
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
use stroem_common::models::job::StepStatus;
use stroem_db::{JobRepo, JobStepRepo, WorkerRepo};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub capabilities: Vec<String>,
    pub tags: Option<Vec<String>>,
    pub version: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<i32>,
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
        req.version.as_deref(),
    )
    .await
    {
        Ok(_) => {
            match &req.version {
                Some(v) => tracing::info!("Registered worker: {} ({}) v{}", req.name, worker_id, v),
                None => tracing::info!("Registered worker: {} ({})", req.name, worker_id),
            }
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

/// Fail a step that was claimed but couldn't be rendered.
/// Marks the step as failed, logs the error, and triggers orchestration.
async fn fail_claimed_step(
    state: &Arc<AppState>,
    job_id: Uuid,
    step_name: &str,
    error_msg: &str,
) -> axum::response::Response {
    tracing::error!(
        job_id = %job_id,
        step_name = %step_name,
        "Template rendering failed at claim time: {}",
        error_msg
    );
    state.append_server_log(job_id, error_msg).await;

    if let Err(e) = JobStepRepo::mark_failed(&state.pool, job_id, step_name, error_msg).await {
        tracing::error!("Failed to mark step as failed after render error: {:#}", e);
    } else {
        // Trigger orchestration so the job can progress (fail/skip downstream steps)
        if let Err(e) = crate::job_recovery::orchestrate_after_step(state, job_id, step_name).await
        {
            let orch_msg = format!("Failed to orchestrate after render failure: {:#}", e);
            tracing::error!("{}", orch_msg);
            state.append_server_log(job_id, &orch_msg).await;
        }
    }

    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"error": error_msg})),
    )
        .into_response()
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
                timeout_secs: None,
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

    // Fetch completed step outputs for template rendering context
    let completed_steps: Vec<(String, Option<serde_json::Value>)> =
        match JobStepRepo::get_steps_for_job(&state.pool, step.job_id).await {
            Ok(all_steps) => all_steps
                .into_iter()
                .filter(|s| s.status == StepStatus::Completed.as_ref())
                .map(|s| (s.step_name, s.output))
                .collect(),
            Err(_) => vec![],
        };

    // Render step input and apply action defaults
    let rendered_input = if let Some(ref workspace) = ws_config {
        let ctx = rendering::RenderContext {
            workspace,
            task_name: &job.task_name,
            step: &step,
            job_input: job.input.as_ref(),
            completed_steps: &completed_steps,
        };

        let raw_input = match rendering::render_step_input(&ctx) {
            Ok(input) => input,
            Err(e) => {
                let msg = format!("Failed to render step input template: {:#}", e);
                return fail_claimed_step(&state, step.job_id, &step.step_name, &msg).await;
            }
        };

        match rendering::prepare_step_action_input(raw_input, &ctx) {
            Ok(input) => input,
            Err(e) => {
                let msg = format!("{:#}", e);
                return fail_claimed_step(&state, step.job_id, &step.step_name, &msg).await;
            }
        }
    } else {
        step.input.clone()
    };

    // Build secrets value for action_spec and image rendering
    let secrets_value = ws_config
        .as_ref()
        .and_then(|w| {
            if w.secrets.is_empty() {
                None
            } else {
                serde_json::to_value(&w.secrets).ok()
            }
        })
        .unwrap_or_else(|| serde_json::json!({}));

    // Render action_spec env/cmd/script/manifest templates
    let rendered_action_spec = match rendering::render_action_spec(
        step.action_spec.as_ref(),
        rendered_input.as_ref(),
        &secrets_value,
    ) {
        Ok(spec) => spec,
        Err(e) => {
            let msg = format!("{:#}", e);
            return fail_claimed_step(&state, step.job_id, &step.step_name, &msg).await;
        }
    };

    // Render action_image templates (e.g. {{ input.image_tag }})
    let rendered_image = match rendering::render_image(
        step.action_image.as_deref(),
        rendered_input.as_ref(),
        &secrets_value,
    ) {
        Ok(img) => img,
        Err(e) => {
            let msg = format!("{:#}", e);
            return fail_claimed_step(&state, step.job_id, &step.step_name, &msg).await;
        }
    };

    // Persist rendered input to DB so the job detail API can return it.
    // Done after all rendering succeeds — avoids writing rendered input for steps that fail.
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
        timeout_secs: step.timeout_secs,
    })
    .into_response()
}

/// POST /worker/jobs/:id/steps/:step/start - Mark step as running
#[tracing::instrument(skip(state))]
pub async fn start_step(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(Uuid, String)>,
    Json(req): Json<StartStepRequest>,
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
    Path((job_id, step_name)): Path<(Uuid, String)>,
    Json(req): Json<CompleteStepRequest>,
) -> impl IntoResponse {
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
    Path(job_id): Path<Uuid>,
    Json(req): Json<AppendLogRequest>,
) -> impl IntoResponse {
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

/// GET /worker/jobs/:id/cancelled - Check if a job has been cancelled
#[tracing::instrument(skip(state))]
pub async fn check_cancelled(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<Uuid>,
) -> impl IntoResponse {
    let cancelled = crate::cancellation::is_cancelled(&state, job_id);
    Json(json!({"cancelled": cancelled})).into_response()
}

/// POST /worker/jobs/:id/complete - Mark job as completed (for local mode)
#[tracing::instrument(skip(state))]
pub async fn complete_job(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<Uuid>,
    Json(req): Json<CompleteJobRequest>,
) -> impl IntoResponse {
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
            timeout_secs: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["task_name"], "deploy-api");
    }

    #[test]
    fn test_register_request_deserializes_version() {
        let json = serde_json::json!({
            "name": "worker-1",
            "capabilities": ["shell"],
            "version": "0.5.9"
        });
        let req: RegisterRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.version.as_deref(), Some("0.5.9"));
    }

    #[test]
    fn test_register_request_version_absent_is_none() {
        let json = serde_json::json!({
            "name": "worker-1",
            "capabilities": ["shell"]
        });
        let req: RegisterRequest = serde_json::from_value(json).unwrap();
        assert!(req.version.is_none());
    }

    #[test]
    fn test_register_request_version_null_is_none() {
        let json = serde_json::json!({
            "name": "worker-1",
            "capabilities": ["shell"],
            "version": null
        });
        let req: RegisterRequest = serde_json::from_value(json).unwrap();
        assert!(req.version.is_none());
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
            timeout_secs: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["task_name"].is_null());
    }
}
