//! Approve or reject a suspended step: the one body behind
//! `POST /api/jobs/{id}/steps/{step}/approve` and the MCP `approve_step` tool.
//! It covers a `type: approval` gate and an agent step suspended by `ask_user`
//! (the same route resumes it).

use super::caller::Caller;
use super::jobs::ApproveStepRequest;
use crate::acl::TaskPermission;
use crate::state::AppState;
use crate::web::error::AppError;
use anyhow::Context;
use chrono::Utc;
use serde_json::json;
use stroem_common::models::job::StepStatus;
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

/// The decision that was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Approved,
    Rejected,
}

impl Decision {
    /// The response body, the same for REST and MCP: `{"status": …}`.
    pub(crate) fn body(self) -> serde_json::Value {
        match self {
            Self::Approved => json!({"status": "approved"}),
            Self::Rejected => json!({"status": "rejected"}),
        }
    }
}

/// Approve or reject `step_name` of `job_id` for `caller`.
///
/// Order: 401 (auth configured, no caller; the route's `require_auth` and the
/// MCP auth layer refuse that earlier) → 404 job missing or ACL `Deny` → 403
/// ACL `View` → 404 step missing → 409 not suspended / not an approval or
/// agent step → 400 a required approval field missing (only when `input` is
/// given). The approver recorded in the step output and the job log is the
/// caller's email, `anonymous` with auth off. A reject goes through
/// `JobStepRepo::fail_or_retry` inline, so the `[approval] … rejected` line is
/// logged before any `[retry]` line.
#[tracing::instrument(skip_all, fields(job_id = %job_id, step = %step_name, approved = req.approved))]
pub(crate) async fn approve_step(
    state: &AppState,
    caller: Caller<'_>,
    job_id: Uuid,
    step_name: &str,
    req: ApproveStepRequest,
) -> Result<Decision, AppError> {
    if state.config.auth.is_some() && caller.claims.is_none() {
        return Err(AppError::Unauthorized("Authentication required".into()));
    }

    let job = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    // ACL check — approve/reject requires Run permission
    let perm = super::jobs::job_permission(state, caller, &job).await?;
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
    let approver = caller.email().unwrap_or("anonymous").to_string();

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
            let user_response = super::jobs::extract_agent_response(req.input.as_ref());

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
                    JobStepRepo::update_agent_state(&state.pool, job_id, step_name, updated_state)
                        .await
                        .context("save agent state after ask_user approval")?;

                    // Mark step ready for re-claim by a worker
                    sqlx::query(
                        "UPDATE job_step SET status = 'ready', ready_at = NOW(), worker_id = NULL \
                         WHERE job_id = $1 AND step_name = $2 AND status = 'suspended'",
                    )
                    .bind(job_id)
                    .bind(step_name)
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

                    return Ok(Decision::Approved);
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
        let applied = JobStepRepo::approve_step(&state.pool, job_id, step_name, Some(output))
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

        if let Err(e) = state.settlement().step_settled(job_id, step_name).await {
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

        Ok(Decision::Approved)
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

        // Atomic reject — only succeeds if the step is still suspended; decides
        // retry in the same transaction. Called directly rather than through
        // `Settlement::step_failed` so the `[approval] ... rejected` line is
        // logged BEFORE any `[retry]` line: `step_failed` appends the retry line
        // itself, which would read as a retry of a step nothing had yet
        // rejected. A rejected approval with retry budget returns to `ready`
        // and is re-suspended only by a later orchestration of this job
        // (`handle_approval_steps`); if no other step completes, it stays
        // `ready` — see TODO.md "approval steps with retry".
        let outcome = JobStepRepo::fail_or_retry(
            &state.pool,
            job_id,
            step_name,
            &reason,
            &[StepStatus::Suspended],
            crate::settlement::retry::compute_retry_delay,
            None,
        )
        .await
        .context("reject suspended step")?;

        if matches!(outcome, stroem_db::FailOutcome::NotApplied) {
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

        if let Some(line) = crate::settlement::retry::retry_log_line(step_name, &outcome) {
            state.append_server_log(job_id, &line).await;
        }

        if matches!(outcome, stroem_db::FailOutcome::Failed { .. }) {
            if let Err(e) = state.settlement().step_settled(job_id, step_name).await {
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
        }

        Ok(Decision::Rejected)
    }
}
