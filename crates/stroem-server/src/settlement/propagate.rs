//! Propagation: a settled child job marks its parent step and advances the
//! parent job.

use super::Settlement;
use anyhow::{Context, Result};
use stroem_common::models::job::JobStatus;
use stroem_db::{JobRow, JobStepRepo};
use uuid::Uuid;

impl Settlement {
    /// Child `child_job` settled: mark the parent step and advance the parent.
    ///
    /// For `agent_tool` children this is gated on a **registration barrier**.
    /// The worker records a child's id in the agent step's `agent_state` only
    /// after the creation response returns, so a child that settles before
    /// that write is not yet part of the conversation. Propagating it anyway
    /// would either drop the tool result or — via the parse fallback — reach
    /// ordinary `mark_completed` and settle an agent step out from under a
    /// still-running worker. Such a child is left alone;
    /// [`Settlement::agent_children_registered`] replays this function for
    /// every already-terminal pending child once the worker registers them.
    ///
    /// Public so tests can drive a job from an arbitrary row state;
    /// production code goes through the entries.
    pub async fn propagate(
        &self,
        child_job: &JobRow,
        parent_job_id: Uuid,
        parent_step: &str,
    ) -> Result<()> {
        // Special handling for agent_tool child jobs: the parent step is an agent step
        // that needs to resume its dispatch loop on a worker. Instead of marking completed/failed,
        // we inject the tool result into agent_state and mark the step ready for re-claim.
        if child_job.source_type == "agent_tool" {
            let parent_steps = JobStepRepo::get_steps_for_job(&self.pool, parent_job_id).await?;
            if let Some(parent_step_row) = parent_steps.iter().find(|s| s.step_name == parent_step)
            {
                let Some(ref state_val) = parent_step_row.agent_state else {
                    tracing::info!(
                        child = %child_job.job_id,
                        parent_job_id = %parent_job_id,
                        step = %parent_step,
                        "agent tool child {} not yet registered on step {}; propagation deferred until agent state is saved",
                        child_job.job_id,
                        parent_step
                    );
                    return Ok(());
                };
                if let Ok(mut conv_state) = serde_json::from_value::<
                    stroem_agent::state::AgentConversationState,
                >(state_val.clone())
                {
                    if !conv_state
                        .pending_tool_calls
                        .iter()
                        .any(|tc| tc.child_job_id == child_job.job_id)
                    {
                        tracing::info!(
                            child = %child_job.job_id,
                            parent_job_id = %parent_job_id,
                            step = %parent_step,
                            "agent tool child {} not yet registered on step {}; propagation deferred until agent state is saved",
                            child_job.job_id,
                            parent_step
                        );
                        return Ok(());
                    }

                    let tool_result_text = if child_job.status == JobStatus::Completed.as_ref() {
                        child_job
                            .output
                            .as_ref()
                            .map(|o| serde_json::to_string(o).unwrap_or_default())
                            .unwrap_or_else(|| "Task completed successfully".to_string())
                    } else {
                        format!("Task failed: {}", child_job.status)
                    };

                    if let Some(resolved) = conv_state.resolve_tool_call(child_job.job_id) {
                        conv_state.resolved_tool_results.push(
                            stroem_agent::state::ResolvedToolResult {
                                tool_call_id: resolved.tool_call_id,
                                result_text: tool_result_text,
                            },
                        );
                    }

                    let updated_state = serde_json::to_value(&conv_state)
                        .context("serialize agent conversation state")?;
                    JobStepRepo::update_agent_state(
                        &self.pool,
                        parent_job_id,
                        parent_step,
                        updated_state,
                    )
                    .await?;

                    if conv_state.all_tool_calls_resolved() {
                        // All tools done — mark step ready so a worker can re-claim it
                        sqlx::query(
                            "UPDATE job_step SET status = 'ready', ready_at = NOW(), worker_id = NULL \
                             WHERE job_id = $1 AND step_name = $2 AND status = 'running'",
                        )
                        .bind(parent_job_id)
                        .bind(parent_step)
                        .execute(&self.pool)
                        .await?;

                        tracing::info!(
                            parent_job_id = %parent_job_id,
                            step = %parent_step,
                            "Agent step marked ready for re-claim after all task tools completed"
                        );
                    } else {
                        tracing::info!(
                            parent_job_id = %parent_job_id,
                            step = %parent_step,
                            pending = conv_state.pending_tool_calls.len(),
                            "Agent tool completed, still waiting for more tools"
                        );
                    }

                    return Ok(());
                }
            }

            // Fallback: if we could not parse agent state, fall through to normal propagation
            tracing::warn!(
                parent_job_id = %parent_job_id,
                step = %parent_step,
                "Could not process agent_tool child completion — falling through to normal propagation"
            );
        }

        if child_job.status == JobStatus::Completed.as_ref() {
            JobStepRepo::mark_completed(
                &self.pool,
                parent_job_id,
                parent_step,
                child_job.output.clone(),
            )
            .await?;
        } else if child_job.status == JobStatus::Cancelled.as_ref() {
            JobStepRepo::mark_cancelled(&self.pool, parent_job_id, parent_step).await?;
        } else {
            let err = format!("Child job {} failed", child_job.job_id);
            JobStepRepo::mark_failed(&self.pool, parent_job_id, parent_step, &err).await?;
        }

        Box::pin(self.advance(parent_job_id)).await
    }
}
