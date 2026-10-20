//! Trigger run-status view: each trigger's last run, its result and the
//! scheduled runs it missed. `missed_runs` is pure; `attach_run_status` is the
//! DB- and ACL-aware half shared by the trigger list and the task detail.

use crate::acl::{job_coords_task_path, load_user_acl_context, make_task_path, TaskPermission};
use crate::state::AppState;
use crate::web::api::middleware::AuthUser;
use crate::web::api::triggers::{AttentionTrigger, TriggerInfo};
use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use std::collections::HashSet;
use stroem_common::models::workflow::{TriggerDef, WorkspaceConfig};
use stroem_db::{TriggerRunKey, TriggerRunRepo};
use uuid::Uuid;

/// A fire later than this after its scheduled time still counts as on time.
pub const MISSED_GRACE_SECS: i64 = 300;
/// Counting stops here; the UI shows `1000+`.
pub const MISSED_CAP: u32 = 1000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MissedRuns {
    pub count: u32,
    pub capped: bool,
    pub first: DateTime<Utc>,
}

/// A trigger's last run. `status` is the RESULT: the newest task retry's
/// status when the run was retried, else the run's own.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LastRun {
    pub job_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_job_id: Option<Uuid>,
}

/// The requesting user as far as the trigger ACL rules need them.
pub struct Viewer {
    pub email: String,
    pub groups: HashSet<String>,
    pub is_admin: bool,
}

/// `Ok(None)` = no filtering (no auth user or ACL not configured). Errors are
/// never swallowed: a failed ACL-context load must not open the view up.
#[tracing::instrument(skip_all)]
pub async fn viewer_for(
    state: &AppState,
    auth: &Option<AuthUser>,
) -> anyhow::Result<Option<Viewer>> {
    let Some(auth) = auth else {
        return Ok(None);
    };
    if !state.acl.is_configured() {
        return Ok(None);
    }
    let user_id: Uuid = auth
        .claims
        .sub
        .parse()
        .context("Invalid user id in the auth token")?;
    let (is_admin, groups) = load_user_acl_context(&state.pool, user_id, auth.is_admin())
        .await
        .context("Failed to load the ACL context")?;
    Ok(Some(Viewer {
        email: auth.claims.email.clone(),
        groups,
        is_admin,
    }))
}

fn denied(state: &AppState, viewer: &Viewer, workspace: &str, task_path: &str) -> bool {
    matches!(
        state.acl.evaluate(
            workspace,
            task_path,
            &viewer.email,
            &viewer.groups,
            viewer.is_admin
        ),
        TaskPermission::Deny
    )
}

/// A trigger is hidden only when its task (live folder, defining workspace) is `Deny`.
pub fn trigger_visible(
    state: &AppState,
    viewer: Option<&Viewer>,
    workspace: &str,
    config: &WorkspaceConfig,
    task: &str,
) -> bool {
    let Some(viewer) = viewer else {
        return true;
    };
    let folder = config.tasks.get(task).and_then(|t| t.folder.as_deref());
    !denied(state, viewer, workspace, &make_task_path(folder, task))
}

pub struct TriggerStatusItem {
    pub workspace: String,
    pub fires_here: bool,
    pub def: TriggerDef,
    pub info: TriggerInfo,
}

