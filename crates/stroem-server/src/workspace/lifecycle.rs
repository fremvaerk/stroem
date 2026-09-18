//! Worker / finalizer / observer execution of loads and peeks (spec § 4.5).
//!
//! - worker: `spawn_blocking` — the only layer that may block, hang or panic;
//! - finalizer: a detached task that owns the execution guard (and permit),
//!   applies the result through the single writer, then releases, then
//!   notifies. Cancelling whoever awaits it never cancels it;
//! - observer: the caller, which may stop waiting.

use super::availability::{Caller, Event, Policy};
use super::entry::{LoadSuccess, ReloadState, WorkspaceEntry};
use super::library::ResolvedLibrary;
use super::source::Peek;
use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stroem_common::budget::LoadBudget;
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit};
use tokio::task::JoinHandle;

/// A reload could not start: another load of this workspace holds its
/// execution mutex. Never a load outcome (spec § 4.5 (2), (8)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadBusy;

impl std::fmt::Display for ReloadBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a reload of this workspace is already in progress")
    }
}

impl std::error::Error for ReloadBusy {}

/// Announces a successful watcher reload to peer replicas — a seam so tests
/// can stall it.
pub trait ReloadNotifier: Send + Sync {
    fn notify(&self, workspace: String) -> BoxFuture<'static, ()>;
}

impl ReloadNotifier for crate::events::EventBus {
    fn notify(&self, workspace: String) -> BoxFuture<'static, ()> {
        let bus = self.clone();
        Box::pin(async move { bus.publish_workspace_reloaded(&workspace).await })
    }
}

pub(crate) struct LoadRequest {
    pub entry: Arc<WorkspaceEntry>,
    pub libs: Arc<HashMap<String, ResolvedLibrary>>,
    pub policy: Policy,
    pub guard: OwnedMutexGuard<ReloadState>,
    pub permit: Option<OwnedSemaphorePermit>,
    pub caller: Caller,
    pub op_id: Option<u64>,
    pub budget: LoadBudget,
    pub notifier: Option<Arc<dyn ReloadNotifier>>,
}

/// Start a load. The returned handle is the FINALIZER's: dropping it, or
/// timing out on it, never cancels the load or releases its guard early.
pub(crate) fn spawn_load(req: LoadRequest) -> JoinHandle<Result<LoadSuccess>> {
    tokio::spawn(async move {
        let LoadRequest {
            entry,
            libs,
            policy,
            mut guard,
            permit,
            caller,
            op_id,
            budget,
            notifier,
        } = req;

        // 1. worker
        let source = Arc::clone(&entry.source);
        let outcome = match tokio::task::spawn_blocking(move || source.load(&budget)).await {
            Ok(result) => result,
            Err(join_err) => Err(anyhow!("workspace load panicked: {join_err}")),
        };

        // 2. the single writer
        let completed_at = Instant::now();
        let result = entry.apply_load_result(caller, op_id, outcome, &libs, &policy, completed_at);

        // 3. API cooldown bookkeeping, while the guard is still held
        if caller == Caller::External {
            guard.last_completed = Some(completed_at);
        }

        // 4. release BEFORE notifying — a stalled notification holds nothing
        drop(guard);
        drop(permit);

        // 5. best-effort, detached peer notification (today's rule: a
        //    successful watcher load that changed the revision)
        if let (Some(notifier), Ok(success)) = (notifier, &result) {
            if caller == Caller::Watcher && success.revision_changed {
                tokio::spawn(notifier.notify(entry.name.clone()));
            }
        }
        result
    })
}

/// Start a peek. The finalizer ALWAYS emits `PeekFinished` (clearing the
/// in-flight record), even after a panic; only the observer applies outcomes.
#[allow(dead_code)] // wired by the watcher (Task 12)
pub(crate) fn spawn_peek(
    entry: Arc<WorkspaceEntry>,
    policy: Policy,
    op_id: u64,
    budget: LoadBudget,
) -> JoinHandle<Peek> {
    tokio::spawn(async move {
        let source = Arc::clone(&entry.source);
        let peek = match tokio::task::spawn_blocking(move || source.peek_revision(&budget)).await {
            Ok(peek) => peek,
            Err(join_err) => Peek::Failed(anyhow!("workspace peek panicked: {join_err}")),
        };
        entry.transition(Event::PeekFinished { op_id }, &policy);
        peek
    })
}

/// Deterministic per-workspace watcher start offset in `[0, poll)`, so N
/// watchers do not tick together (spec § 4.4).
#[allow(dead_code)] // wired by the watcher (Task 12)
pub(crate) fn jitter_offset(name: &str, poll: Duration) -> Duration {
    let millis = poll.as_millis() as u64;
    if millis == 0 {
        return Duration::ZERO;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    Duration::from_millis(hasher.finish() % millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_offsets_differ_and_stay_below_the_poll_interval() {
        let poll = Duration::from_secs(60);
        let offsets: Vec<_> = ["a", "b", "jobs", "jobs_beta", "ai_traffic_model"]
            .iter()
            .map(|n| jitter_offset(n, poll))
            .collect();
        assert!(offsets.iter().all(|o| *o < poll));
        let unique: std::collections::HashSet<_> = offsets.iter().collect();
        assert!(unique.len() >= 4, "offsets should spread out: {offsets:?}");
        assert_eq!(
            jitter_offset("a", poll),
            jitter_offset("a", poll),
            "deterministic"
        );
        assert_eq!(jitter_offset("a", Duration::ZERO), Duration::ZERO);
    }
}
