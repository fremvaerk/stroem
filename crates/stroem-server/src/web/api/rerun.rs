//! Re-run (spec 2026-10-06-json-input-type D12, git-refs spec § 7.3): the one
//! body behind `POST /api/workspaces/{ws}/tasks/{name}/execute` with a
//! `source_job_id` and the MCP `rerun_job` tool, so the source check order,
//! the pin re-resolution and the create → settle sequence exist once.

use super::caller::Caller;
use super::tasks::{check_rerun_source, require_task_run_as, source_job_not_found};
use crate::acl::TaskPermission;
use crate::input_files::FileGrant;
use crate::job_creator::{create_job_for_rerun, create_job_for_task_pinned, CreationMode};
use crate::state::AppState;
use crate::web::api::{classify_execute_error, get_workspace_or_error};
use crate::web::error::AppError;
use anyhow::Context;
use std::collections::HashMap;
use stroem_common::models::workflow::InputFieldDef;
use stroem_db::{JobRepo, JobRow};
use uuid::Uuid;

/// The task the re-run is of.
#[derive(Clone, Copy)]
pub(crate) enum Destination<'a> {
    /// REST: the route's `{ws}` and `{name}`; they must be the source's.
    Task { workspace: &'a str, task: &'a str },
    /// MCP: the source job's own workspace and task, read only after the
    /// source's existence and `Deny` check.
    Source,
}

/// Which fields take the source job's stored `raw_input` value (each a field
/// of the destination task; one the source has no value for stays absent, so
/// its default applies, or is a 400 when it is required with no default).
#[derive(Clone, Copy)]
pub(crate) enum Replay<'a> {
    /// REST `replay_fields` (spec D12): exactly these.
    Fields(&'a [String]),
    /// Every field of the destination task's input schema that `input` does
    /// not set — a re-run that needs no value it was not given (MCP).
    Unset,
}

impl Replay<'_> {
    fn fields(
        self,
        schema: &HashMap<String, InputFieldDef>,
        input: &serde_json::Value,
    ) -> Vec<String> {
        match self {
            Replay::Fields(fields) => fields.to_vec(),
            Replay::Unset => {
                let mut fields: Vec<String> = schema
                    .keys()
                    .filter(|k| input.get(k.as_str()).is_none())
                    .cloned()
                    .collect();
                fields.sort();
                fields
            }
        }
    }
}

/// Re-run `source_job_id`: a new job of the same task (source type `rerun`,
/// `source_job_id` set) whose `input` is `input` plus the [`Replay`] fields,
/// with `••••••` sentinels in secret and connection fields replayed too.
///
/// Order: 401 (auth configured, no caller) → the source row: missing or ACL
/// `Deny` → the one 404 `Source job not found`, before anything else — then a
/// pinned source takes [`rerun_pinned`]; an unpinned one: workspace 404 →
/// task 404 → `Run` on the live task (Deny → 404 "Task", View → 403
/// "View-only access") → [`check_rerun_source`] (400) → same task (400) →
/// create (errors through [`classify_execute_error`]).
#[tracing::instrument(skip_all, fields(source_job_id = %source_job_id))]
pub(crate) async fn rerun_job(
    state: &AppState,
    caller: Caller<'_>,
    source_job_id: Uuid,
    destination: Destination<'_>,
    input: serde_json::Value,
    replay: Replay<'_>,
) -> Result<Uuid, AppError> {
    if state.config.auth.is_some() && caller.claims.is_none() {
        return Err(AppError::Unauthorized("Authentication required".into()));
    }

    // A missing source and a source the caller may not read (pinned or not)
    // answer identically for every destination (spec
    // 2026-10-06-json-input-type, revision 10).
    let source_job = JobRepo::get(&state.pool, source_job_id)
        .await
        .context("load source job for re-run")?
        .ok_or_else(source_job_not_found)?;
    if matches!(
        super::jobs::job_permission(state, caller, &source_job).await?,
        TaskPermission::Deny
    ) {
        return Err(source_job_not_found());
    }
    let (ws, name) = match destination {
        Destination::Task { workspace, task } => (workspace, task),
        Destination::Source => (source_job.workspace.as_str(), source_job.task_name.as_str()),
    };
    // A PINNED source takes its own path, since its task may exist only at
    // the source's ref.
    if source_job.git_ref.is_some() {
        return rerun_pinned(state, caller, ws, name, input, replay, &source_job).await;
    }

    let workspace = get_workspace_or_error(state, ws).await?;
    let task = workspace
        .tasks
        .get(name)
        .ok_or_else(|| AppError::not_found("Task"))?;
    require_task_run_as(state, caller, ws, name, task.folder.as_deref()).await?;
    check_rerun_source(&source_job, ws)?;
    check_same_task(&source_job, name)?;

    let replay_fields = replay.fields(&task.input, &input);
    let files = FileGrant::direct(state, caller.user_id()?);
    let revision = state.workspaces.get_revision(ws);
    let created = create_job_for_rerun(
        &state.workspaces,
        &state.pool,
        &workspace,
        ws,
        name,
        input,
        "rerun",
        caller.email(),
        revision.as_deref(),
        source_job.job_id,
        &replay_fields,
        state.config.agents.as_ref(),
        state.job_defaults(),
        files,
    )
    .await
    .map_err(classify_execute_error)?;
    let job_id = created.job_id;

    // Root approval steps suspended during creation (FIX 2).
    crate::settlement::dispatch::fire_initial_suspended_hooks(state, job_id).await;
    state.settlement().job_created(created).await;
    Ok(job_id)
}