/// One `last_runs` call for all items (any number of workspaces); fills
/// `info.last_run`, `last_run_hidden` and `missed`.
#[tracing::instrument(skip_all, fields(items = items.len()))]
pub async fn attach_run_status(
    state: &AppState,
    viewer: Option<&Viewer>,
    items: &mut [TriggerStatusItem],
) -> anyhow::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let keys: Vec<TriggerRunKey> = items
        .iter()
        .map(|i| TriggerRunKey {
            source_type: source_type_of(&i.def).to_string(),
            source_id: format!("{}/{}", i.workspace, i.info.name),
        })
        .collect();
    let (db_now, rows) = TriggerRunRepo::last_runs(&state.pool, &keys).await?;

    for (item, row) in items.iter_mut().zip(rows) {
        // A trigger with no run other than `skipped` ones shows the newest skipped row.
        let shown = match (row.run_job_id, row.skipped_job_id) {
            (Some(job_id), _) => Some((
                job_id,
                row.run_created_at,
                row.retry_job_id
                    .and(row.retry_status.clone())
                    .or(row.run_status.clone()),
                row.retry_job_id,
                row.run_workspace.as_deref(),
                row.run_task_name.as_deref(),
                row.run_git_ref.as_deref(),
                row.run_task_folder.as_deref(),
            )),
            (None, Some(job_id)) => Some((
                job_id,
                row.skipped_at,
                Some("skipped".to_string()),
                None,
                row.skipped_workspace.as_deref(),
                row.skipped_task_name.as_deref(),
                row.skipped_git_ref.as_deref(),
                row.skipped_task_folder.as_deref(),
            )),
            (None, None) => None,
        };
        if let Some((
            job_id,
            Some(created_at),
            Some(status),
            retry_job_id,
            ws,
            task,
            git_ref,
            folder,
        )) = shown
        {
            let hidden = match (viewer, ws, task) {
                (Some(viewer), Some(ws), Some(task)) => {
                    let path = job_coords_task_path(state, ws, task, git_ref, folder).await;
                    denied(state, viewer, ws, &path)
                }
                _ => false,
            };
            if hidden {
                item.info.last_run_hidden = true;
            } else {
                item.info.last_run = Some(LastRun {
                    job_id,
                    created_at,
                    status,
                    retry_job_id,
                });
            }
        }

        if let (true, TriggerDef::Scheduler { cron, timezone, .. }) = (item.fires_here, &item.def) {
            let last_fire = [row.run_created_at, row.skipped_at]
                .into_iter()
                .flatten()
                .max();
            if let Some(last_fire) = last_fire {
                item.info.missed = missed_runs(cron, timezone.as_deref(), last_fire, db_now);
            }
        }
    }
    Ok(())
}

/// The last run failed and the viewer may see it. A hidden run never counts.
pub fn last_run_failed(item: &TriggerStatusItem) -> bool {
    !item.info.last_run_hidden
        && item
            .info
            .last_run
            .as_ref()
            .is_some_and(|r| r.status == "failed")
}

/// A trigger that fires here and either missed runs or last failed.
pub fn needs_attention(item: &TriggerStatusItem) -> bool {
    item.fires_here && (item.info.missed.is_some() || last_run_failed(item))
}

/// Missed first (most missed first), then the rest by newest last run; ties by
/// workspace and name.
pub fn sort_attention(items: &mut [AttentionTrigger]) {
    items.sort_by(|a, b| {
        let a_missed = a.trigger.missed.as_ref().map(|m| m.count);
        let b_missed = b.trigger.missed.as_ref().map(|m| m.count);
        let a_run = a.trigger.last_run.as_ref().map(|r| r.created_at);
        let b_run = b.trigger.last_run.as_ref().map(|r| r.created_at);
        match (a_missed, b_missed) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b_run.cmp(&a_run),
        }
        .then_with(|| (&a.workspace, &a.trigger.name).cmp(&(&b.workspace, &b.trigger.name)))
    });
}

/// The `job.source_type` a trigger's jobs carry.
pub fn source_type_of(def: &TriggerDef) -> &'static str {
    match def {
        TriggerDef::Scheduler { .. } => "trigger",
        TriggerDef::Webhook { .. } => "webhook",
        TriggerDef::EventSource { .. } => "event_source",
    }
}

/// Scheduled occurrences `t` with `last_fire < t <= now - grace`.
/// `None` for an unparsable cron or when there is none.
pub fn missed_runs(
    cron: &str,
    timezone: Option<&str>,
    last_fire: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<MissedRuns> {
    let cron = stroem_common::cron::parse(cron).ok()?;
    let tz: Tz = timezone
        .and_then(|s| s.parse().ok())
        .unwrap_or(chrono_tz::UTC);
    let limit = now - Duration::seconds(MISSED_GRACE_SECS);

    let mut prev = last_fire;
    let mut first = None;
    let mut count: u32 = 0;
    for dt in cron.iter_after(last_fire.with_timezone(&tz)) {
        let utc = dt.with_timezone(&Utc);
        // A DST gap or overlap can repeat or reorder local times.
        if utc <= prev {
            continue;
        }
        if utc > limit {
            break;
        }
        if count == MISSED_CAP {
            return Some(MissedRuns {
                count,
                capped: true,
                first: first?,
            });
        }
        first.get_or_insert(utc);
        count += 1;
        prev = utc;
    }
    first.map(|first| MissedRuns {
        count,
        capped: false,
        first,
    })
}

#[cfg(test)]
mod tests;
