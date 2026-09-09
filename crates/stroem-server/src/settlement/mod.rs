//! Job settlement: everything a job owes after one of its steps moves, from
//! the step cascade through terminal handling. See
//! `docs/superpowers/specs/2026-09-08-job-settlement-design.md` and the
//! `### Settlement` section of CLAUDE.md.

pub mod dispatch;
pub mod hooks;
mod propagate;
pub mod retry;
pub mod settle;
pub mod terminal;

pub use settle::{cascade_and_settle, settle_if_all_terminal, Settled};

use crate::config::JobDefaults;
use crate::events::EventBus;
use crate::job_completion::{JobCompletionEvent, JobCompletionNotifier};
use crate::log_broadcast::LogBroadcast;
use crate::log_storage::LogStorage;
use crate::state::AppState;
use crate::workspace::WorkspaceManager;
use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use stroem_common::models::job::{JobStatus, SourceType, StepStatus};
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{FailOutcome, JobRepo, JobRow, JobStepRepo};
use uuid::Uuid;

/// The settlement module's dependencies: eight of `AppState`'s fields.
#[derive(Clone)]
pub struct Settlement {
    pub(crate) pool: PgPool,
    pub(crate) workspaces: Arc<WorkspaceManager>,
    pub(crate) defaults: JobDefaults,
    pub(crate) log_storage: Arc<LogStorage>,
    pub(crate) log_broadcast: Arc<LogBroadcast>,
    pub(crate) job_completion: Arc<JobCompletionNotifier>,
    pub(crate) cancelled_jobs: Arc<RwLock<HashSet<Uuid>>>,
    pub(crate) event_bus: EventBus,
}

/// An agent tool child that can never deliver a tool result (spec §6.5).
#[derive(Debug)]
pub struct BornTerminal {
    pub job_id: Uuid,
    pub status: String,
}

/// Result of job creation. Consume it with `Settlement::job_created` (or
/// `agent_child_created` for agent tool children); nothing else can act on
/// the terminal-at-creation flag.
///
/// Residual hole: `create_job_for_task_detailed(..).await?.job_id` moves the
/// id out and drops the struct without finalizing it; `#[must_use]` does not
/// catch field access. Reviewers: every creation site ends in `job_created`.
#[must_use = "pass to Settlement::job_created or agent_child_created"]
#[derive(Debug)]
pub struct CreatedJob {
    pub job_id: Uuid,
    terminal_at_creation: bool,
}

impl CreatedJob {
    pub(crate) fn new(job_id: Uuid, terminal_at_creation: bool) -> Self {
        Self {
            job_id,
            terminal_at_creation,
        }
    }
}

/// Result of a cancel operation
#[derive(Debug)]
pub enum CancelResult {
    /// Job was cancelled successfully
    Cancelled,
    /// Job was not found
    NotFound,
    /// Job is already in a terminal state
    AlreadyTerminal,
}

impl AppState {
    /// Cheap: clones of `Arc`s and a pool handle.
    pub fn settlement(&self) -> Settlement {
        Settlement {
            pool: self.pool.clone(),
            workspaces: self.workspaces.clone(),
            defaults: JobDefaults::from(self.config.as_ref()),
            log_storage: self.log_storage.clone(),
            log_broadcast: self.log_broadcast.clone(),
            job_completion: self.job_completion.clone(),
            cancelled_jobs: self.cancelled_jobs.clone(),
            event_bus: self.event_bus.clone(),
        }
    }
}

