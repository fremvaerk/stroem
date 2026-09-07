use anyhow::{Context, Result};
use sqlx::PgPool;
use std::collections::HashSet;
use stroem_common::models::job::{JobStatus, StepStatus};
use stroem_common::models::workflow::{TaskDef, WorkspaceConfig};
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

/// Handle step completion and orchestrate next steps.
///
/// `workspace_config` is optional — when provided, `when` conditions on steps
/// are evaluated (secrets are available for template rendering). When `None`,
/// steps with `when` conditions stay pending until a context is available.
#[tracing::instrument(skip(pool, task, workspace_config))]
pub async fn on_step_completed(
    pool: &PgPool,
    job_id: Uuid,
    step_name: &str,
    task: &TaskDef,
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<()> {
    tracing::info!("Orchestrating after step '{}' completed", step_name);

    // Fetch job_row once — it doesn't change during orchestration, but is only
    // needed when workspace_config is provided (for render context construction).
    let job_row = if workspace_config.is_some() {
        Some(JobRepo::get(pool, job_id).await?.context("Job not found")?)
    } else {
        None
    };

    // 1. Promote pending steps to ready if their dependencies are met.
    //    Loop because conditional skips may cascade and unblock further steps.
    //    Rebuild the render context each iteration so newly-skipped steps are
    //    visible to subsequent `when` condition evaluations.
    //    `for_each` placeholders are deliberately ignored by both
    //    `promote_ready_steps` and `skip_unreachable_steps`; they are resolved
    //    (expanded, skipped, or failed) by `expand_for_each_steps`, which must
    //    therefore run INSIDE this cascade — otherwise a placeholder skipped
    //    because its upstream failed is never seen by the terminal check below
    //    and the job stays `running` forever (prod job 54b3c7b8, 2026-09-02).
    //    Mirrors the creation-time loop in `create_job_for_task_inner`.
    //    Safety bound: each iteration must change at least one step; the bound
    //    is generous to accommodate expansion cascades.
    let max_iterations = task.flow.len() * 2 + 10;
    for _iteration in 0..max_iterations {
        // Rebuild render context from fresh step data each iteration
        let render_ctx = if let Some(ws_config) = workspace_config {
            let steps_snapshot = JobStepRepo::get_steps_for_job(pool, job_id).await?;
            Some(crate::job_creator::build_step_render_context(
                job_row.as_ref().unwrap(),
                &steps_snapshot,
                ws_config,
            ))
        } else {
            None
        };

        // TODO(optimize): promote_ready_steps and skip_unreachable_steps each
        // call get_steps_for_job internally, so this loop issues two separate
        // DB fetches per iteration. A future refactor could load the step list
        // once and pass it into both functions to halve the round-trips.
        let changed =
            JobStepRepo::promote_ready_steps(pool, job_id, &task.flow, render_ctx.as_ref())
                .await
                .context("Failed to promote ready steps")?;

        if !changed.is_empty() {
            tracing::info!("Promoted/skipped steps: {:?}", changed);
        }

        // Skip unreachable pending steps (cascade until stable)
        let skipped = JobStepRepo::skip_unreachable_steps(pool, job_id, &task.flow)
            .await
            .context("Failed to skip unreachable steps")?;

        if !skipped.is_empty() {
            tracing::info!("Skipped unreachable steps: {:?}", skipped);
        }

        // Resolve for_each placeholders whose dependencies are now terminal.
        // Needs a workspace config (for template rendering); without one the
        // placeholders stay pending, same as `when`-conditioned steps.
        let expanded = match (workspace_config, job_row.as_ref()) {
            (Some(ws_config), Some(job)) => crate::job_creator::expand_for_each_steps(
                pool,
                ws_config,
                &job.workspace,
                job_id,
                task,
            )
            .await
            .context("Failed to expand for_each steps")?,
            _ => Vec::new(),
        };

        if !expanded.is_empty() {
            tracing::info!("Expanded/resolved for_each steps: {:?}", expanded);
        }

        // If nothing changed in this iteration, we're stable
        if changed.is_empty() && skipped.is_empty() && expanded.is_empty() {
            break;
        }

        if _iteration + 1 == max_iterations {
            tracing::warn!(
                job_id = %job_id,
                "Cascade loop reached iteration limit ({}) — breaking to avoid infinite loop",
                max_iterations
            );
        }
    }

    // 2. Settle the job if every step is terminal.
    settle_if_all_terminal(pool, job_id, task).await?;
    Ok(())
}

/// If every step of the job is terminal, decide and persist the job's final
/// status and return it; otherwise return `None` and touch nothing.
///
/// Single source of truth for terminal settlement — called from
/// `on_step_completed` AND from job creation (`create_job_for_task_inner`), so
/// a job that is already terminal at creation (all steps skipped by `when`, or
/// a server-dispatched root step that failed) gets exactly the same rules:
/// `continue_on_failure` tolerance, `cancelled` propagation, and aggregated
/// output of the flow's terminal steps.
#[tracing::instrument(skip(pool, task))]
pub async fn settle_if_all_terminal(
    pool: &PgPool,
    job_id: Uuid,
    task: &TaskDef,
) -> Result<Option<JobStatus>> {
    let all_terminal = JobStepRepo::all_steps_terminal(pool, job_id)
        .await
        .context("Failed to check if all steps are terminal")?;
    if !all_terminal {
        return Ok(None);
    }

    // Never overwrite an explicit cancellation.
    if let Some(j) = JobRepo::get(pool, job_id).await? {
        if j.status == JobStatus::Cancelled.as_ref() {
            tracing::info!(
                "Job {} is already cancelled, skipping status update",
                job_id
            );
            return Ok(Some(JobStatus::Cancelled));
        }
    }

    let steps = JobStepRepo::get_steps_for_job(pool, job_id)
        .await
        .context("Failed to get steps for settlement")?;

    // Loop instance steps ("process[0]") are not in task.flow — look up by
    // their placeholder name. Instance failures are already folded into the
    // placeholder by `check_loop_completion`.
    let flow_name = |name: &str| -> String {
        match name.find('[') {
            Some(i) => name[..i].to_string(),
            None => name.to_string(),
        }
    };
    let tolerated = |name: &str| -> bool {
        task.flow
            .get(&flow_name(name))
            .map(|fs| fs.continue_on_failure)
            .unwrap_or(false)
    };

    let untolerated_failure = steps
        .iter()
        .any(|s| s.status == StepStatus::Failed.as_ref() && !tolerated(&s.step_name));
    if untolerated_failure {
        tracing::info!("Job {} failed (one or more steps failed)", job_id);
        JobRepo::mark_failed(pool, job_id)
            .await
            .context("Failed to mark job as failed")?;
        return Ok(Some(JobStatus::Failed));
    }

    let any_cancelled = steps
        .iter()
        .any(|s| s.status == StepStatus::Cancelled.as_ref());
    if any_cancelled {
        tracing::info!(
            "Job {} cancelled (a step was cancelled, no untolerated failure)",
            job_id
        );
        JobRepo::mark_cancelled(pool, job_id)
            .await
            .context("Failed to mark job as cancelled")?;
        return Ok(Some(JobStatus::Cancelled));
    }

    let failed_count = steps
        .iter()
        .filter(|s| s.status == StepStatus::Failed.as_ref())
        .count();
    if failed_count > 0 {
        tracing::info!(
            "Job {} completed with {} tolerable failure(s)",
            job_id,
            failed_count
        );
    } else {
        tracing::info!("Job {} completed successfully", job_id);
    }

    // Output = outputs of the flow's terminal steps (nothing depends on them).
    let depended_on: HashSet<&str> = task
        .flow
        .values()
        .flat_map(|fs| fs.depends_on.iter().map(|s| s.as_str()))
        .collect();
    let terminal_steps: HashSet<&str> = task
        .flow
        .keys()
        .filter(|name| !depended_on.contains(name.as_str()))
        .map(|s| s.as_str())
        .collect();
    let mut job_output = serde_json::Map::new();
    for s in &steps {
        if terminal_steps.contains(s.step_name.as_str()) {
            if let Some(ref output) = s.output {
                job_output.insert(s.step_name.clone(), output.clone());
            }
        }
    }
    let output = if job_output.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(job_output))
    };
    JobRepo::mark_completed(pool, job_id, output)
        .await
        .context("Failed to mark job as completed")?;
    Ok(Some(JobStatus::Completed))
}
