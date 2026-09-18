//! One workspace's state, split by how long each part is held (spec § 4.4):
//! the execution mutex (whole load), `Availability` (microseconds) and the
//! published snapshot (clone/swap an `Arc`). Readers touch ONLY the snapshot.

use super::availability::{transition, Availability, Caller, Effect, Event, Policy};
use super::library::{merge_library_into_workspace, ResolvedLibrary};
use super::source::{LoadOutcome, WorkspaceSource};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use stroem_common::models::workflow::WorkspaceConfig;

/// Guarded by the execution mutex. `last_completed` drives the API refresh
/// cooldown and is stamped by external loads only.
#[derive(Default)]
pub struct ReloadState {
    pub last_completed: Option<Instant>,
}

/// What readers serve, swapped as one `Arc` (spec § 4.6).
#[derive(Debug, Clone)]
pub struct Published {
    pub config: Arc<WorkspaceConfig>,
    pub revision: Option<String>,
    pub warnings: Vec<String>,
    /// `Some` ⇔ the workspace is unavailable (last load failed).
    pub error: Option<String>,
    pub loaded_at: Option<Instant>,
    pub loaded_at_utc: Option<DateTime<Utc>>,
}

/// Outcome of a successful load, as seen by its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSuccess {
    pub revision_changed: bool,
}

pub struct WorkspaceEntry {
    pub name: String,
    pub source: Arc<dyn WorkspaceSource>,
    pub source_path: PathBuf,
    published: RwLock<Arc<Published>>,
    availability: Mutex<Availability>,
    /// Serializes loads on one checkout. Held for a whole load — by the load's
    /// finalizer, until the worker really returns. Nobody waits on it.
    exec: Arc<tokio::sync::Mutex<ReloadState>>,
    /// Source of `InFlight::op_id` for peeks/loads this entry starts.
    op_seq: AtomicU64,
}

impl WorkspaceEntry {
    /// A healthy entry serving `config` at `revision` (tests and in-memory sources).
    pub fn new(
        name: impl Into<String>,
        source: Arc<dyn WorkspaceSource>,
        config: WorkspaceConfig,
        revision: Option<String>,
    ) -> Self {
        Self::loaded(name.into(), source, config, Vec::new(), revision)
    }

    pub(crate) fn loaded(
        name: String,
        source: Arc<dyn WorkspaceSource>,
        config: WorkspaceConfig,
        warnings: Vec<String>,
        revision: Option<String>,
    ) -> Self {
        let published = Published {
            config: Arc::new(config),
            revision,
            warnings,
            error: None,
            loaded_at: Some(Instant::now()),
            loaded_at_utc: Some(Utc::now()),
        };
        Self::build(name, source, published, Availability::fresh())
    }

    pub(crate) fn startup_failed(
        name: String,
        source: Arc<dyn WorkspaceSource>,
        error: String,
        policy: &Policy,
    ) -> Self {
        let published = Published {
            config: Arc::new(WorkspaceConfig::new()),
            revision: None,
            warnings: Vec::new(),
            error: Some(error),
            loaded_at: None,
            loaded_at_utc: None,
        };
        Self::build(
            name,
            source,
            published,
            Availability::startup_failed(Instant::now(), policy),
        )
    }

    fn build(
        name: String,
        source: Arc<dyn WorkspaceSource>,
        published: Published,
        availability: Availability,
    ) -> Self {
        let source_path = source.path().to_path_buf();
        Self {
            name,
            source,
            source_path,
            published: RwLock::new(Arc::new(published)),
            availability: Mutex::new(availability),
            exec: Arc::new(tokio::sync::Mutex::new(ReloadState::default())),
            op_seq: AtomicU64::new(0),
        }
    }

    pub fn published(&self) -> Arc<Published> {
        Arc::clone(&self.published.read().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn is_healthy(&self) -> bool {
        self.published().error.is_none()
    }

    pub fn availability(&self) -> Availability {
        *self.availability.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn exec(&self) -> Arc<tokio::sync::Mutex<ReloadState>> {
        Arc::clone(&self.exec)
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.source.poll_interval_secs().max(1))
    }

    /// Drives the watcher's peek/load state machine (`Event::Tick`/`Event::Peek*`).
    pub(crate) fn transition(&self, event: Event, policy: &Policy) -> Effect {
        let mut a = self.availability.lock().unwrap_or_else(|e| e.into_inner());
        transition(&mut a, event, policy)
    }

    pub(crate) fn next_op_id(&self) -> u64 {
        self.op_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The ONLY writer of the published snapshot and of load-completion
    /// availability (spec § 4.4). Every load path calls it — watcher,
    /// external callers, startup. A failed load publishes nothing new: the
    /// previous config and revision stay, hidden behind `error`.
    pub(crate) fn apply_load_result(
        &self,
        caller: Caller,
        op_id: Option<u64>,
        result: Result<LoadOutcome>,
        libs: &HashMap<String, ResolvedLibrary>,
        policy: &Policy,
        completed_at: Instant,
    ) -> Result<LoadSuccess> {
        // Lock order: availability, then published. Readers take only the
        // published read lock, so this cannot deadlock.
        let mut availability = self.availability.lock().unwrap_or_else(|e| e.into_inner());
        let mut published = self.published.write().unwrap_or_else(|e| e.into_inner());
        let event = Event::LoadCompleted {
            op_id,
            caller,
            ok: result.is_ok(),
            completed_at,
        };
        transition(&mut availability, event, policy);
        match result {
            Ok(LoadOutcome {
                mut config,
                warnings,
                revision,
            }) => {
                for lib in libs.values() {
                    merge_library_into_workspace(&mut config, lib);
                }
                if !warnings.is_empty() {
                    tracing::warn!(
                        "Workspace '{}': {} file(s) skipped due to errors",
                        self.name,
                        warnings.len()
                    );
                }
                let revision_changed = published.revision != revision;
                *published = Arc::new(Published {
                    config: Arc::new(config),
                    revision,
                    warnings,
                    error: None,
                    loaded_at: Some(completed_at),
                    loaded_at_utc: Some(Utc::now()),
                });
                Ok(LoadSuccess { revision_changed })
            }
            Err(e) => {
                let previous = Arc::clone(&published);
                *published = Arc::new(Published {
                    config: Arc::clone(&previous.config),
                    revision: previous.revision.clone(),
                    warnings: Vec::new(),
                    error: Some(format!("{e:#}")),
                    loaded_at: previous.loaded_at,
                    loaded_at_utc: previous.loaded_at_utc,
                });
                Err(e)
            }
        }
    }

    /// Test support: swap the config, keeping revision and health.
    pub(crate) fn replace_config(&self, config: WorkspaceConfig) {
        let mut published = self.published.write().unwrap_or_else(|e| e.into_inner());
        let mut next = (**published).clone();
        next.config = Arc::new(config);
        *published = Arc::new(next);
    }
}

impl std::fmt::Debug for WorkspaceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("WorkspaceEntry");
        s.field("name", &self.name)
            .field("source_path", &self.source_path);
        if let Some(err) = &self.published().error {
            s.field("load_error", err);
        }
        s.finish()
    }
}