/// A re-run copies stored values only between runs of one task (spec D12).
fn check_same_task(source_job: &JobRow, name: &str) -> Result<(), AppError> {
    if source_job.task_name != name {
        return Err(AppError::BadRequest(format!(
            "Source job {} is a run of task '{}', not '{}'",
            source_job.job_id, source_job.task_name, name
        )));
    }
    Ok(())
}

/// Re-run of a pinned source (spec § 7.3). The ACL check comes FIRST, so a
/// denied caller learns nothing about the source: `Run` on the SOURCE's path,
/// `{task_folder}/{task}` (§ 7.8), because the task may not exist in the live
/// config at all. Then the source checks of the unpinned path and the task
/// name; then the ref is re-resolved (a branch to its current tip), the task
/// looked up at that commit, and `Run` required on the folder it declares
/// there — the new job's `task_folder` — as the unpinned path requires it on
/// the live task's folder.
///
/// Deny → 404, View → 403 (either check), a bad source or a task missing at
/// the commit → 400, `RefNotFound` → 400, `PinUnavailable` → 500.
async fn rerun_pinned(
    state: &AppState,
    caller: Caller<'_>,
    ws: &str,
    name: &str,
    input: serde_json::Value,
    replay: Replay<'_>,
    source_job: &JobRow,
) -> Result<Uuid, AppError> {
    match super::jobs::job_permission(state, caller, source_job).await? {
        TaskPermission::Deny => return Err(source_job_not_found()),
        TaskPermission::View => return Err(AppError::Forbidden("View-only access".into())),
        TaskPermission::Run => {}
    }
    check_rerun_source(source_job, ws)?;
    check_same_task(source_job, name)?;

    let source_pin = super::pinned_source::resolve_source_pin(state, source_job)
        .await?
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "re-run source {} has no ref to re-resolve",
                source_job.job_id
            ))
        })?;
    let task = source_pin.task(name)?;
    require_task_run_as(state, caller, ws, name, task.folder.as_deref()).await?;
    let replay_fields = replay.fields(&task.input, &input);
    let files = FileGrant::direct(state, caller.user_id()?);

    let created = create_job_for_task_pinned(
        &state.workspaces,
        &state.pool,
        source_pin.handle.config(),
        ws,
        name,
        input,
        "rerun",
        caller.email(),
        &source_pin.pin.commit,
        &source_pin.pin.git_ref,
        CreationMode::Rerun {
            source_job_id: source_job.job_id,
            replay_fields: &replay_fields,
        },
        state.config.agents.as_ref(),
        state.job_defaults(),
        files,
    )
    .await
    .map_err(classify_execute_error)?;
    let job_id = created.job_id;

    crate::settlement::dispatch::fire_initial_suspended_hooks(state, job_id).await;
    state.settlement().job_created(created).await;
    Ok(job_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(names: &[&str]) -> HashMap<String, InputFieldDef> {
        names
            .iter()
            .map(|n| {
                let def: InputFieldDef = serde_yaml::from_str("{type: string}").unwrap();
                (n.to_string(), def)
            })
            .collect()
    }

    #[test]
    fn replay_unset_names_every_schema_field_the_input_does_not_set() {
        let schema = schema(&["b", "a", "c"]);
        let input = json!({"b": "given", "z": "not in the schema"});
        assert_eq!(Replay::Unset.fields(&schema, &input), vec!["a", "c"]);
        // A field given as null counts as given.
        let input = json!({"a": null});
        assert_eq!(Replay::Unset.fields(&schema, &input), vec!["b", "c"]);
        assert!(Replay::Unset.fields(&HashMap::new(), &json!({})).is_empty());
    }

    #[test]
    fn replay_fields_are_passed_as_given() {
        let named = vec!["cfg".to_string()];
        assert_eq!(
            Replay::Fields(&named).fields(&schema(&["a"]), &json!({})),
            vec!["cfg"]
        );
    }
}
