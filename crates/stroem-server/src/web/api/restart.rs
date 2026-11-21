//! Restart From Step (spec 2026-09-07 § 6.1) and Restart Failed Steps (spec
//! 2026-11-02): the one body behind `POST /api/jobs/{id}/restart` and the MCP
//! `restart_job` tool, so the check order, the pin re-resolution and the
//! plan → create → settle sequence exist once.

use super::caller::Caller;
use crate::restart::{RestartFrom, RestartPlan};
use crate::state::AppState;
use crate::web::error::AppError;
use anyhow::Context;
use serde_json::json;
use stroem_common::models::job::JobStatus;
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

/// The restart mode from the request shape: exactly one of `from_step` and
/// `all_failed: true` (spec 2026-11-02 § 5.1), else 400. Both REST and MCP
/// check it before anything else, the job lookup included.
pub(crate) fn restart_mode(
    from_step: Option<String>,
    all_failed: bool,
) -> Result<RestartFrom, AppError> {
    match (from_step, all_failed) {
        (Some(step), false) => Ok(RestartFrom::Step(step)),
        (None, true) => Ok(RestartFrom::AllFailed),
        _ => Err(AppError::BadRequest(
            "Specify exactly one of from_step or all_failed".into(),
        )),
    }
}

/// What a restart planned and, unless it was a dry run, the job it created.
pub(crate) struct RestartOutcome {
    /// The new job; `None` for a dry run.
    pub job_id: Option<Uuid>,
    plan: RestartPlan,
}

impl RestartOutcome {
    /// The response body, the same for REST and MCP: `job_id` (not for a dry
    /// run), `start_steps`, `restart_steps`, `carried_over`, `carried_failed`,
    /// `carried_failed_tolerated`.
    pub(crate) fn body(&self) -> serde_json::Value {
        let plan = &self.plan;
        let carried: Vec<&str> = plan.carried.iter().map(|s| s.step_name.as_str()).collect();
        match self.job_id {
            Some(id) => json!({
                "job_id": id.to_string(),
                "start_steps": plan.start_steps,
                "restart_steps": plan.restart_steps,
                "carried_over": carried,
                "carried_failed": plan.carried_failed,
                "carried_failed_tolerated": plan.carried_failed_tolerated,
            }),
            None => json!({
                "start_steps": plan.start_steps,
                "restart_steps": plan.restart_steps,
                "carried_over": carried,
                "carried_failed": plan.carried_failed,
                "carried_failed_tolerated": plan.carried_failed_tolerated,
            }),
        }
    }
}

