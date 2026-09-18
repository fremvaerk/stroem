//! The workspace source contract (spec § 4.3).

use anyhow::Result;
use std::path::Path;
use stroem_common::budget::LoadBudget;
use stroem_common::models::workflow::WorkspaceConfig;

/// A successful load. Loading mutates no PUBLISHED state — only
/// `WorkspaceEntry::apply_load_result` publishes (spec § 4.6).
#[derive(Debug, Clone)]
pub struct LoadOutcome {
    pub config: WorkspaceConfig,
    pub warnings: Vec<String>,
    pub revision: Option<String>,
}

/// Result of a cheap change check (spec § 4.3).
#[derive(Debug)]
pub enum Peek {
    /// Remote/tree answered authoritatively; compare to the PUBLISHED revision.
    Revision(String),
    /// This source cannot peek; the caller must do a full load.
    Unsupported,
    /// Could not determine the current state — skip, keep the loaded config.
    Failed(anyhow::Error),
    /// Local state is unusable — only a full load can fix it.
    LocalInvalid(anyhow::Error),
}

/// A workspace source. Both `load` and `peek_revision` BLOCK — always call
/// them through `tokio::task::spawn_blocking`, never on a runtime thread.
pub trait WorkspaceSource: Send + Sync {
    /// Load the workspace. Mutates no published state (spec § 4.6).
    fn load(&self, budget: &LoadBudget) -> Result<LoadOutcome>;
    /// Filesystem path where the workspace files reside.
    fn path(&self) -> &Path;
    /// Cheap change check. Default: cannot peek.
    fn peek_revision(&self, _budget: &LoadBudget) -> Peek {
        Peek::Unsupported
    }
    /// Polling interval in seconds for the background watcher.
    fn poll_interval_secs(&self) -> u64 {
        30
    }
}
