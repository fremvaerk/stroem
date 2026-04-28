use crate::state::AppState;
use crate::web::error::AppError;
use anyhow::Context;
use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use stroem_common::models::workflow::TriggerDef;

/// Request body for the emit endpoint.
///
/// Sent by the worker when an event source process emits a JSON line to stdout.
/// The server creates a new job for the target task with the provided input.
#[derive(Debug, Deserialize)]
pub struct EmitRequest {
    /// Workspace the event source trigger belongs to.
    pub workspace: String,
    /// Target task to execute (the task that processes the emitted event).
    pub task: String,
    /// Input data for the new job (from the emitted JSON line).
    pub input: serde_json::Value,
    /// The source_id for the event source ("{workspace}/{trigger_name}").
    /// Used to link emitted jobs back to their origin.
    pub source_id: String,
}

/// Response body for the emit endpoint.
#[derive(Debug, Serialize)]
pub struct EmitResponse {
    /// UUID of the newly created job.
    pub job_id: String,
}

/// POST /worker/event-source/emit
///
/// Called by a worker running an event source step when the event source
/// process emits a JSON line to stdout. Creates a new job for the target
/// task configured in the event source trigger.
#[tracing::instrument(skip(state, req))]
pub async fn emit_event(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EmitRequest>,
) -> Result<Json<EmitResponse>, AppError> {
    let workspace_config = state
        .get_workspace(&req.workspace)
        .await
        .ok_or_else(|| AppError::NotFound(format!("Workspace '{}' not found", req.workspace)))?;

    // Validate that source_id matches an enabled EventSource trigger and that
    // the request targets the trigger's configured target_task.
    let parts: Vec<&str> = req.source_id.splitn(2, '/').collect();
    if parts.len() != 2 {
        return Err(AppError::BadRequest("Invalid source_id format".to_string()));
    }
    let (ws_name, trigger_name) = (parts[0], parts[1]);
    if ws_name != req.workspace {
        return Err(AppError::BadRequest(
            "source_id workspace mismatch".to_string(),
        ));
    }
    match workspace_config.triggers.get(trigger_name) {
        Some(TriggerDef::EventSource {
            target_task,
            enabled: true,
            ..
        }) if target_task == &req.task => {}
        Some(TriggerDef::EventSource { enabled: false, .. }) => {
            return Err(AppError::BadRequest(format!(
                "No active event source trigger '{}' targeting task '{}'",
                trigger_name, req.task
            )));
        }
        Some(TriggerDef::EventSource { .. }) => {
            return Err(AppError::BadRequest(format!(
                "No active event source trigger '{}' targeting task '{}'",
                trigger_name, req.task
            )));
        }
        Some(_) => {
            // Trigger exists but is not an EventSource variant (scheduler/webhook).
            return Err(AppError::BadRequest(format!(
                "Trigger '{}' is not an event source trigger",
                trigger_name
            )));
        }
        None => {
            return Err(AppError::BadRequest(format!(
                "No active event source trigger '{}' targeting task '{}'",
                trigger_name, req.task
            )));
        }
    }

    let revision = state.workspaces.get_revision(&req.workspace);

    let job_id = crate::job_creator::create_job_for_task(
        &state.pool,
        &workspace_config,
        &req.workspace,
        &req.task,
        req.input,
        "event_source",
        Some(&req.source_id),
        revision.as_deref(),
        None,
        state.config.agents.as_ref(),
    )
    .await
    .with_context(|| {
        format!(
            "Failed to create job for task '{}' in workspace '{}'",
            req.task, req.workspace
        )
    })
    .map_err(AppError::Internal)?;

    // Fire on_suspended hooks for any root-level approval steps that were
    // suspended during job creation (mirrors the scheduler pattern).
    crate::job_creator::fire_initial_suspended_hooks(
        &state,
        &workspace_config,
        &req.workspace,
        &req.task,
        job_id,
    )
    .await;

    tracing::info!(
        "emit_event: created job {} for task '{}' in workspace '{}' (source_id='{}')",
        job_id,
        req.task,
        req.workspace,
        req.source_id,
    );

    Ok(Json(EmitResponse {
        job_id: job_id.to_string(),
    }))
}