/// Restart `job_id` from `from` (spec 2026-11-02).
///
/// Creates a new job for the source job's task in which the start steps —
/// `from_step`, or with `all_failed` every failed or cancelled step — and
/// everything below them rerun, while everything else is carried over from
/// the source as it ended. `dry_run` returns the same plan without creating
/// anything, so a confirm dialog can be authoritative about steps added to the
/// flow since the source job ran.
///
/// The caller checked the request shape ([`restart_mode`]) first. Then checks
/// run in the spec's order: 401 (auth configured, no caller) → 404 (job
/// missing or ACL `Deny`) → 403 (ACL `View`) → 409 (source not terminal) → 400
/// (not top-level, legacy source, workspace/task gone, bad `from_step`).
///
/// A pinned source (git-refs spec § 7.3) is authorised by its own
/// `task_folder` (§ 7.8) and re-resolves its ref BEFORE the task lookup, so
/// the plan uses the flow at the commit the restart will run; `Run` is then
/// also required on the folder the task declares at that commit (404 "Task
/// not found" / 403 "View-only access", like the execute route), `dry_run`
/// included. That adds: 400 when the ref no longer resolves (`RefNotFound`) or
/// its config does not load (`PinLoadFailed`, withheld), 400 when the task
/// does not exist at the re-resolved commit, and 500 when the git remote is
/// unavailable (`PinUnavailable`).
#[tracing::instrument(skip_all, fields(job_id = %job_id, dry_run))]
pub(crate) async fn restart_job(
    state: &AppState,
    caller: Caller<'_>,
    job_id: Uuid,
    restart_from: RestartFrom,
    dry_run: bool,
) -> Result<RestartOutcome, AppError> {
    // Explicit 401: the ACL checks below grant `Run` when there is no caller,
    // so a missing caller under auth must be refused first (mirrors execute).
    if state.config.auth.is_some() && caller.claims.is_none() {
        return Err(AppError::Unauthorized("Authentication required".into()));
    }
    let source_id = caller.email();

    let source = JobRepo::get(&state.pool, job_id)
        .await
        .context("get job")?
        .ok_or_else(|| AppError::not_found("Job"))?;

    // Restart creates a job, so it needs Run — like execute and cancel.
    match super::jobs::job_permission(state, caller, &source).await? {
        crate::acl::TaskPermission::Deny => return Err(AppError::not_found("Job")),
        crate::acl::TaskPermission::View => {
            return Err(AppError::Forbidden(
                "Insufficient permissions to restart this job".into(),
            ))
        }
        crate::acl::TaskPermission::Run => {}
    }

    let terminal = matches!(
        source.status.parse::<JobStatus>(),
        Ok(JobStatus::Completed)
            | Ok(JobStatus::Failed)
            | Ok(JobStatus::Cancelled)
            | Ok(JobStatus::Skipped)
    );
    if !terminal {
        return Err(AppError::Conflict("Job is not in a terminal state".into()));
    }
    if !super::jobs::is_top_level_job(&source) {
        return Err(AppError::BadRequest(
            "Only top-level jobs can be restarted".into(),
        ));
    }
    if source.raw_input.is_none() {
        return Err(AppError::BadRequest(
            "Source job predates Re-run prefill (no raw_input)".into(),
        ));
    }

    // Spec § 7.3: a pinned source re-resolves its ref BEFORE the task lookup —
    // the task may exist only at the ref, and the plan must be computed
    // against the flow of the commit the restart will run.
    let source_pin = super::pinned_source::resolve_source_pin(state, &source).await?;
    let live_workspace;
    let (workspace, task) = match &source_pin {
        Some(sp) => {
            let task = sp.task(&source.task_name)?;
            // Final review I3: the new job is stamped with the folder the task
            // declares at the re-resolved commit, so `Run` is required there
            // too (the source check above covers only the source's folder).
            super::tasks::require_task_run_as(
                state,
                caller,
                &source.workspace,
                &source.task_name,
                task.folder.as_deref(),
            )
            .await?;
            (sp.handle.config(), task)
        }
        None => {
            live_workspace = state
                .get_workspace(&source.workspace)
                .await
                .ok_or_else(|| {
                    AppError::BadRequest(format!("Workspace '{}' is not loaded", source.workspace))
                })?;
            let task = live_workspace.tasks.get(&source.task_name).ok_or_else(|| {
                AppError::BadRequest(format!(
                    "Task '{}' no longer exists in workspace '{}'",
                    source.task_name, source.workspace
                ))
            })?;
            (live_workspace.as_ref(), task)
        }
    };

    let source_steps = JobStepRepo::get_steps_for_job(&state.pool, job_id)
        .await
        .context("get source steps")?;
    let plan =
        crate::restart::plan_restart(&task.flow, &source_steps, &restart_from).map_err(|e| {
            AppError::BadRequest(match e {
                crate::restart::RestartError::UnknownStep(s) => format!(
                    "Step '{}' is not in the current flow of task '{}'",
                    s, source.task_name
                ),
                crate::restart::RestartError::NoFailedSteps => format!(
                    "No failed or cancelled steps to restart in the current flow of task '{}'",
                    source.task_name
                ),
                other => other.to_string(),
            })
        })?;

    if dry_run {
        return Ok(RestartOutcome { job_id: None, plan });
    }

    let revision = match &source_pin {
        Some(sp) => Some(sp.pin.commit.clone()),
        None => state.workspaces.get_revision(&source.workspace),
    };
    let files = crate::input_files::FileGrant::direct(state, caller.user_id()?);
    let created = crate::job_creator::create_restart_job(
        &state.workspaces,
        &state.pool,
        workspace,
        &source.workspace,
        &source,
        &plan,
        source_id,
        revision.as_deref(),
        state.job_defaults(),
        source_pin.as_ref().map(|sp| sp.pin.git_ref.as_str()),
        files,
    )
    .await
    .map_err(super::classify_execute_error)?;

    // Root approval steps suspended during creation, then terminal handling for
    // a restart whose set cascade-skipped and settled on the spot.
    crate::settlement::dispatch::fire_initial_suspended_hooks(state, created.job_id).await;
    let new_job_id = created.job_id;
    state.settlement().job_created(created).await;

    Ok(RestartOutcome {
        job_id: Some(new_job_id),
        plan,
    })
}
