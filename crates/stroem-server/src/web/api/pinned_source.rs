//! Re-run / Restart of a pinned source job (spec 2026-10-02 § 7.3).
//!
//! A pinned source's task may exist only at its ref, so the ref is
//! re-resolved — a branch moves to its current tip, a tag or SHA stays put —
//! BEFORE any task lookup. Unpinned sources keep today's live path.
//!
//! TODO(git-refs): both entry points authorise a pinned source by the SOURCE
//! job's `task_folder` only (`check_job_acl`, § 7.8). The folder the task
//! declares at the re-resolved commit — the new job's `task_folder` — is not
//! checked, so a task moved into a stricter folder on its branch can still be
//! re-run or restarted by a user allowed the old one.

use crate::state::AppState;
use crate::web::error::AppError;
use crate::workspace::pins::{short_sha, PinRef};
use crate::workspace::ConfigHandle;
use stroem_common::models::workflow::TaskDef;
use stroem_db::JobRow;

pub(crate) struct SourcePin {
    pub pin: PinRef,
    pub handle: ConfigHandle,
}

impl SourcePin {
    /// Task `name` of the pinned config. Missing → 400 naming the ref and
    /// the commit it re-resolved to.
    pub(crate) fn task(&self, name: &str) -> Result<&TaskDef, AppError> {
        self.handle.config().tasks.get(name).ok_or_else(|| {
            AppError::BadRequest(format!(
                "Task '{}' does not exist at ref '{}' ({})",
                name,
                self.pin.git_ref,
                short_sha(&self.pin.commit)
            ))
        })
    }
}

/// `Ok(None)` for an unpinned source. A pinned one re-resolves `source.git_ref`
/// and loads that commit's config; a ref or pin error is classified like any
/// creation error (`RefNotFound` → 400, `PinUnavailable` → 500, a
/// `PinLoadFailed` withheld behind its fixed sentence).
#[tracing::instrument(skip_all, fields(job_id = %source.job_id))]
pub(crate) async fn resolve_source_pin(
    state: &AppState,
    source: &JobRow,
) -> Result<Option<SourcePin>, AppError> {
    let Some(git_ref) = source.git_ref.as_deref() else {
        return Ok(None);
    };
    let (pin, pinned) =
        crate::job_creator::pin_at_ref(&state.workspaces, &source.workspace, git_ref)
            .await
            .map_err(super::classify_execute_error)?;
    Ok(Some(SourcePin {
        pin,
        handle: ConfigHandle::Pinned(pinned),
    }))
}