impl Settlement {
    /// Private copy of [`AppState::append_server_log`] (same JSONL shape, same
    /// broadcast, same best-effort NOTIFY).
    pub(crate) async fn server_log(&self, job_id: Uuid, message: &str) {
        let line = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "stream": "stderr",
            "step": "_server",
            "line": message,
        });
        let chunk = format!("{}\n", line);
        if let Err(e) = self.log_storage.append_log(job_id, &chunk).await {
            tracing::warn!("Failed to write server log for job {}: {:#}", job_id, e);
            return;
        }
        self.log_broadcast.broadcast(job_id, chunk.clone()).await;
        // Forward to peer replicas so WS viewers on followers see server-side
        // errors (recovery, hooks, orchestration) live, not just on this replica.
        // Discard the publish result here: we're already inside an _server log
        // path, and emitting another _server log on failure would recurse.
        // The `tracing::warn!` inside `publish_log_segment` is sufficient.
        let _ = self.event_bus.publish_log_chunk(job_id, &chunk).await;
    }

    /// Resolve the workspace config and task for a job row, with the
    /// hook / event-source minimal-task fallback. `None` when either is
    /// missing (already logged).
    async fn resolve(&self, job: &JobRow) -> Result<Option<(Arc<WorkspaceConfig>, TaskDef)>> {
        let Some(workspace) = self.workspaces.get_config(&job.workspace).await else {
            tracing::error!("Workspace '{}' not found", job.workspace);
            return Ok(None);
        };
        let task = match workspace.tasks.get(&job.task_name) {
            Some(t) => t.clone(),
            None if job.source_type == SourceType::Hook.as_ref()
                || job.source_type == SourceType::EventSource.as_ref() =>
            {
                terminal::build_minimal_task_def(self, job.job_id).await?
            }
            None => {
                tracing::error!(
                    "Task '{}' not found in workspace '{}'",
                    job.task_name,
                    job.workspace
                );
                return Ok(None);
            }
        };
        Ok(Some((workspace, task)))
    }

    /// Move `job_id` as far as its rows allow. Spec §6.3. Idempotent: every
    /// effect is guarded (cascade guards, settlement re-read, drain gate,
    /// claim).
    ///
    /// The ordering of the terminal block is load-bearing:
    ///
    /// - **Drain before claim.** A job row can be terminal while its workers
    ///   still run (`JobRepo::cancel` stamps `cancelled` immediately). Without
    ///   the gate the first worker to report would win the one-shot claim and
    ///   `close_log` + archive while a sibling is still emitting; the later
    ///   completion loses the claim and the archive is never refreshed.
    /// - **`clear_cancelled` behind the gate**, for the same reason: the
    ///   cancellation signal must stay visible until the workers acknowledge.
    /// - **The claim is one-shot**, so a propagation error must never abort the
    ///   rest of the block — no later path can re-observe these effects.
    /// - **Archive after hooks**, so the server events hooks write are in the
    ///   uploaded log.
    ///
    /// An unresolvable workspace or task stops execution (step 3 needs the
    /// flow) but NOT terminal handling: a terminal job whose workspace is no
    /// longer loaded still drains, takes its claim, counts, and propagates to
    /// its parent — only hooks, retry and the archive are skipped, exactly as
    /// the pre-`Settlement` terminal path did. Dropping the claim there would
    /// strand the parent step forever, since no later path re-observes it.
    ///
    /// Public so tests can drive a job from an arbitrary row state;
    /// production code goes through the entries.
    #[tracing::instrument(skip(self))]
    pub async fn advance(&self, job_id: Uuid) -> Result<()> {
        let Some(job) = JobRepo::get(&self.pool, job_id).await? else {
            tracing::warn!("Job {} not found during orchestration", job_id);
            return Ok(());
        };
        let resolved = self.resolve(&job).await?;

        // Step 3 — once, not a loop (spec §6.3).
        if !is_terminal(&job.status) {
            let Some((workspace, task)) = resolved.as_ref() else {
                return Ok(());
            };
            // Run the cascade: promote steps, skip unreachable, expand/resolve
            // for_each placeholders (inside the cascade), check terminal.
            cascade_and_settle(&self.pool, job_id, task, workspace).await?;

            // Handle any newly-promoted type: task steps (including loop instances)
            if let Err(e) = dispatch::handle_task_steps(
                &self.workspaces,
                &self.pool,
                workspace,
                &job.workspace,
                job_id,
                task,
                self.defaults,
            )
            .await
            {
                tracing::error!("Failed to handle task steps for job {}: {:#}", job_id, e);
                self.server_log(
                    job_id,
                    &format!("[orchestration] Failed to handle task steps: {:#}", e),
                )
                .await;
            }
            self.reconcile(job_id).await;
            self.dispatch_approvals(&job, workspace, task).await?;
        }

        // Step 4.
        let Some(job) = JobRepo::get(&self.pool, job_id).await? else {
            return Ok(());
        };
        if !is_terminal(&job.status) {
            return Ok(());
        }
        // Drain gate: a sibling worker may still be executing a step of this
        // job (a cancel stamps the job terminal immediately). Its completion
        // re-enters here and drains the job.
        if !terminal::drained(self, job_id).await {
            return Ok(());
        }
        // Cancelled jobs stay in the cancelled set until they drain, so
        // workers keep seeing the signal. Now that they have, drop it.
        crate::cancellation::clear_cancelled_in(&self.cancelled_jobs, job_id);
        // Exactly-once claim: only the winner propagates to the parent,
        // creates a retry job, and runs terminal actions (hooks, notify,
        // archive) for this job. See `terminal::claim`'s doc comment.
        if !terminal::claim(self, &job).await {
            tracing::debug!(job_id = %job_id, "advance: terminal handling already claimed, skipping");
            return Ok(());
        }

        let plan = terminal::plan(&job);
        if plan.propagate {
            let (Some(parent_job_id), Some(ref parent_step)) =
                (job.parent_job_id, &job.parent_step_name)
            else {
                unreachable!("plan.propagate implies both parent fields are set")
            };
            if let Err(e) = self.propagate(&job, parent_job_id, parent_step).await {
                tracing::error!(
                    "Failed to propagate child job {} to parent {}: {:#}",
                    job.job_id,
                    parent_job_id,
                    e
                );
                self.server_log(
                    job_id,
                    &format!(
                        "[orchestration] Failed to propagate to parent job {}: {:#}",
                        parent_job_id, e
                    ),
                )
                .await;
            }
        }
        // Hooks, retry and the archive need the flow definition. The claim,
        // the metric and the parent propagation above do not, and must still
        // run for a job whose workspace is gone (the old terminal path's
        // "skipping hooks and S3 upload" branch). `resolve` already logged.
        let Some((workspace, task)) = resolved.as_ref() else {
            return Ok(());
        };
        // Task-level retry: if the job failed and has retries remaining,
        // create a new retry job instead of running terminal actions (hooks
        // etc.). Child jobs (type: task sub-jobs) never retry — their parent
        // is responsible for retry at the job level.
        if plan.retry {
            match retry::create_retry_job(self, &job, workspace, task).await {
                Ok(Some(created)) => {
                    // Retry job created. Still upload logs for this failed
                    // attempt, but skip hooks — they fire only on the final
                    // failure.
                    terminal::upload_logs_for_job(self, &job).await;
                    self.job_completion
                        .notify(JobCompletionEvent {
                            job_id,
                            status: job.status.clone(),
                            output: job.output.clone(),
                        })
                        .await;
                    Box::pin(self.job_created(created)).await;
                    return Ok(());
                }
                Ok(None) => {} // retry failed to create — fall through
                Err(e) => {
                    tracing::error!("Failed to create retry job for {}: {:#}", job_id, e);
                    self.server_log(
                        job_id,
                        &format!("[retry] Failed to create retry job: {:#}", e),
                    )
                    .await;
                }
            }
        }
        // Fire hooks, notify waiters, upload to S3.
        // This intentionally runs for child jobs too (source_type == "task"):
        // - fire_hooks() already skips workspace-level hooks for non-top-level jobs
        // - task-level hooks should fire regardless of how the task was invoked
        // - S3 upload is per-job (each child has its own log file)
        // - job_completion.notify() is a no-op when no sync waiters exist
        terminal::run_terminal_actions(self, &job, workspace, task, plan.hooks).await;
        Ok(())
    }

    /// Dispatch newly-promoted `type: approval` steps and fire `on_suspended`
    /// hooks for the steps that just entered `suspended`.
    async fn dispatch_approvals(
        &self,
        job: &JobRow,
        workspace: &WorkspaceConfig,
        task: &TaskDef,
    ) -> Result<()> {
        let job_id = job.job_id;

        // Snapshot steps before suspension to detect which steps just became suspended
        let steps_before = JobStepRepo::get_steps_for_job(&self.pool, job_id).await?;
        let previously_suspended: HashSet<&str> = steps_before
            .iter()
            .filter(|s| s.status == StepStatus::Suspended.as_ref())
            .map(|s| s.step_name.as_str())
            .collect();

        if let Err(e) =
            dispatch::handle_approval_steps(&self.pool, workspace, &job.workspace, job_id, task)
                .await
        {
            tracing::error!(
                "Failed to handle approval steps for job {}: {:#}",
                job_id,
                e
            );
            self.server_log(
                job_id,
                &format!("[orchestration] Failed to handle approval steps: {:#}", e),
            )
            .await;
        }

        // Fire on_suspended hooks for steps that newly entered suspended state
        let steps_after = JobStepRepo::get_steps_for_job(&self.pool, job_id).await?;
        for step in &steps_after {
            if step.status == StepStatus::Suspended.as_ref()
                && !previously_suspended.contains(step.step_name.as_str())
            {
                let rendered_message = step
                    .output
                    .as_ref()
                    .and_then(|o| o["approval_message"].as_str())
                    .unwrap_or("")
                    .to_string();

                self.server_log(
                    job_id,
                    &format!("[approval] Step '{}' waiting for approval", step.step_name),
                )
                .await;

                hooks::fire_suspended_hooks(
                    self,
                    workspace,
                    job,
                    task,
                    &step.step_name,
                    &rendered_message,
                )
                .await;
            }
        }
        Ok(())
    }

    /// Descendants of `root_job_id` that are terminal while their parent step
    /// is still `running` never reached [`Settlement::propagate`] (they settled
    /// inside `create_job_for_task_inner`, which has no `AppState`). Advance
    /// each; that propagates to the parent step and fires that job's hooks. The
    /// "parent step still running" predicate makes this idempotent.
    ///
    /// The walk covers the WHOLE descendant chain, not just direct children:
    /// with P → C → G, a G that settles at creation leaves C `running` with no
    /// step that will ever complete, so C never orchestrates and only a
    /// descendant walk from P reaches G. Rows arrive deepest-first, so handling
    /// G settles C via propagation, and the exactly-once claim makes the later
    /// visit to C in the same loop a no-op.
    ///
    /// Public so tests can drive a job from an arbitrary row state.
    pub async fn reconcile(&self, root_job_id: Uuid) {
        let children = match JobRepo::get_settled_descendants_with_running_parent_step(
            &self.pool,
            root_job_id,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(job_id = %root_job_id, "reconcile_settled_children: {:#}", e);
                return;
            }
        };
        for child in children {
            tracing::info!(
                child = %child.job_id, root = %root_job_id,
                "descendant job settled at creation — running terminal handling"
            );
            // `advance` → `propagate` → `reconcile` → `advance` forms a call
            // cycle; box this leg to avoid an infinitely-sized future.
            if let Err(e) = Box::pin(self.advance(child.job_id)).await {
                tracing::error!(child = %child.job_id, "terminal handling for settled descendant failed: {:#}", e);
            }
        }
    }

    // ── Entries (spec §6.2) ────────────────────────────────────────────

    /// A step of `job_id` reached a terminal status (or was reset by an agent
    /// tool result).
    pub async fn step_settled(&self, job_id: Uuid, step_name: &str) -> Result<()> {
        tracing::info!("Orchestrating after step '{}' completed", step_name);
        self.advance(job_id).await
    }

    /// Record a step failure, deciding its retry atomically (see
    /// `JobStepRepo::fail_or_retry`), append the matching server-log line, and
    /// advance the job when the outcome is `Failed`. `RetryScheduled` and
    /// `NotApplied` do not advance — the step is `ready` again.
    pub async fn step_failed(
        &self,
        job_id: Uuid,
        step_name: &str,
        error: &str,
        expected: &[StepStatus],
    ) -> Result<FailOutcome> {
        let outcome = JobStepRepo::fail_or_retry(
            &self.pool,
            job_id,
            step_name,
            error,
            expected,
            retry::compute_retry_delay,
        )
        .await
        .with_context(|| format!("fail_or_retry for step '{}' of job {}", step_name, job_id))?;
        if let Some(line) = retry::retry_log_line(step_name, &outcome) {
            self.server_log(job_id, &line).await;
        }
        if matches!(outcome, FailOutcome::Failed { .. }) {
            self.step_settled(job_id, step_name).await?;
        }
        Ok(outcome)
    }

    /// Run the side effects a freshly created job may already owe.
    ///
    /// - `terminal_at_creation` → `advance` (hooks, metrics, archive, parent
    ///   propagation) — the creator itself has no `AppState`.
    /// - Always → `reconcile`: `type: task` root steps dispatched at creation
    ///   may have produced a child that settled synchronously.
    ///
    /// Best-effort: creation already succeeded, so problems are logged, not
    /// returned.
    pub async fn job_created(&self, created: CreatedJob) {
        if created.terminal_at_creation {
            // `advance` can, via hook dispatch, create a `type: task` hook job
            // that itself settles synchronously and calls back into
            // `job_created` — box this leg to avoid an infinitely-sized future.
            if let Err(e) = Box::pin(self.advance(created.job_id)).await {
                tracing::error!(job_id = %created.job_id, "terminal handling after creation failed: {:#}", e);
            }
        }
        self.reconcile(created.job_id).await;
    }

    /// Agent tool child: reconcile its subtree, then reject it if it is
    /// terminal. Never finalizes (the registration barrier, spec §6.5).
    pub async fn agent_child_created(&self, created: CreatedJob) -> Result<Uuid, BornTerminal> {
        // The child's own subtree may contain a descendant that settled
        // synchronously — a nested `type: task` grandchild whose every root
        // step is skipped leaves the child `running` with no step that will
        // ever complete. Walk the CHILD's subtree so such a child settles here
        // and is caught by the terminal check below, instead of hanging the
        // agent forever.
        self.reconcile(created.job_id).await;
        let status = JobRepo::get(&self.pool, created.job_id)
            .await
            .ok()
            .flatten()
            .map(|j| j.status)
            .unwrap_or_else(|| "unknown".to_string());
        if created.terminal_at_creation || is_terminal(&status) {
            return Err(BornTerminal {
                job_id: created.job_id,
                status,
            });
        }
        Ok(created.job_id)
    }

    /// Lift the agent registration barrier for children that already settled.
    ///
    /// A task-tool child can reach a terminal state before the worker's
    /// `agent-state` write records its id, in which case its own terminal
    /// handling found no registration and deliberately deferred propagation
    /// ([`Settlement::propagate`]). Once the ids are persisted, replay
    /// propagation for every pending child that is already terminal so the
    /// tool result reaches the conversation and the step is released for
    /// re-claim.
    ///
    /// The child's terminal-handling claim was consumed by its own settlement;
    /// propagation is the piece that was skipped, so it is called directly
    /// rather than through `advance`. Best-effort: failures are logged and
    /// never fail the worker's request.
    pub async fn agent_children_registered(&self, job_id: Uuid, step_name: &str) {
        let agent_state = match JobStepRepo::get_steps_for_job(&self.pool, job_id).await {
            Ok(steps) => steps
                .into_iter()
                .find(|s| s.step_name == step_name)
                .and_then(|s| s.agent_state),
            Err(e) => {
                tracing::warn!(
                    job_id = %job_id,
                    step = %step_name,
                    "could not load steps to replay settled tool children: {:#}",
                    e
                );
                return;
            }
        };
        let Some(agent_state) = agent_state else {
            return;
        };

        let conv = match serde_json::from_value::<stroem_agent::state::AgentConversationState>(
            agent_state,
        ) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    job_id = %job_id,
                    step = %step_name,
                    "could not parse saved agent state to replay settled tool children: {:#}",
                    e
                );
                return;
            }
        };

        for pending in &conv.pending_tool_calls {
            let child = match JobRepo::get(&self.pool, pending.child_job_id).await {
                Ok(Some(c)) => c,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        child = %pending.child_job_id,
                        "could not load pending tool-call child job: {:#}",
                        e
                    );
                    continue;
                }
            };
            if !is_terminal(&child.status) {
                continue;
            }

            tracing::info!(
                child = %child.job_id,
                job_id = %job_id,
                step = %step_name,
                "pending tool-call child was already terminal at registration — replaying propagation"
            );
            if let Err(e) = self.propagate(&child, job_id, step_name).await {
                tracing::error!(
                    child = %child.job_id,
                    job_id = %job_id,
                    step = %step_name,
                    "failed to replay propagation for settled tool child: {:#}",
                    e
                );
            }
        }
    }

    /// Local-mode worker reported the whole job complete.
    pub async fn worker_completed_job(
        &self,
        job_id: Uuid,
        output: Option<serde_json::Value>,
    ) -> Result<()> {
        JobRepo::mark_completed(&self.pool, job_id, output)
            .await
            .context("mark job completed")?;
        // The completion WRITE is the worker's contract and its failure must
        // reach the worker as a 5xx; terminal handling is best-effort and only
        // logged, as it was before the settlement module.
        if let Err(e) = self.advance(job_id).await {
            tracing::error!("Failed to handle job terminal state: {:#}", e);
        }
        Ok(())
    }

    /// Cancel a job and all its child jobs recursively.
    ///
    /// 1. Mark the job as cancelled in the database
    /// 2. Cancel all pending/ready steps
    /// 3. Record running steps in `cancelled_jobs` set for worker polling
    /// 4. Recurse into active child jobs
    /// 5. If no running steps remain, trigger terminal handling immediately
    ///
    /// **Race window**: There is a small window between the DB update (step 1) and
    /// the in-memory set insertion (step 3). A worker polling `check_cancelled`
    /// during this window will not yet see the cancellation, but will catch it on
    /// the next poll cycle (typically 5s). This is acceptable since the DB is the
    /// source of truth and the in-memory set is a best-effort optimisation.
    ///
    /// **Restart behaviour**: The in-memory `cancelled_jobs` set is not persisted.
    /// On server restart, any jobs that were cancelled but still had running steps
    /// will be handled by the recovery sweeper: it detects stale workers, fails
    /// their stuck steps, and orchestrates the job to terminal state.
    #[tracing::instrument(skip(self))]
    pub async fn cancel(&self, job_id: Uuid) -> Result<CancelResult> {
        // Check if job exists first
        if JobRepo::get(&self.pool, job_id).await?.is_none() {
            return Ok(CancelResult::NotFound);
        }

        // Try to cancel the job (only works for pending/running)
        let updated = JobRepo::cancel(&self.pool, job_id)
            .await
            .context("Failed to cancel job")?;

        if !updated {
            // Job is already terminal
            return Ok(CancelResult::AlreadyTerminal);
        }

        // Cancel all pending/ready steps
        let cancelled_count = JobStepRepo::cancel_pending_steps(&self.pool, job_id)
            .await
            .context("Failed to cancel pending steps")?;
        tracing::info!(
            "Cancelled {} pending/ready steps for job {}",
            cancelled_count,
            job_id
        );

        // Cancel server-managed running steps (for_each placeholders, type:task steps).
        // These have no worker to signal — transition them directly to cancelled.
        let server_managed_count = JobStepRepo::cancel_server_managed_steps(&self.pool, job_id)
            .await
            .context("Failed to cancel server-managed steps")?;
        if server_managed_count > 0 {
            tracing::info!(
                "Cancelled {} server-managed running steps for job {}",
                server_managed_count,
                job_id
            );
        }

        // Get running steps — these need active kill from the worker
        let running_steps = JobStepRepo::get_running_steps(&self.pool, job_id)
            .await
            .context("Failed to get running steps")?;

        let has_running_steps = !running_steps.is_empty();

        if has_running_steps {
            // Add to cancelled_jobs set so workers polling this replica detect
            // the cancellation immediately.
            self.cancelled_jobs
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(job_id);
            tracing::info!(
                "Added job {} to cancelled_jobs set ({} running steps to kill)",
                job_id,
                running_steps.len()
            );

            // Propagate to peer replicas so workers polling any other server see
            // the cancellation without waiting for their next sweep cycle.
            // Best-effort: DB is the source of truth; recovery sweeper will
            // catch stragglers if NOTIFY fails.
            self.event_bus.publish_job_cancelled(job_id).await;
        }

        // Log cancellation
        self.server_log(job_id, "Job cancelled by user").await;

        // Recursively cancel child jobs
        let child_jobs = JobRepo::get_child_jobs(&self.pool, job_id)
            .await
            .context("Failed to get child jobs")?;

        for child in &child_jobs {
            if let Err(e) = Box::pin(self.cancel(child.job_id)).await {
                tracing::error!(
                    "Failed to cancel child job {} of parent {}: {:#}",
                    child.job_id,
                    job_id,
                    e
                );
            }
        }

        // Unconditional: the drain gate inside `advance` returns early while a
        // worker still owns a step, which is exactly the old `!has_running_steps`
        // condition, now in one place (spec §6.7).
        if let Err(e) = self.advance(job_id).await {
            tracing::error!(
                "Failed to handle terminal state for cancelled job {}: {:#}",
                job_id,
                e
            );
        }

        Ok(CancelResult::Cancelled)
    }
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status.parse::<JobStatus>().ok(),
        Some(JobStatus::Completed)
            | Some(JobStatus::Failed)
            | Some(JobStatus::Cancelled)
            | Some(JobStatus::Skipped)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancel_result_debug() {
        let result = CancelResult::Cancelled;
        assert!(format!("{:?}", result).contains("Cancelled"));

        let result = CancelResult::NotFound;
        assert!(format!("{:?}", result).contains("NotFound"));

        let result = CancelResult::AlreadyTerminal;
        assert!(format!("{:?}", result).contains("AlreadyTerminal"));
    }
}
