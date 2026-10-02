//! Resolve a trigger's `task` (+ optional `ref:`) to the workspace, task and
//! config its job is created from, and create that job (spec § 7.5). Shared
//! by the scheduler and the webhook handler.

use crate::job_creator::OwnerConfig;
use crate::settlement::CreatedJob;
use crate::state::AppState;
use crate::workspace::pins::PinRef;
use crate::workspace::{ConfigHandle, WorkspaceManager};
use anyhow::Result;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;

/// Where a trigger fire creates its job.
pub(crate) struct ResolvedTarget {
    /// The task owner `T` — the job's workspace.
    pub workspace: String,
    /// The task's name inside `workspace`.
    pub task_name: String,
    /// `Some` when the trigger carries `ref:` — the job is pinned to it.
    pub pin: Option<PinRef>,
    /// The task's `folder` at the pin, stamped as `job.task_folder`.
    /// `None` for an unpinned target (its ACL path stays live, spec § 7.8).
    pub task_folder: Option<String>,
    /// `T`'s config: the pin's, or `T`'s live config.
    pub config: ConfigHandle,
}

impl ResolvedTarget {
    /// The revision to stamp on the job: the pin's commit, else `T`'s live revision.
    pub fn revision(&self, workspaces: &WorkspaceManager) -> Option<String> {
        match &self.pin {
            Some(pin) => Some(pin.commit.clone()),
            None => workspaces.get_revision(&self.workspace),
        }
    }

    /// The pin columns for the job row (`git_ref`, `task_folder`).
    pub fn pin_cols(&self) -> Option<stroem_db::JobPinCols> {
        self.pin.as_ref().map(|pin| stroem_db::JobPinCols {
            git_ref: pin.git_ref.clone(),
            task_folder: self.task_folder.clone(),
        })
    }
}

/// Spec § 7.5 steps 1–2. Resolution is Task 9's `resolve_task_for_step` with
/// no base pin, so a trigger resolves exactly like a `type: task` reference
/// (local first, then `ws.task`) and, with `git_ref`, like a `ref:`'d one
/// (owner decided syntactically, task looked up at the pin) — with the same
/// error types as job creation. Every error is a MISSED fire for the caller;
/// nothing is written here.
pub(crate) async fn resolve_trigger_target(
    workspaces: &WorkspaceManager,
    defining_ws: &str,
    defining_cfg: &Arc<WorkspaceConfig>,
    task_ref: &str,
    git_ref: Option<&str>,
) -> Result<ResolvedTarget> {
    let (libraries, configured, git) = crate::job_creator::ref_world_sets(workspaces);
    let world = crate::refs::RefWorld {
        library_names: &libraries,
        configured: &configured,
        git: &git,
    };
    let (resolved, pin) = crate::job_creator::resolve_task_for_step(
        workspaces,
        defining_ws,
        defining_cfg,
        None,
        task_ref,
        git_ref,
        &world,
    )
    .await?;
    let task_folder = pin.as_ref().and_then(|_| resolved.task.folder.clone());
    let config = match &pin {
        // `resolve_task_for_step` just ensured this commit; the handle needs
        // the `Pinned` itself, which `ensure` returns from the cache.
        Some(pin) => match workspaces
            .pins()
            .ensure(&resolved.workspace, &pin.commit)
            .await
        {
            Ok(pinned) => ConfigHandle::Pinned(pinned),
            Err(e) => {
                return Err(workspaces
                    .pin_error_for_user(&resolved.workspace, pin, e)
                    .await)
            }
        },
        None => match resolved.config {
            OwnerConfig::Base => ConfigHandle::Live(Arc::clone(defining_cfg)),
            OwnerConfig::Foreign(cfg) => ConfigHandle::Live(cfg),
        },
    };
    Ok(ResolvedTarget {
        workspace: resolved.workspace,
        task_name: resolved.task_name,
        pin,
        task_folder,
        config,
    })
}

/// R4: create the top-level job for a resolved target —
/// `create_job_for_task_pinned` when the target is pinned, else
/// `create_job_for_task_detailed` at the target's live revision. Does NOT
/// call `Settlement::job_created`: callers do, after their own follow-ups
/// (initial `on_suspended` hooks).
#[tracing::instrument(skip_all, fields(workspace = %target.workspace, task = %target.task_name, source_type, source_id))]
pub(crate) async fn create_target_job(
    state: &AppState,
    target: &ResolvedTarget,
    input: serde_json::Value,
    source_type: &str,
    source_id: &str,
) -> Result<CreatedJob> {
    let agents = state.config.agents.as_ref();
    let defaults = crate::config::JobDefaults::from(state.config.as_ref());
    match &target.pin {
        Some(pin) => {
            crate::job_creator::create_job_for_task_pinned(
                &state.workspaces,
                &state.pool,
                target.config.config(),
                &target.workspace,
                &target.task_name,
                input,
                source_type,
                Some(source_id),
                &pin.commit,
                &pin.git_ref,
                crate::job_creator::CreationMode::Normal,
                agents,
                defaults,
            )
            .await
        }
        None => {
            let revision = target.revision(&state.workspaces);
            crate::job_creator::create_job_for_task_detailed(
                &state.workspaces,
                &state.pool,
                target.config.config(),
                &target.workspace,
                &target.task_name,
                input,
                source_type,
                Some(source_id),
                revision.as_deref(),
                None,
                agents,
                defaults,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(pin: Option<PinRef>, folder: Option<&str>) -> ResolvedTarget {
        ResolvedTarget {
            workspace: "billing".to_string(),
            task_name: "nightly".to_string(),
            pin,
            task_folder: folder.map(str::to_string),
            config: ConfigHandle::Live(Arc::new(WorkspaceConfig::new())),
        }
    }

    #[test]
    fn pinned_target_stamps_its_commit_ref_and_folder() {
        let mgr = WorkspaceManager::from_config("billing", WorkspaceConfig::new());
        let commit = "a".repeat(40);
        let t = target(
            Some(PinRef {
                git_ref: "v4.1.0".to_string(),
                commit: commit.clone(),
            }),
            Some("etl"),
        );
        assert_eq!(t.revision(&mgr), Some(commit));
        let cols = t.pin_cols().expect("pinned target has pin columns");
        assert_eq!(cols.git_ref, "v4.1.0");
        assert_eq!(cols.task_folder.as_deref(), Some("etl"));
    }

    #[test]
    fn unpinned_target_uses_the_live_revision_and_no_pin_columns() {
        let mgr = WorkspaceManager::from_configs(vec![(
            "billing".to_string(),
            WorkspaceConfig::new(),
            Some("rev-1".to_string()),
        )]);
        let t = target(None, None);
        assert_eq!(t.revision(&mgr), Some("rev-1".to_string()));
        assert!(t.pin_cols().is_none());
    }

    #[tokio::test]
    async fn ref_on_a_non_git_owner_reports_the_creation_error_type() {
        let mgr = WorkspaceManager::from_config("docs", WorkspaceConfig::new());
        let cfg = Arc::new(WorkspaceConfig::new());
        let err = resolve_trigger_target(&mgr, "docs", &cfg, "t", Some("main"))
            .await
            .err()
            .expect("a folder owner cannot be pinned");
        assert!(matches!(
            err.downcast_ref::<crate::refs::RefResolveError>(),
            Some(crate::refs::RefResolveError::NotGit(ws)) if ws == "docs"
        ));
    }
}
