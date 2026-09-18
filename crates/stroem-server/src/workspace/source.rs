//! The workspace source contract (spec § 4.3).

use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use stroem_common::models::workflow::WorkspaceConfig;

/// A successful load. Loading mutates no PUBLISHED state — only
/// `WorkspaceEntry::apply_load_result` publishes (spec § 4.6).
#[derive(Debug, Clone)]
pub struct LoadOutcome {
    pub config: WorkspaceConfig,
    pub warnings: Vec<String>,
    pub revision: Option<String>,
}

/// Trait for workspace sources (folder, git, etc.)
#[async_trait]
pub trait WorkspaceSource: Send + Sync {
    /// Load/reload workspace configuration from this source.
    /// Returns the config paired with per-file warnings for files that were
    /// skipped due to read or parse errors.
    async fn load(&self) -> Result<(WorkspaceConfig, Vec<String>)>;
    /// Filesystem path where the workspace files reside
    fn path(&self) -> &Path;
    /// Current revision identifier (content hash for folder, git OID for git)
    fn revision(&self) -> Option<String>;
    /// Compute the current revision without a full load.
    /// Used by the watcher to cheaply detect changes before doing expensive YAML parsing.
    /// Default implementation returns `None` (forces a full reload every cycle).
    fn peek_revision(&self) -> Option<String> {
        None
    }
    /// Polling interval in seconds for the background watcher.
    /// Default is 30 seconds. Git sources override with their configured value.
    fn poll_interval_secs(&self) -> u64 {
        30
    }
}
