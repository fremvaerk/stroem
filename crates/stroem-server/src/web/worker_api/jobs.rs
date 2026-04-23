use super::rendering;
use crate::state::AppState;
use crate::web::error::AppError;
use anyhow::Context;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use stroem_common::models::job::StepStatus;
use stroem_db::{JobRepo, JobStepRepo, TaskStateRepo, WorkerRepo, WorkspaceStateRepo};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub tags: Vec<String>,
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
    pub tags: Vec<String>,
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
    /// Workspace revision (git SHA or folder hash) at the time this step was
    /// created. Workers use this to request the exact tarball that matches the
    /// job, avoiding workspace-update races. Absent when there is no work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,

    // ── Agent step fields ──────────────────────────────────────────
    /// Provider name for agent steps (worker looks up full config locally).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_provider_name: Option<String>,
    /// Rendered prompt for agent steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_prompt: Option<String>,
    /// Rendered system prompt for agent steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_system_prompt: Option<String>,
    /// MCP server definitions from workspace config (for agent steps).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers:
        Option<std::collections::HashMap<String, stroem_common::models::workflow::McpServerDef>>,
    /// Saved conversation state for resuming multi-turn agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_state: Option<serde_json::Value>,
    /// Task tool schemas — task name → {description, input schema}.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_tool_tasks: Option<serde_json::Value>,

    // ── Event source step fields ───────────────────────────────────
    /// Present when this step belongs to an event source consumer job.
    /// Contains `target_task`, `source_id`, `input_defaults`, `max_in_flight`,
    /// and `env`. The worker uses this to activate stdout JSON-line interception
    /// and reads env directly from `event_source_config.env`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_source_config: Option<serde_json::Value>,

    // ── Task state fields ──────────────────────────────────────────
    /// Storage key for the most recent task state snapshot (if any).
    /// Workers download this via `GET /worker/state/{ws}/{task}` to restore
    /// the `/state` directory before running the step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_storage_key: Option<String>,
    /// Whether the previous state snapshot contains a structured `state.json`
    /// sidecar. When `true`, the contents are also available as `{{ state.* }}`
    /// in Tera templates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_has_json: Option<bool>,

    // ── Global workspace state fields ──────────────────────────────
    /// Storage key for the most recent global workspace state snapshot (if any).
    /// Workers download this via `GET /worker/global-state/{ws}` to restore
    /// the `/global-state` directory before running the step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_state_storage_key: Option<String>,
    /// Whether the previous global state snapshot contains a structured `state.json`
    /// sidecar. When `true`, the contents are also available as `{{ global_state.* }}`
    /// in Tera templates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_state_has_json: Option<bool>,
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

/// POST /worker/jobs/:id/steps/:step/task-tool — Worker requests child job creation
#[derive(Debug, Deserialize)]
pub struct TaskToolRequest {
    pub task_name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct TaskToolResponse {
    pub child_job_id: String,
}

/// POST /worker/jobs/:id/steps/:step/suspend — Worker suspends step for ask_user
#[derive(Debug, Deserialize)]
pub struct SuspendStepRequest {
    pub agent_state: serde_json::Value,
    pub message: String,
}

/// POST /worker/jobs/:id/steps/:step/agent-state — Worker saves intermediate agent state
#[derive(Debug, Deserialize)]
pub struct SaveAgentStateRequest {
    pub agent_state: serde_json::Value,
}

/// POST /worker/register - Register a worker
#[tracing::instrument(skip(state))]
pub async fn register_worker(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRequest>,
) -> Result<impl IntoResponse, AppError> {
    let worker_id = Uuid::new_v4();

    WorkerRepo::register(
        &state.pool,
        worker_id,
        &req.name,
        &req.tags,
        req.version.as_deref(),
    )
    .await
    .context("register worker")?;

    match &req.version {
        Some(v) => tracing::info!("Registered worker: {} ({}) v{}", req.name, worker_id, v),
        None => tracing::info!("Registered worker: {} ({})", req.name, worker_id),
    }

    Ok(Json(RegisterResponse {
        worker_id: worker_id.to_string(),
    }))
}

/// POST /worker/heartbeat - Update worker heartbeat
#[tracing::instrument(skip(state))]
pub async fn heartbeat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<HeartbeatRequest>,
) -> Result<impl IntoResponse, AppError> {
    let worker_id = Uuid::parse_str(&req.worker_id)
        .map_err(|_| AppError::BadRequest("Invalid worker ID".into()))?;

    WorkerRepo::heartbeat(&state.pool, worker_id)
        .await
        .context("update heartbeat")?;

    Ok(Json(json!({"status": "ok"})))
}

/// Fail a step that was claimed but couldn't be rendered.
/// Marks the step as failed, logs the error, and triggers orchestration.
async fn fail_claimed_step(
    state: &Arc<AppState>,
    job_id: Uuid,
    step_name: &str,
    error_msg: &str,
) -> Response {
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
) -> Result<Response, AppError> {
    let worker_id = Uuid::parse_str(&req.worker_id)
        .map_err(|_| AppError::BadRequest("Invalid worker ID".into()))?;

    let step = match JobStepRepo::claim_ready_step(&state.pool, &req.tags, worker_id)
        .await
        .context("claim ready step")?
    {
        Some(step) => step,
        None => {
            return Ok(Json(ClaimResponse {
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
                revision: None,
                agent_provider_name: None,
                agent_prompt: None,
                agent_system_prompt: None,
                mcp_servers: None,
                agent_state: None,
                agent_tool_tasks: None,
                event_source_config: None,
                state_storage_key: None,
                state_has_json: None,
                global_state_storage_key: None,
                global_state_has_json: None,
            })
            .into_response());
        }
    };

    tracing::info!(
        "Worker {} claimed step {} from job {}",
        worker_id,
        step.step_name,
        step.job_id
    );

    // Get the job to access task_name, workspace, and job input
    let job = match JobRepo::get(&state.pool, step.job_id)
        .await
        .context("get job for claimed step")?
    {
        Some(j) => j,
        None => {
            tracing::error!("Job {} not found", step.job_id);
            return Err(AppError::Internal(anyhow::anyhow!(
                "Job {} not found after claim",
                step.job_id
            )));
        }
    };

    // Fetch workspace config once — used for template rendering, secrets, and action defaults
    let ws_config = state.get_workspace(&job.workspace).await;

    // Fetch all steps once — reused for both completed-step template context and agent rendering.
    let all_steps_for_job = JobStepRepo::get_steps_for_job(&state.pool, step.job_id)
        .await
        .unwrap_or_default();

    let completed_steps: Vec<(String, Option<serde_json::Value>)> = all_steps_for_job
        .iter()
        .filter(|s| s.status == StepStatus::Completed.as_ref())
        .map(|s| (s.step_name.clone(), s.output.clone()))
        .collect();

    // ── Task state snapshot lookup ─────────────────────────────────────────
    // Resolve the latest snapshot so the worker knows to download it and so
    // the Tera render context can expose `{{ state.* }}` variables.
    let (state_storage_key, state_has_json, state_json_value) =
        if let Some(ref storage) = state.state_storage {
            match TaskStateRepo::get_latest(&state.pool, &job.workspace, &job.task_name).await {
                Ok(Some(snapshot)) => {
                    // If the snapshot carries a structured JSON sidecar, retrieve and
                    // parse it now so it can be injected into the Tera context.
                    let json_value = if snapshot.has_json {
                        match storage.retrieve(&snapshot.storage_key).await {
                            Ok(Some(data)) => super::state::extract_state_json(&data),
                            Ok(None) => None,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to retrieve state snapshot for Tera context: {:#}",
                                    e
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };
                    (
                        Some(snapshot.storage_key),
                        Some(snapshot.has_json),
                        json_value,
                    )
                }
                Ok(None) => (None, None, None),
                Err(e) => {
                    tracing::warn!("Failed to look up task state snapshot: {:#}", e);
                    (None, None, None)
                }
            }
        } else {
            (None, None, None)
        };

    // ── Global workspace state snapshot lookup ────────────────────────────────
    // Resolve the latest global snapshot so the worker knows to download it and
    // so the Tera render context can expose `{{ global_state.* }}` variables.
    let (global_state_storage_key, global_state_has_json, global_state_json_value) =
        if let Some(ref storage) = state.state_storage {
            match WorkspaceStateRepo::get_latest(&state.pool, &job.workspace).await {
                Ok(Some(snapshot)) => {
                    let json_value = if snapshot.has_json {
                        match storage.retrieve(&snapshot.storage_key).await {
                            Ok(Some(data)) => super::state::extract_state_json(&data),
                            Ok(None) => None,
                            Err(e) => {
                                tracing::warn!(
                                "Failed to retrieve global state snapshot for Tera context: {:#}",
                                e
                            );
                                None
                            }
                        }
                    } else {
                        None
                    };
                    (
                        Some(snapshot.storage_key),
                        Some(snapshot.has_json),
                        json_value,
                    )
                }
                Ok(None) => (None, None, None),
                Err(e) => {
                    tracing::warn!("Failed to look up global workspace state: {:#}", e);
                    (None, None, None)
                }
            }
        } else {
            (None, None, None)
        };

    // Render step input and apply action defaults
    let rendered_input = if let Some(ref workspace) = ws_config {
        let ctx = rendering::RenderContext {
            workspace,
            task_name: &job.task_name,
            step: &step,
            job_input: job.input.as_ref(),
            completed_steps: &completed_steps,
            state_json: state_json_value.as_ref(),
            global_state_json: global_state_json_value.as_ref(),
        };

        let raw_input = match rendering::render_step_input(&ctx) {
            Ok(input) => input,
            Err(e) => {
                let msg = format!("Failed to render step input template: {:#}", e);
                return Ok(fail_claimed_step(&state, step.job_id, &step.step_name, &msg).await);
            }
        };

        match rendering::prepare_step_action_input(raw_input, &ctx) {
            Ok(input) => input,
            Err(e) => {
                let msg = format!("{:#}", e);
                return Ok(fail_claimed_step(&state, step.job_id, &step.step_name, &msg).await);
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
        &completed_steps,
        step.loop_item.as_ref(),
        step.loop_index,
        step.loop_total,
        state_json_value.as_ref(),
        global_state_json_value.as_ref(),
    ) {
        Ok(spec) => spec,
        Err(e) => {
            let msg = format!("{:#}", e);
            return Ok(fail_claimed_step(&state, step.job_id, &step.step_name, &msg).await);
        }
    };

    // Render action_image templates (e.g. {{ input.image_tag }})
    let rendered_image = match rendering::render_image(
        step.action_image.as_deref(),
        rendered_input.as_ref(),
        &secrets_value,
        &completed_steps,
        step.loop_item.as_ref(),
        step.loop_index,
        step.loop_total,
    ) {
        Ok(img) => img,
        Err(e) => {
            let msg = format!("{:#}", e);
            return Ok(fail_claimed_step(&state, step.job_id, &step.step_name, &msg).await);
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

    // ── Agent step rendering ─────────────────────────────────────────
    // For agent steps, resolve templates server-side and populate extra fields.
    let (
        agent_provider_name,
        agent_prompt,
        agent_system_prompt,
        mcp_servers_val,
        agent_state_val,
        agent_tool_tasks,
    ) = if step.action_type == "agent" {
        // Deserialize action spec to get agent fields
        let action_def: Option<stroem_common::models::workflow::ActionDef> = rendered_action_spec
            .as_ref()
            .and_then(|s| serde_json::from_value(s.clone()).ok());

        let provider_name = action_def.as_ref().and_then(|a| a.provider.clone());

        // Reuse the full steps list fetched earlier — avoids a second DB round-trip.
        let render_ctx = if let Some(ref workspace) = ws_config {
            crate::job_creator::build_step_render_context(&job, &all_steps_for_job, workspace)
        } else {
            serde_json::json!({})
        };

        // Render prompt and system_prompt templates
        let prompt = action_def
            .as_ref()
            .and_then(|a| a.prompt.as_deref())
            .and_then(
                |tmpl| match stroem_common::template::render_template(tmpl, &render_ctx) {
                    Ok(rendered) => Some(rendered),
                    Err(e) => {
                        tracing::warn!("Failed to render agent prompt template: {:#}", e);
                        None
                    }
                },
            );

        let system = action_def
            .as_ref()
            .and_then(|a| a.system_prompt.as_deref())
            .and_then(
                |tmpl| match stroem_common::template::render_template(tmpl, &render_ctx) {
                    Ok(rendered) => Some(rendered),
                    Err(e) => {
                        tracing::warn!("Failed to render agent system_prompt template: {:#}", e);
                        None
                    }
                },
            );

        // MCP servers: only send servers referenced by the step's tools
        let mcp = if let (Some(ref action), Some(ref ws)) = (&action_def, &ws_config) {
            let referenced: std::collections::HashSet<&str> = action
                .tools
                .iter()
                .filter_map(|t| match t {
                    stroem_common::models::workflow::AgentToolRef::Mcp { mcp } => {
                        Some(mcp.as_str())
                    }
                    _ => None,
                })
                .collect();
            if referenced.is_empty() {
                None
            } else {
                let filtered: std::collections::HashMap<_, _> = ws
                    .mcp_servers
                    .iter()
                    .filter(|(name, _)| referenced.contains(name.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if filtered.is_empty() {
                    None
                } else {
                    Some(filtered)
                }
            }
        } else {
            None
        };

        // Agent state for resume (from DB)
        let agent_st = step.agent_state.clone();

        // Build task tool infos for LLM tool definitions
        let task_tools = if let (Some(ref action), Some(ref workspace)) = (&action_def, &ws_config)
        {
            let mut tools_map = serde_json::Map::new();
            for tool_ref in &action.tools {
                if let stroem_common::models::workflow::AgentToolRef::Task { task } = tool_ref {
                    if let Some(task_def) = workspace.tasks.get(task) {
                        tools_map.insert(task.clone(), serde_json::json!({
                                "description": task_def.description,
                                "input": stroem_agent::tools::input_schema_to_json_schema(&task_def.input),
                            }));
                    }
                }
            }
            if tools_map.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(tools_map))
            }
        } else {
            None
        };

        (provider_name, prompt, system, mcp, agent_st, task_tools)
    } else {
        (None, None, None, None, None, None)
    };

    // ── Event source metadata ────────────────────────────────────────────────
    // When this step belongs to an event source consumer job, surface the
    // `_event_source` metadata from the job input so the worker can activate
    // stdout JSON-line interception. The worker reads `event_source_config.env`
    // directly — no separate `env_overrides` field is needed.
    let event_source_config = if job.source_type == "event_source" {
        job.input
            .as_ref()
            .and_then(|v| v.get("_event_source"))
            .cloned()
    } else {
        None
    };

    Ok(Json(ClaimResponse {
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
        revision: job.revision.clone(),
        agent_provider_name,
        agent_prompt,
        agent_system_prompt,
        mcp_servers: mcp_servers_val,
        agent_state: agent_state_val,
        agent_tool_tasks,
        event_source_config,
        state_storage_key,
        state_has_json,
        global_state_storage_key,
        global_state_has_json,
    })
    .into_response())
}

/// POST /worker/jobs/:id/steps/:step/start - Mark step as running
#[tracing::instrument(skip(state))]
pub async fn start_step(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(Uuid, String)>,
    Json(req): Json<StartStepRequest>,
) -> Result<impl IntoResponse, AppError> {
    let worker_id = Uuid::parse_str(&req.worker_id)
        .map_err(|_| AppError::BadRequest("Invalid worker ID".into()))?;

    JobStepRepo::mark_running(&state.pool, job_id, &step_name, worker_id)
        .await
        .context("mark step running")?;

    // Also transition the job itself to running (idempotent — no-op if already running)
    if let Err(e) = JobRepo::mark_running_if_pending(&state.pool, job_id, worker_id).await {
        tracing::warn!("Failed to transition job to running: {:#}", e);
    }

    Ok(Json(json!({"status": "ok"})))
}

/// POST /worker/jobs/:id/steps/:step/complete - Mark step as completed
#[tracing::instrument(skip(state))]
pub async fn complete_step(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(Uuid, String)>,
    Json(req): Json<CompleteStepRequest>,
) -> Result<impl IntoResponse, AppError> {
    // Determine if this step failed based on exit_code or error
    let step_failed = req.exit_code.unwrap_or(0) != 0 || req.error.is_some();

    if step_failed {
        let error_msg = req
            .error
            .unwrap_or_else(|| format!("Process exited with code {}", req.exit_code.unwrap_or(1)));
        JobStepRepo::mark_failed(&state.pool, job_id, &step_name, &error_msg)
            .await
            .context("mark step failed")?;
    } else {
        JobStepRepo::mark_completed(&state.pool, job_id, &step_name, req.output)
            .await
            .context("mark step completed")?;
    }

    // Orchestrate: promote steps, skip unreachable, propagate to parent, fire hooks
    crate::job_recovery::orchestrate_after_step(&state, job_id, &step_name)
        .await
        .context("orchestration after step completion")?;

    Ok(Json(json!({"status": "ok"})))
}

/// POST /worker/jobs/:id/logs - Append log chunk
#[tracing::instrument(skip(state))]
pub async fn append_log(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<Uuid>,
    Json(req): Json<AppendLogRequest>,
) -> Result<impl IntoResponse, AppError> {
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

    state
        .log_storage
        .append_log(job_id, &jsonl_chunk)
        .await
        .context("append log")?;

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

    Ok(Json(json!({"status": "ok"})))
}

/// GET /worker/jobs/:id/cancelled - Check if a job has been cancelled
#[tracing::instrument(skip(state))]
pub async fn check_cancelled(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<Uuid>,
) -> impl IntoResponse {
    let cancelled = crate::cancellation::is_cancelled(&state, job_id);
    Json(json!({"cancelled": cancelled}))
}

/// POST /worker/jobs/:id/complete - Mark job as completed (for local mode)
#[tracing::instrument(skip(state))]
pub async fn complete_job(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<Uuid>,
    Json(req): Json<CompleteJobRequest>,
) -> Result<impl IntoResponse, AppError> {
    JobRepo::mark_completed(&state.pool, job_id, req.output)
        .await
        .context("mark job completed")?;

    // Handle terminal state: S3 upload, parent propagation, hooks
    if let Err(e) = crate::job_recovery::handle_job_terminal(&state, job_id).await {
        tracing::error!("Failed to handle job terminal state: {:#}", e);
    }

    Ok(Json(json!({"status": "ok"})))
}

/// POST /worker/jobs/:id/steps/:step/task-tool
/// Worker requests child job creation for a task tool call.
#[tracing::instrument(skip(state))]
pub async fn agent_task_tool(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(Uuid, String)>,
    Json(req): Json<TaskToolRequest>,
) -> Result<impl IntoResponse, AppError> {
    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    // Validate task_name is in the agent step's allowed tools list
    let step = stroem_db::JobStepRepo::get_steps_for_job(&state.pool, job_id)
        .await
        .context("get job steps")?
        .into_iter()
        .find(|s| s.step_name == step_name)
        .ok_or_else(|| AppError::not_found("Step"))?;

    if let Some(ref spec) = step.action_spec {
        if let Some(tools) = spec.get("tools").and_then(|v| v.as_array()) {
            let allowed = tools
                .iter()
                .any(|t| t.get("task").and_then(|v| v.as_str()) == Some(&req.task_name));
            if !allowed {
                return Err(AppError::BadRequest(format!(
                    "Task '{}' is not in the agent step's allowed tools list",
                    req.task_name
                )));
            }
        }
    }

    let workspace = state
        .get_workspace(&job.workspace)
        .await
        .ok_or_else(|| AppError::not_found("Workspace"))?;

    let source_id = format!("{}/{}", job_id, step_name);

    let child_job_id = crate::job_creator::create_child_job_for_task(
        &state.pool,
        &workspace,
        &job.workspace,
        &req.task_name,
        req.input,
        "agent_tool",
        Some(&source_id),
        job_id,
        &step_name,
        job.revision.as_deref(),
    )
    .await
    .context("create child job for task tool")?;

    tracing::info!(
        job_id = %job_id,
        step = %step_name,
        child_job_id = %child_job_id,
        task = %req.task_name,
        "Created child job for agent task tool"
    );

    Ok(Json(TaskToolResponse {
        child_job_id: child_job_id.to_string(),
    }))
}

/// POST /worker/jobs/:id/steps/:step/suspend
/// Worker suspends step for ask_user.
#[tracing::instrument(skip(state))]
pub async fn agent_suspend_step(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(Uuid, String)>,
    Json(req): Json<SuspendStepRequest>,
) -> Result<impl IntoResponse, AppError> {
    // Save agent state
    stroem_db::JobStepRepo::update_agent_state(&state.pool, job_id, &step_name, req.agent_state)
        .await
        .context("save agent state")?;

    // Atomically set output and transition from running to suspended
    let output = serde_json::json!({ "approval_message": req.message });
    sqlx::query(
        "UPDATE job_step SET status = 'suspended', suspended_at = NOW(), \
         output = $3 WHERE job_id = $1 AND step_name = $2 AND status = 'running'",
    )
    .bind(job_id)
    .bind(&step_name)
    .bind(output)
    .execute(&state.pool)
    .await
    .context("suspend agent step")?;

    state
        .append_server_log(
            job_id,
            &format!(
                "[agent] Step '{}': waiting for user input (ask_user)",
                step_name
            ),
        )
        .await;

    // Fire on_suspended hooks
    let job = JobRepo::get(&state.pool, job_id).await.context("get job")?;
    if let Some(ref job) = job {
        if let Some(workspace) = state.get_workspace(&job.workspace).await {
            if let Some(task) = workspace.tasks.get(&job.task_name) {
                crate::hooks::fire_suspended_hooks(
                    &state,
                    &workspace,
                    job,
                    task,
                    &step_name,
                    &req.message,
                )
                .await;
            }
        }
    }

    Ok(Json(serde_json::json!({"status": "suspended"})))
}

/// POST /worker/jobs/:id/steps/:step/agent-state
/// Worker saves intermediate agent state before releasing step.
#[tracing::instrument(skip(state))]
pub async fn agent_save_state(
    State(state): State<Arc<AppState>>,
    Path((job_id, step_name)): Path<(Uuid, String)>,
    Json(req): Json<SaveAgentStateRequest>,
) -> Result<impl IntoResponse, AppError> {
    stroem_db::JobStepRepo::update_agent_state(&state.pool, job_id, &step_name, req.agent_state)
        .await
        .context("save agent state")?;

    Ok(Json(serde_json::json!({"status": "ok"})))
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
            action_name: Some("deploy".to_string()),
            action_type: Some("script".to_string()),
            action_image: None,
            action_spec: None,
            input: None,
            runner: Some("local".to_string()),
            timeout_secs: None,
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
            event_source_config: None,
            state_storage_key: None,
            state_has_json: None,
            global_state_storage_key: None,
            global_state_has_json: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["task_name"], "deploy-api");
    }

    #[test]
    fn test_register_request_deserializes_version() {
        let json = serde_json::json!({
            "name": "worker-1",
            "tags": ["script"],
            "version": "0.5.9"
        });
        let req: RegisterRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.version.as_deref(), Some("0.5.9"));
    }

    #[test]
    fn test_register_request_version_absent_is_none() {
        let json = serde_json::json!({
            "name": "worker-1",
            "tags": ["script"]
        });
        let req: RegisterRequest = serde_json::from_value(json).unwrap();
        assert!(req.version.is_none());
    }

    #[test]
    fn test_register_request_version_null_is_none() {
        let json = serde_json::json!({
            "name": "worker-1",
            "tags": ["script"],
            "version": null
        });
        let req: RegisterRequest = serde_json::from_value(json).unwrap();
        assert!(req.version.is_none());
    }

    #[test]
    fn test_register_request_missing_tags_fails_deserialization() {
        let json = serde_json::json!({"name": "worker-1"});
        let result = serde_json::from_value::<RegisterRequest>(json);
        assert!(result.is_err(), "missing tags must fail deserialization");
    }

    #[test]
    fn test_claim_request_missing_tags_fails_deserialization() {
        let json = serde_json::json!({"worker_id": "abc"});
        let result = serde_json::from_value::<ClaimRequest>(json);
        assert!(result.is_err(), "missing tags must fail deserialization");
    }

    #[test]
    fn test_register_request_with_only_capabilities_fails() {
        let json = serde_json::json!({"name": "old-worker", "capabilities": ["script"]});
        let result = serde_json::from_value::<RegisterRequest>(json);
        assert!(
            result.is_err(),
            "old payload with capabilities but no tags must fail"
        );
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
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
            event_source_config: None,
            state_storage_key: None,
            state_has_json: None,
            global_state_storage_key: None,
            global_state_has_json: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["task_name"].is_null());
    }

    #[test]
    fn test_claim_response_serializes_revision() {
        // When revision is Some it appears in the JSON output.
        let resp = ClaimResponse {
            workspace: Some("default".to_string()),
            job_id: Some("abc-123".to_string()),
            task_name: Some("deploy".to_string()),
            step_name: Some("build".to_string()),
            action_name: Some("build".to_string()),
            action_type: Some("script".to_string()),
            action_image: None,
            action_spec: None,
            input: None,
            runner: Some("local".to_string()),
            timeout_secs: None,
            revision: Some("abc123".to_string()),
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
            event_source_config: None,
            state_storage_key: None,
            state_has_json: None,
            global_state_storage_key: None,
            global_state_has_json: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["revision"], "abc123");
    }

    #[test]
    fn test_claim_response_revision_none_is_omitted() {
        // When revision is None, skip_serializing_if must suppress the key entirely.
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
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
            event_source_config: None,
            state_storage_key: None,
            state_has_json: None,
            global_state_storage_key: None,
            global_state_has_json: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json.get("revision").is_none(),
            "revision: None must not appear in serialized JSON, got: {}",
            json
        );
    }

    #[test]
    fn test_claim_response_event_source_config_omitted_when_none() {
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
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
            event_source_config: None,
            state_storage_key: None,
            state_has_json: None,
            global_state_storage_key: None,
            global_state_has_json: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            json.get("event_source_config").is_none(),
            "event_source_config: None must be omitted from JSON, got: {json}"
        );
        // env_overrides no longer exists as a separate field; env is nested
        // inside event_source_config.
        assert!(
            json.get("env_overrides").is_none(),
            "env_overrides must not appear in serialized JSON (field removed), got: {json}"
        );
    }

    #[test]
    fn test_claim_response_event_source_config_serialized_when_present() {
        let config = serde_json::json!({
            "target_task": "process-event",
            "source_id": "default/my-queue",
            "input_defaults": {},
            "max_in_flight": 5,
            "env": {"QUEUE_URL": "https://example.com"},
        });

        let resp = ClaimResponse {
            workspace: Some("default".to_string()),
            job_id: Some("abc-123".to_string()),
            task_name: Some("my-consumer".to_string()),
            step_name: Some("run".to_string()),
            action_name: Some("consume".to_string()),
            action_type: Some("script".to_string()),
            action_image: None,
            action_spec: None,
            input: None,
            runner: Some("local".to_string()),
            timeout_secs: None,
            revision: None,
            agent_provider_name: None,
            agent_prompt: None,
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
            event_source_config: Some(config),
            state_storage_key: None,
            state_has_json: None,
            global_state_storage_key: None,
            global_state_has_json: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["event_source_config"]["target_task"], "process-event");
        // env is now nested inside event_source_config, not a top-level field.
        assert_eq!(
            json["event_source_config"]["env"]["QUEUE_URL"],
            "https://example.com"
        );
        assert!(
            json.get("env_overrides").is_none(),
            "env_overrides must not be present as a top-level field"
        );
    }
}
