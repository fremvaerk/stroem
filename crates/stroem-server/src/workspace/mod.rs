pub mod availability;
pub mod entry;
pub mod folder;
pub mod git;
pub mod library;
pub mod source;

pub use entry::{LoadSuccess, Published, ReloadState, WorkspaceEntry};
pub use folder::load_folder_workspace;
pub use library::{merge_library_into_workspace, LibraryResolver, ResolvedLibrary};
pub use source::{LoadOutcome, WorkspaceSource};

use anyhow::{Context, Result};
use async_trait::async_trait;
use availability::{Caller, ReloadSettings};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stroem_common::models::workflow::WorkspaceConfig;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::{GitAuthConfig, LibraryDef, WorkspaceSourceDef};

/// Upper bound on how many workspace sources `WorkspaceManager::new` loads at
/// once. Git sources block a whole worker thread each (see the comment in
/// `new()`), so this also caps how many worker threads a large workspace
/// fleet can occupy at startup.
const MAX_CONCURRENT_WORKSPACE_LOADS: usize = 8;

/// Outcome of `WorkspaceManager::reload_for_api`. Distinguishes the three
/// cases the public refresh endpoint cares about so the handler can return
/// 404 / 429 / 500 without sniffing error strings.
#[derive(Debug)]
pub enum ReloadApiError {
    /// Workspace name does not exist (or is ACL-hidden — the handler chooses
    /// when to return this).
    NotFound,
    /// Last reload completed less than `cooldown` ago; client should retry
    /// after `retry_after_secs`.
    Cooldown { retry_after_secs: u64 },
    /// The reload was attempted and failed.
    Failed(anyhow::Error),
}

impl std::fmt::Display for ReloadApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "workspace not found"),
            Self::Cooldown { retry_after_secs } => {
                write!(f, "refresh rate-limited; retry after {retry_after_secs}s")
            }
            Self::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ReloadApiError {}

/// In-memory workspace source for testing
struct InMemorySource {
    config: tokio::sync::RwLock<WorkspaceConfig>,
    /// Optional pinned revision. `None` mirrors the historic behaviour of
    /// `from_config`; `from_configs` can supply an explicit revision so tests
    /// exercising cross-workspace pinning observe a concrete value.
    revision: Option<String>,
}

#[async_trait]
impl WorkspaceSource for InMemorySource {
    async fn load(&self) -> Result<(WorkspaceConfig, Vec<String>)> {
        Ok((self.config.read().await.clone(), Vec::new()))
    }

    fn path(&self) -> &Path {
        Path::new("/dev/null")
    }

    fn revision(&self) -> Option<String> {
        self.revision.clone()
    }

    fn peek_revision(&self) -> Option<String> {
        None
    }
}

/// Manages multiple workspaces
#[derive(Debug)]
pub struct WorkspaceManager {
    entries: HashMap<String, Arc<WorkspaceEntry>>,
    load_errors: HashMap<String, String>,
    /// Resolved libraries — shared across all workspaces and load tasks.
    resolved_libraries: Arc<HashMap<String, ResolvedLibrary>>,
    /// Workspaces whose triggers this server must NOT fire
    /// (`workspaces.<name>.triggers: false` in the server config). Kept
    /// separate from `entries` so it also covers workspaces whose source
    /// failed to construct, and so test constructors need not thread it.
    triggers_disabled: HashSet<String>,
    settings: ReloadSettings,
}

impl WorkspaceManager {
    /// Create a new WorkspaceManager from config definitions.
    /// Individual workspace failures are captured in `load_errors` rather than
    /// failing the entire server — other workspaces continue to load normally.
    /// Libraries are resolved first and merged into every workspace config.
    pub async fn new(
        defs: HashMap<String, WorkspaceSourceDef>,
        library_defs: HashMap<String, LibraryDef>,
        git_auth: HashMap<String, GitAuthConfig>,
    ) -> Self {
        let settings = ReloadSettings::default();
        let mut entries = HashMap::new();
        let mut load_errors = HashMap::new();

        // Resolve libraries (git clones are blocking I/O — use block_in_place)
        let resolved_libraries = if !library_defs.is_empty() {
            let cache_dir = std::env::temp_dir().join("stroem").join("libraries");
            let resolver = LibraryResolver::new(cache_dir, git_auth);
            tokio::task::block_in_place(|| match resolver.resolve_all(&library_defs) {
                Ok(libs) => {
                    tracing::info!("Resolved {} library/libraries", libs.len());
                    libs
                }
                Err(e) => {
                    tracing::error!("Failed to resolve libraries: {:#}", e);
                    HashMap::new()
                }
            })
        } else {
            HashMap::new()
        };

        let total_configured = defs.len();
        let start = Instant::now();

        // Construct every source up front. Construction failures (e.g. a
        // malformed git config) go straight into `load_errors`, same as
        // before — they never reach the concurrent load step below.
        let mut sources: Vec<(String, Arc<dyn WorkspaceSource>)> = Vec::with_capacity(defs.len());
        let mut triggers_disabled = HashSet::new();
        for (name, def) in defs {
            if !def.triggers_enabled() {
                tracing::info!(
                    "Workspace '{}': triggers disabled by server config — \
                     schedules, webhooks and event sources will not fire here",
                    name
                );
                triggers_disabled.insert(name.clone());
            }
            let source: Arc<dyn WorkspaceSource> = match def {
                WorkspaceSourceDef::Folder { ref path, .. } => {
                    Arc::new(folder::FolderSource::new(path))
                }
                WorkspaceSourceDef::Git {
                    ref url,
                    ref git_ref,
                    poll_interval_secs,
                    ref auth,
                    ..
                } => {
                    match git::GitSource::new(&name, url, git_ref, auth.clone(), poll_interval_secs)
                    {
                        Ok(s) => Arc::new(s),
                        Err(e) => {
                            tracing::error!("Failed to init workspace '{}': {:#}", name, e);
                            load_errors.insert(name, format!("{:#}", e));
                            continue;
                        }
                    }
                }
            };
            sources.push((name, source));
        }

        // Load every workspace concurrently. `tokio::spawn` (via `JoinSet`)
        // is required here rather than `join_all` over the loading futures:
        // `GitSource::load` wraps its blocking libgit2 clone/fetch in
        // `tokio::task::block_in_place`, which hands the *current* worker
        // thread over to blocking work for the duration of the call — it
        // does not yield that thread back to the runtime for other tasks to
        // use. Polling several such futures on one task (as `join_all`
        // would) still runs them one at a time; only separate spawned tasks,
        // each occupying its own worker thread, actually run the blocking
        // git operations in parallel. Without this, a single slow or
        // misbehaving remote would still stall every other workspace's
        // startup, exactly as it did before this change.
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_WORKSPACE_LOADS));
        let mut join_set = JoinSet::new();
        let mut task_names: HashMap<tokio::task::Id, String> =
            HashMap::with_capacity(sources.len());
        for (name, source) in sources {
            let semaphore = semaphore.clone();
            let abort_handle = join_set.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("semaphore is never closed");
                let result = source.load().await;
                (source, result)
            });
            task_names.insert(abort_handle.id(), name);
        }

        type LoadedWorkspace = (
            String,
            Arc<dyn WorkspaceSource>,
            Result<(WorkspaceConfig, Vec<String>)>,
        );
        let mut loaded: Vec<LoadedWorkspace> = Vec::with_capacity(task_names.len());
        while let Some(joined) = join_set.join_next_with_id().await {
            match joined {
                Ok((id, (source, result))) => {
                    let name = task_names
                        .remove(&id)
                        .unwrap_or_else(|| "<unknown>".to_string());
                    loaded.push((name, source, result));
                }
                Err(join_err) => {
                    // A panicked load task must not take the server down.
                    // The source itself is gone (it was moved into the
                    // unwound task), so this workspace gets no `entries`
                    // row — same shape as a source-construction failure.
                    let name = task_names
                        .remove(&join_err.id())
                        .unwrap_or_else(|| "<unknown>".to_string());
                    tracing::error!("Workspace '{}' load task panicked: {}", name, join_err);
                    load_errors.insert(name, format!("workspace load task panicked: {join_err}"));
                }
            }
        }

        for (name, source, result) in loaded {
            let entry = match result {
                Ok((mut config, warnings)) => {
                    for lib in resolved_libraries.values() {
                        merge_library_into_workspace(&mut config, lib);
                    }
                    if !warnings.is_empty() {
                        tracing::warn!(
                            "Workspace '{}': {} file(s) skipped due to errors",
                            name,
                            warnings.len()
                        );
                    }
                    let revision = source.revision();
                    WorkspaceEntry::loaded(name.clone(), source, config, warnings, revision)
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    tracing::error!("Failed to load workspace '{}': {}", name, err_msg);
                    let poll = Duration::from_secs(source.poll_interval_secs().max(1));
                    let policy = settings.policy(poll);
                    WorkspaceEntry::startup_failed(name.clone(), source, err_msg, &policy)
                }
            };
            entries.insert(name, Arc::new(entry));
        }

        tracing::info!(
            "Loaded {} workspace(s) in {:?}",
            total_configured,
            start.elapsed()
        );

        Self {
            entries,
            load_errors,
            resolved_libraries: Arc::new(resolved_libraries),
            triggers_disabled,
            settings,
        }
    }

    /// Create a WorkspaceManager from pre-built entries (for testing)
    pub fn from_entries(entries: HashMap<String, WorkspaceEntry>) -> Self {
        Self {
            entries: entries.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
            load_errors: HashMap::new(),
            resolved_libraries: Arc::new(HashMap::new()),
            triggers_disabled: HashSet::new(),
            settings: ReloadSettings::default(),
        }
    }

    /// Create a WorkspaceManager from an in-memory WorkspaceConfig (for testing).
    /// The workspace is registered under the given name with a temp path.
    pub fn from_config(name: &str, config: WorkspaceConfig) -> Self {
        let source = Arc::new(InMemorySource {
            config: tokio::sync::RwLock::new(config.clone()),
            revision: None,
        });
        let mut entries = HashMap::new();
        entries.insert(
            name.to_string(),
            Arc::new(WorkspaceEntry::new(
                name.to_string(),
                source as Arc<dyn WorkspaceSource>,
                config,
                None,
            )),
        );
        Self {
            entries,
            load_errors: HashMap::new(),
            resolved_libraries: Arc::new(HashMap::new()),
            triggers_disabled: HashSet::new(),
            settings: ReloadSettings::default(),
        }
    }

    /// Create a WorkspaceManager from several in-memory configs, each with an
    /// optional explicit revision (for testing multi-workspace behaviour such
    /// as cross-workspace action resolution).
    pub fn from_configs(configs: Vec<(String, WorkspaceConfig, Option<String>)>) -> Self {
        let mut entries = HashMap::new();
        for (name, config, revision) in configs {
            let source = Arc::new(InMemorySource {
                config: tokio::sync::RwLock::new(config.clone()),
                revision: revision.clone(),
            });
            entries.insert(
                name.clone(),
                Arc::new(WorkspaceEntry::new(
                    name,
                    source as Arc<dyn WorkspaceSource>,
                    config,
                    revision,
                )),
            );
        }
        Self {
            entries,
            load_errors: HashMap::new(),
            resolved_libraries: Arc::new(HashMap::new()),
            triggers_disabled: HashSet::new(),
            settings: ReloadSettings::default(),
        }
    }

    /// Test-only: register a source-construction failure for `name` with no
    /// corresponding `entries` row — mirrors what `new()` does when e.g.
    /// `GitSource::new()` itself fails, before any placeholder entry exists.
    #[cfg(test)]
    pub fn insert_load_error_for_test(&mut self, name: &str, error: &str) {
        self.load_errors.insert(name.to_string(), error.to_string());
    }

    /// Look up an entry by name (`pub(crate)` — the manager is the only
    /// public surface outside the module). Only exercised by unit tests
    /// today; Task 9/10 wire it into the peek/load dispatch paths.
    #[allow(dead_code)]
    pub(crate) fn entry(&self, name: &str) -> Option<Arc<WorkspaceEntry>> {
        self.entries.get(name).cloned()
    }

    /// Test-only: replace a loaded workspace's config in place (simulates a
    /// reload that changed the YAML) without touching the source. Not
    /// `#[cfg(test)]` for the same reason as `mark_unavailable_for_test`:
    /// integration test binaries link this crate without `cfg(test)`.
    #[doc(hidden)]
    pub async fn replace_config_for_test(&self, name: &str, cfg: WorkspaceConfig) {
        if let Some(entry) = self.entries.get(name) {
            entry.replace_config(cfg);
        }
    }

    /// Test-only: flip an already-registered entry (e.g. from `from_configs`)
    /// to the "configured but unavailable" state — `has_workspace` stays
    /// `true`, `get_config` returns `None` — mirroring a workspace whose
    /// source loaded once but whose last reload failed. Not `#[cfg(test)]`:
    /// integration test binaries link this crate without `cfg(test)`, so a
    /// `cfg(test)`-gated helper would be invisible to them.
    ///
    /// Panics if `name` has no entry — register it via `from_configs` first.
    #[doc(hidden)]
    pub fn mark_unavailable_for_test(&self, name: &str) {
        let entry = self
            .entries
            .get(name)
            .expect("mark_unavailable_for_test: no entry registered for this name");
        self.fail_for_test(entry, "marked unavailable for test");
    }

    /// Test-only: put a loaded workspace into its load-error state, as a
    /// failed reload does.
    #[cfg(test)]
    pub fn mark_errored_for_test(&self, name: &str, error: &str) {
        let entry = self.entries.get(name).expect("workspace entry");
        self.fail_for_test(entry, error);
    }

    /// Route a synthetic test failure through the single writer so
    /// availability and the published error never diverge.
    fn fail_for_test(&self, entry: &WorkspaceEntry, error: &str) {
        let policy = self.settings.policy(entry.poll_interval());
        let _ = entry.apply_load_result(
            Caller::External,
            None,
            Err(anyhow::anyhow!("{error}")),
            &HashMap::new(),
            &policy,
            Instant::now(),
        );
    }

    /// Get the workspace config for a given name.
    /// Returns None for workspaces with a load error (empty placeholder config).
    pub async fn get_config(&self, name: &str) -> Option<Arc<WorkspaceConfig>> {
        let published = self.entries.get(name)?.published();
        published
            .error
            .is_none()
            .then(|| Arc::clone(&published.config))
    }

    /// Get the filesystem path for a workspace.
    /// Returns None for workspaces with a load error.
    pub fn get_path(&self, name: &str) -> Option<&Path> {
        let entry = self.entries.get(name)?;
        entry.is_healthy().then_some(entry.source_path.as_path())
    }

    /// Get the current revision for a workspace.
    /// Returns `None` if the workspace does not exist or has a load error.
    pub fn get_revision(&self, name: &str) -> Option<String> {
        let published = self.entries.get(name)?.published();
        if published.error.is_some() {
            return None;
        }
        published.revision.clone()
    }

    /// List all workspace names (including errored workspaces with placeholder entries).
    /// Note: source construction failures (e.g. `GitSource::new()`) are not included here
    /// as they have no entry — use `list_workspace_info()` or `configured_names()` for
    /// the complete list.
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(|s| s.as_str()).collect()
    }

    /// Whether this server should fire `name`'s triggers (cron schedules,
    /// webhooks, event sources). `false` only when the server config sets
    /// `workspaces.<name>.triggers: false`; unknown names report `true`
    /// (there is nothing to suppress). Consumers that iterate workspaces for
    /// triggers must check this and skip the workspace when it is `false`.
    pub fn triggers_enabled(&self, name: &str) -> bool {
        !self.triggers_disabled.contains(name)
    }

    /// Builder (used by unit and integration tests, which construct managers
    /// via `from_config`/`from_entries` rather than `new`): mark `name` as
    /// having its triggers disabled, as if the server config had
    /// `workspaces.<name>.triggers: false`.
    pub fn with_triggers_disabled(mut self, name: &str) -> Self {
        self.triggers_disabled.insert(name.to_string());
        self
    }

    /// Every workspace name the server was CONFIGURED with, whether or not it
    /// loaded successfully — the union of `entries` (placeholder-or-healthy)
    /// and `load_errors` (source construction failed before an entry could
    /// even be created, e.g. `GitSource::new()`).
    ///
    /// Callers that need to distinguish "not configured at all" (unknown,
    /// 400) from "configured but currently unavailable" (unavailable, 500)
    /// should use this instead of `names()`.
    pub fn configured_names(&self) -> Vec<String> {
        let mut names: HashSet<String> = self.entries.keys().cloned().collect();
        names.extend(self.load_errors.keys().cloned());
        names.into_iter().collect()
    }

    /// Check whether a workspace with the given name exists (regardless of health status).
    // TODO: like `names()`, this ignores `load_errors` — a workspace whose
    // *source construction* failed (e.g. `GitSource::new()`) has no `entries`
    // row and so reads as nonexistent here, not merely unavailable. Same gap
    // as `names()`; `configured_names()` closes it for `WorkspaceSet::load`
    // but this method is used for action-reference detection and is left
    // unchanged for now.
    pub fn has_workspace(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Get all workspace configs as (name, config) pairs.
    /// Skips workspaces with load errors.
    pub async fn get_all_configs(&self) -> Vec<(String, Arc<WorkspaceConfig>)> {
        self.entries
            .iter()
            .filter_map(|(name, entry)| {
                let p = entry.published();
                p.error
                    .is_none()
                    .then(|| (name.clone(), Arc::clone(&p.config)))
            })
            .collect()
    }

    /// List all workspaces including ones that failed to load.
    /// This includes both placeholder entries (load failures) and source construction
    /// failures from `load_errors`. Failed workspaces have `error` set and zero counts.
    pub async fn list_workspace_info(&self) -> Vec<WorkspaceInfo> {
        let mut infos = Vec::new();
        for (name, entry) in &self.entries {
            let p = entry.published();
            let warnings = if p.error.is_none() {
                p.warnings.clone()
            } else {
                Vec::new()
            };
            infos.push(WorkspaceInfo {
                name: name.clone(),
                tasks_count: p.config.tasks.len(),
                actions_count: p.config.actions.len(),
                triggers_count: p.config.triggers.len(),
                connections_count: p.config.connections.len(),
                revision: p.revision.clone(),
                error: p.error.clone(),
                warnings,
                triggers_enabled: self.triggers_enabled(name),
            });
        }
        // Source construction failures (e.g. GitSource::new() failed — no source object)
        for (name, error) in &self.load_errors {
            infos.push(WorkspaceInfo {
                name: name.clone(),
                tasks_count: 0,
                actions_count: 0,
                triggers_count: 0,
                connections_count: 0,
                triggers_enabled: self.triggers_enabled(name),
                revision: None,
                error: Some(error.clone()),
                warnings: Vec::new(),
            });
        }
        infos
    }

    /// Reload a specific workspace from its source, updating config and revision.
    /// Library items are re-merged into the workspace config.
    /// On success, clears any previous load error. On failure, sets the load error.
    ///
    /// Concurrent reload attempts on the same workspace are serialized via the
    /// per-workspace `reload_state` mutex — this is the only reason the
    /// expensive `source.load()` (git fetch + reset --hard) can be safely
    /// invoked from multiple call sites (watcher, scheduler, hooks, API).
    /// Without this guard, two simultaneous reloads would perform
    /// `reset --hard` on the same on-disk libgit2 clone and corrupt its
    /// working tree.
    pub async fn reload(&self, name: &str) -> Result<()> {
        let entry = self
            .entries
            .get(name)
            .with_context(|| format!("Workspace '{}' not found", name))?;

        let exec = entry.exec();
        let mut reload_state = exec.lock().await;
        let result = self.do_reload(name, entry).await;
        reload_state.last_completed = Some(Instant::now());
        result
    }

    /// Like [`Self::reload`], but enforces a `cooldown` window since the last
    /// completed reload and returns a typed [`ReloadApiError`] so the HTTP
    /// handler can map cleanly to 404 / 429 / 500. Pass `Duration::ZERO` to
    /// skip the cooldown (used by tests).
    pub async fn reload_for_api(
        &self,
        name: &str,
        cooldown: Duration,
    ) -> std::result::Result<(), ReloadApiError> {
        let entry = self.entries.get(name).ok_or(ReloadApiError::NotFound)?;

        // try_lock first so a refresh that's currently in flight is
        // surfaced as a cooldown response rather than queueing the caller
        // and tying up a worker.
        let exec = entry.exec();
        let mut reload_state = match exec.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                return Err(ReloadApiError::Cooldown {
                    retry_after_secs: cooldown.as_secs().max(1),
                });
            }
        };

        if let Some(last) = reload_state.last_completed {
            let elapsed = last.elapsed();
            if elapsed < cooldown {
                let retry_after = (cooldown - elapsed).as_secs().max(1);
                return Err(ReloadApiError::Cooldown {
                    retry_after_secs: retry_after,
                });
            }
        }

        let result = self.do_reload(name, entry).await;
        reload_state.last_completed = Some(Instant::now());
        result.map_err(ReloadApiError::Failed)
    }

    /// Reload implementation. Caller must hold `entry.exec()`.
    async fn do_reload(&self, name: &str, entry: &WorkspaceEntry) -> Result<()> {
        let result = entry
            .source
            .load()
            .await
            .map(|(config, warnings)| LoadOutcome {
                config,
                warnings,
                revision: entry.source.revision(),
            });
        let policy = self.settings.policy(entry.poll_interval());
        entry
            .apply_load_result(
                Caller::External,
                None,
                result,
                &self.resolved_libraries,
                &policy,
                Instant::now(),
            )
            .map(|_| ())
            .with_context(|| format!("Failed to reload workspace '{}'", name))
    }

    /// Get library source paths for tarball building.
    /// Returns map of library name → source path.
    pub fn get_library_paths(&self) -> HashMap<String, PathBuf> {
        self.resolved_libraries
            .iter()
            .map(|(name, lib)| (name.clone(), lib.path.clone()))
            .collect()
    }

    /// Start background watchers for hot-reload (folder watchers + git pollers).
    /// Only reloads when the source revision changes.
    /// Uses each source's `poll_interval_secs()` for the polling frequency.
    /// Errored workspaces retry `load()` on each poll cycle until they recover.
    /// Watchers stop cleanly when `cancel_token` is cancelled.
    ///
    /// When `event_bus` is provided, the watcher emits
    /// [`crate::events::CHANNEL_WORKSPACE_RELOADED`] every time the source
    /// revision changes so peer replicas can refresh their cached config
    /// without waiting for their own poll tick.
    pub fn start_watchers(
        &self,
        cancel_token: CancellationToken,
        event_bus: Option<crate::events::EventBus>,
    ) {
        for entry in self.entries.values() {
            let entry = Arc::clone(entry);
            let ws_name = entry.name.clone();
            let poll_secs = entry.source.poll_interval_secs();
            let libs = self.resolved_libraries.clone();
            let settings = self.settings;
            let cancel = cancel_token.clone();
            let needs_initial_load = !entry.is_healthy();
            let bus = event_bus.clone();

            tokio::spawn(async move {
                tracing::info!(
                    "Watcher started for workspace '{}' (poll interval: {}s{})",
                    ws_name,
                    poll_secs,
                    if needs_initial_load {
                        ", retrying failed load"
                    } else {
                        ""
                    },
                );

                let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
                let mut last_revision = entry.published().revision.clone();

                if !needs_initial_load {
                    // Skip the first immediate tick — the workspace was just loaded
                    interval.tick().await;
                }

                loop {
                    tokio::select! {
                        _ = interval.tick() => {}
                        () = cancel.cancelled() => {
                            tracing::info!(
                                "Watcher for workspace '{}' stopping (shutdown)",
                                ws_name
                            );
                            break;
                        }
                    }

                    let is_errored = !entry.is_healthy();

                    // For errored entries, skip the peek optimization and always try load()
                    if !is_errored {
                        // Check revision cheaply first. For folder sources this
                        // hashes file metadata+content without parsing YAML.
                        // For git sources this does a lightweight ls-remote
                        // (blocking network call, so wrap in spawn_blocking).
                        let source_clone = entry.source.clone();
                        let current_revision =
                            tokio::task::spawn_blocking(move || source_clone.peek_revision())
                                .await
                                .unwrap_or_else(|e| {
                                    tracing::error!("peek_revision task failed: {:#}", e);
                                    None
                                });
                        if current_revision == last_revision && current_revision.is_some() {
                            continue;
                        }

                        tracing::info!("Workspace '{}': change detected, reloading...", ws_name);
                    }

                    // Revision changed (or source doesn't support peek, or errored) — do full reload
                    let source = entry.source.clone();
                    let result = source.load().await.map(|(config, warnings)| LoadOutcome {
                        config,
                        warnings,
                        revision: source.revision(),
                    });
                    let policy = settings.policy(entry.poll_interval());
                    match entry.apply_load_result(
                        Caller::Watcher,
                        None,
                        result,
                        &libs,
                        &policy,
                        Instant::now(),
                    ) {
                        Ok(_) => {
                            let new_revision = entry.published().revision.clone();
                            tracing::info!(
                                "Workspace '{}' reloaded (revision: {:?} -> {:?})",
                                entry.name,
                                last_revision.as_deref().map(|s| &s[..8.min(s.len())]),
                                new_revision.as_deref().map(|s| &s[..8.min(s.len())]),
                            );
                            if let Some(bus) = &bus {
                                bus.publish_workspace_reloaded(&entry.name).await;
                            }
                            last_revision = new_revision;
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reload workspace '{}': {:#}", entry.name, e);
                        }
                    }
                }
            });
        }
    }
}

/// Info about a workspace for the API
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub tasks_count: usize,
    pub actions_count: usize,
    pub triggers_count: usize,
    pub connections_count: usize,
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// `false` when the server config sets `triggers: false` for this
    /// workspace — its triggers are listed but never fired by this server.
    pub triggers_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_workspace_dir() -> TempDir {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();
        temp
    }

    #[tokio::test]
    async fn test_workspace_manager_single() {
        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert_eq!(mgr.names().len(), 1);

        let config = mgr.get_config("default").await.unwrap();
        assert_eq!(config.tasks.len(), 1);
        assert!(config.tasks.contains_key("hello"));
    }

    #[tokio::test]
    async fn test_workspace_manager_multiple() {
        let temp1 = create_test_workspace_dir();
        let temp2 = TempDir::new().unwrap();
        fs::write(
            temp2.path().join("another.yaml"),
            r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    flow:
      step1:
        action: build
"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp1.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "ops".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp2.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert_eq!(mgr.names().len(), 2);

        let default_config = mgr.get_config("default").await.unwrap();
        assert!(default_config.tasks.contains_key("hello"));

        let ops_config = mgr.get_config("ops").await.unwrap();
        assert!(ops_config.tasks.contains_key("deploy"));
    }

    #[tokio::test]
    async fn test_workspace_manager_unknown() {
        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert!(mgr.get_config("nonexistent").await.is_none());
        assert!(mgr.get_path("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_workspace_info() {
        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "default");
        assert_eq!(infos[0].tasks_count, 1);
        assert_eq!(infos[0].actions_count, 1);
        assert_eq!(infos[0].triggers_count, 0);
    }

    #[tokio::test]
    async fn test_triggers_enabled_defaults_to_true() {
        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp.path().to_str().unwrap().to_string(),
                triggers: true,
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert!(mgr.triggers_enabled("default"));
        // Unknown workspace: nothing to suppress, report enabled.
        assert!(mgr.triggers_enabled("nonexistent"));
        let infos = mgr.list_workspace_info().await;
        assert!(infos[0].triggers_enabled);
    }

    #[tokio::test]
    async fn test_triggers_disabled_from_def() {
        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "quiet".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp.path().to_str().unwrap().to_string(),
                triggers: false,
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert!(!mgr.triggers_enabled("quiet"));
        // Config still loads normally — the flag only affects trigger firing.
        let config = mgr.get_config("quiet").await.unwrap();
        assert_eq!(config.tasks.len(), 1);
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        assert!(!infos[0].triggers_enabled);
        assert!(infos[0].error.is_none());
    }

    #[tokio::test]
    async fn test_triggers_disabled_reported_for_workspace_that_failed_to_load() {
        // The flag is captured from the def before any load happens, so a
        // workspace whose load fails (placeholder entry) still reports it.
        let mut defs = HashMap::new();
        defs.insert(
            "broken".to_string(),
            WorkspaceSourceDef::Folder {
                path: "/nonexistent/stroem-workspace-triggers-test".to_string(),
                triggers: false,
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert!(!mgr.triggers_enabled("broken"));
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        assert!(infos[0].error.is_some(), "load must have failed");
        assert!(!infos[0].triggers_enabled);
    }

    #[tokio::test]
    async fn test_triggers_disabled_reported_for_source_construction_failure() {
        // Source-construction failures have no `entries` row, only a
        // `load_errors` row — `list_workspace_info` must still carry the flag.
        let mut mgr = WorkspaceManager::from_configs(vec![]).with_triggers_disabled("broken");
        mgr.insert_load_error_for_test("broken", "boom");
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "broken");
        assert_eq!(infos[0].error.as_deref(), Some("boom"));
        assert!(!infos[0].triggers_enabled);
    }

    #[tokio::test]
    async fn test_with_triggers_disabled_builder() {
        let mgr = WorkspaceManager::from_configs(vec![
            ("a".to_string(), WorkspaceConfig::new(), None),
            ("b".to_string(), WorkspaceConfig::new(), None),
        ])
        .with_triggers_disabled("a");
        assert!(!mgr.triggers_enabled("a"));
        assert!(mgr.triggers_enabled("b"));
    }

    #[test]
    fn test_workspace_info_serializes_triggers_enabled() {
        let info = WorkspaceInfo {
            name: "x".to_string(),
            tasks_count: 0,
            actions_count: 0,
            triggers_count: 2,
            connections_count: 0,
            revision: None,
            error: None,
            warnings: Vec::new(),
            triggers_enabled: false,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["triggers_enabled"], serde_json::Value::Bool(false));
        assert_eq!(json["triggers_count"], 2);
    }

    #[tokio::test]
    async fn test_workspace_manager_empty() {
        let mgr = WorkspaceManager::new(HashMap::new(), HashMap::new(), HashMap::new()).await;
        assert_eq!(mgr.names().len(), 0);
        assert!(mgr.get_config("anything").await.is_none());
        assert!(mgr.get_path("anything").is_none());
    }

    #[tokio::test]
    async fn test_from_config_creates_workspace() {
        use stroem_common::models::workflow::{ActionDef, FlowStep, TaskDef};

        let mut config = WorkspaceConfig::new();
        config.actions.insert(
            "test-action".to_string(),
            ActionDef {
                action_type: "script".to_string(),
                name: None,
                description: None,
                task: None,
                cmd: None,
                script: Some("echo test".to_string()),
                source: None,
                runner: None,
                language: None,
                dependencies: vec![],
                interpreter: None,
                args: vec![],
                tags: vec![],
                image: None,
                command: None,
                entrypoint: None,
                env: None,
                workdir: None,
                resources: None,
                input: HashMap::new(),
                output: None,
                manifest: None,
                provider: None,
                model: None,
                system_prompt: None,
                prompt: None,
                temperature: None,
                max_tokens: None,
                tools: vec![],
                max_turns: None,
                interactive: false,
                message: None,
                retry: None,
            },
        );

        let mut flow = HashMap::new();
        flow.insert(
            "step1".to_string(),
            FlowStep {
                action: "test-action".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                continue_when_skipped: false,
                timeout: None,
                when: None,
                for_each: None,
                sequential: false,
                retry: None,
                inline_action: None,
            },
        );

        config.tasks.insert(
            "test-task".to_string(),
            TaskDef {
                name: None,
                description: None,
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow,
                timeout: None,
                retry: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
                on_cancel: vec![],
            },
        );

        let mgr = WorkspaceManager::from_config("test", config);
        assert_eq!(mgr.names().len(), 1);
        assert!(mgr.names().contains(&"test"));

        let loaded_config = mgr.get_config("test").await.unwrap();
        assert_eq!(loaded_config.actions.len(), 1);
        assert_eq!(loaded_config.tasks.len(), 1);
        assert!(loaded_config.actions.contains_key("test-action"));
        assert!(loaded_config.tasks.contains_key("test-task"));
    }

    #[tokio::test]
    async fn test_from_config_multiple() {
        use stroem_common::models::workflow::{ActionDef, FlowStep, TaskDef};

        let mut config1 = WorkspaceConfig::new();
        config1.actions.insert(
            "action1".to_string(),
            ActionDef {
                action_type: "script".to_string(),
                name: None,
                description: None,
                task: None,
                cmd: None,
                script: Some("echo 1".to_string()),
                source: None,
                runner: None,
                language: None,
                dependencies: vec![],
                interpreter: None,
                args: vec![],
                tags: vec![],
                image: None,
                command: None,
                entrypoint: None,
                env: None,
                workdir: None,
                resources: None,
                input: HashMap::new(),
                output: None,
                manifest: None,
                provider: None,
                model: None,
                system_prompt: None,
                prompt: None,
                temperature: None,
                max_tokens: None,
                tools: vec![],
                max_turns: None,
                interactive: false,
                message: None,
                retry: None,
            },
        );

        let mut flow1 = HashMap::new();
        flow1.insert(
            "step1".to_string(),
            FlowStep {
                action: "action1".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                continue_when_skipped: false,
                timeout: None,
                when: None,
                for_each: None,
                sequential: false,
                retry: None,
                inline_action: None,
            },
        );
        config1.tasks.insert(
            "task1".to_string(),
            TaskDef {
                name: None,
                description: None,
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow: flow1,
                timeout: None,
                retry: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
                on_cancel: vec![],
            },
        );

        let mut config2 = WorkspaceConfig::new();
        config2.actions.insert(
            "action2".to_string(),
            ActionDef {
                action_type: "script".to_string(),
                name: None,
                description: None,
                task: None,
                cmd: None,
                script: Some("echo 2".to_string()),
                source: None,
                runner: None,
                language: None,
                dependencies: vec![],
                interpreter: None,
                args: vec![],
                tags: vec![],
                image: None,
                command: None,
                entrypoint: None,
                env: None,
                workdir: None,
                resources: None,
                input: HashMap::new(),
                output: None,
                manifest: None,
                provider: None,
                model: None,
                system_prompt: None,
                prompt: None,
                temperature: None,
                max_tokens: None,
                tools: vec![],
                max_turns: None,
                interactive: false,
                message: None,
                retry: None,
            },
        );

        let mut flow2 = HashMap::new();
        flow2.insert(
            "step1".to_string(),
            FlowStep {
                action: "action2".to_string(),
                name: None,
                description: None,
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
                continue_when_skipped: false,
                timeout: None,
                when: None,
                for_each: None,
                sequential: false,
                retry: None,
                inline_action: None,
            },
        );
        config2.tasks.insert(
            "task2".to_string(),
            TaskDef {
                name: None,
                description: None,
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow: flow2,
                timeout: None,
                retry: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
                on_cancel: vec![],
            },
        );

        let mgr1 = WorkspaceManager::from_config("ws1", config1);
        let mgr2 = WorkspaceManager::from_config("ws2", config2);

        // Verify they're independent
        assert_eq!(mgr1.names().len(), 1);
        assert_eq!(mgr2.names().len(), 1);
        assert!(mgr1.get_config("ws1").await.is_some());
        assert!(mgr1.get_config("ws2").await.is_none());
        assert!(mgr2.get_config("ws2").await.is_some());
        assert!(mgr2.get_config("ws1").await.is_none());

        let config1_loaded = mgr1.get_config("ws1").await.unwrap();
        let config2_loaded = mgr2.get_config("ws2").await.unwrap();
        assert!(config1_loaded.actions.contains_key("action1"));
        assert!(!config1_loaded.actions.contains_key("action2"));
        assert!(config2_loaded.actions.contains_key("action2"));
        assert!(!config2_loaded.actions.contains_key("action1"));
    }

    #[tokio::test]
    async fn test_workspace_get_path() {
        let temp = create_test_workspace_dir();
        let temp_path = temp.path().to_str().unwrap().to_string();

        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp_path.clone(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let path = mgr.get_path("default").unwrap();
        assert_eq!(path.to_str().unwrap(), temp_path);
    }

    #[tokio::test]
    async fn test_workspace_get_revision_folder() {
        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let revision = mgr.get_revision("default");
        assert!(revision.is_some());
        // Verify it's a valid hex string (blake2 hash)
        let rev_str = revision.unwrap();
        assert!(!rev_str.is_empty());
        assert!(rev_str.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn test_workspace_revision_changes_on_file_change() {
        let temp = create_test_workspace_dir();
        let temp_path = temp.path().to_str().unwrap().to_string();

        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp_path.clone(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let revision1 = mgr.get_revision("default").unwrap();

        // Modify the file content
        let workflows_dir = temp.path().join(".workflows");
        fs::write(
            workflows_dir.join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo goodbye"
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        // Reload the workspace
        let mut defs2 = HashMap::new();
        defs2.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp_path,
            },
        );
        let mgr2 = WorkspaceManager::new(defs2, HashMap::new(), HashMap::new()).await;
        let revision2 = mgr2.get_revision("default").unwrap();

        // Revision should have changed
        assert_ne!(revision1, revision2);
    }

    #[tokio::test]
    async fn test_workspace_manager_nonexistent_folder_records_error() {
        let mut defs = HashMap::new();
        defs.insert(
            "nonexistent".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: "/nonexistent/path/12345/xyz".to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        // Workspace should be in entries (placeholder) but get_config returns None
        assert_eq!(mgr.names().len(), 1);
        assert!(mgr.names().contains(&"nonexistent"));
        assert!(mgr.get_config("nonexistent").await.is_none());
        assert!(mgr.get_path("nonexistent").is_none());
        // Should appear in list_workspace_info with error
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        let info = &infos[0];
        assert_eq!(info.name, "nonexistent");
        assert!(info.error.is_some());
        assert_eq!(info.tasks_count, 0);
    }

    #[tokio::test]
    async fn test_list_workspace_info_multiple() {
        let temp1 = create_test_workspace_dir();
        let temp2 = TempDir::new().unwrap();
        let workflows_dir2 = temp2.path().join(".workflows");
        fs::create_dir(&workflows_dir2).unwrap();
        fs::write(
            workflows_dir2.join("test.yaml"),
            r#"
actions:
  build:
    type: script
    script: "make build"
  deploy:
    type: script
    script: "make deploy"
tasks:
  ci:
    flow:
      step1:
        action: build
  cd:
    flow:
      step1:
        action: deploy
"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp1.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "ops".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp2.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let infos = mgr.list_workspace_info().await;

        assert_eq!(infos.len(), 2);

        // Find each workspace in the infos
        let default_info = infos.iter().find(|i| i.name == "default").unwrap();
        let ops_info = infos.iter().find(|i| i.name == "ops").unwrap();

        assert_eq!(default_info.tasks_count, 1);
        assert_eq!(default_info.actions_count, 1);

        assert_eq!(ops_info.tasks_count, 2);
        assert_eq!(ops_info.actions_count, 2);
    }

    #[tokio::test]
    async fn test_workspace_names_is_unordered() {
        let temp1 = create_test_workspace_dir();
        let temp2 = create_test_workspace_dir();
        let temp3 = create_test_workspace_dir();

        let mut defs = HashMap::new();
        defs.insert(
            "alpha".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp1.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "beta".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp2.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "gamma".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp3.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let names = mgr.names();

        assert_eq!(names.len(), 3);
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(names.contains(&"gamma"));
    }

    #[tokio::test]
    async fn test_malformed_yaml_records_error() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("bad.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo hello
    # Missing closing quote - invalid YAML
"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "bad".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        // Workspace loads successfully — the bad file is skipped with a warning
        assert_eq!(mgr.names().len(), 1);
        assert!(mgr.get_config("bad").await.is_some());
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "bad");
        // No hard error — workspace is healthy
        assert!(infos[0].error.is_none());
        // One warning for the skipped bad file
        assert_eq!(infos[0].warnings.len(), 1);
        let warning = &infos[0].warnings[0];
        assert!(
            warning.contains("bad.yaml"),
            "Warning should mention the file: {}",
            warning
        );
    }

    #[tokio::test]
    async fn test_mixed_valid_invalid_yaml_files() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();

        // Valid file 1
        fs::write(
            workflows_dir.join("valid1.yaml"),
            r#"
actions:
  action1:
    type: script
    script: "echo 1"
tasks:
  task1:
    flow:
      step1:
        action: action1
"#,
        )
        .unwrap();

        // Valid file 2
        fs::write(
            workflows_dir.join("valid2.yaml"),
            r#"
actions:
  action2:
    type: script
    script: "echo 2"
tasks:
  task2:
    flow:
      step1:
        action: action2
"#,
        )
        .unwrap();

        // Invalid YAML (broken string literal)
        fs::write(
            workflows_dir.join("invalid.yaml"),
            "actions:\n  bad:\n    type: script\n    script: \"unterminated\n",
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "mixed".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;

        // Workspace should load successfully with both valid files merged
        let config = mgr.get_config("mixed").await.unwrap();
        assert_eq!(config.actions.len(), 2);
        assert!(config.actions.contains_key("action1"));
        assert!(config.actions.contains_key("action2"));
        assert_eq!(config.tasks.len(), 2);

        // One warning for the invalid file
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        let info = &infos[0];
        assert!(info.error.is_none());
        assert_eq!(info.warnings.len(), 1);
        let warning = &info.warnings[0];
        assert!(
            warning.contains("invalid.yaml"),
            "Warning should mention the file: {}",
            warning
        );
    }

    #[tokio::test]
    async fn test_all_invalid_yaml_files() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();

        // Two invalid YAML files
        fs::write(
            workflows_dir.join("bad1.yaml"),
            "actions:\n  a:\n    script: \"unterminated\n",
        )
        .unwrap();
        fs::write(
            workflows_dir.join("bad2.yaml"),
            "actions:\n  b:\n    script: \"unterminated\n",
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "all-bad".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;

        // Workspace loads with empty config — no hard error
        let config = mgr.get_config("all-bad").await.unwrap();
        assert_eq!(config.actions.len(), 0);
        assert_eq!(config.tasks.len(), 0);

        // Two warnings, one per file
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        let info = &infos[0];
        assert!(info.error.is_none());
        assert_eq!(info.warnings.len(), 2);
    }

    /// Warnings should be cleared when a previously-bad file is fixed on reload.
    #[tokio::test]
    async fn test_warnings_cleared_when_file_fixed_on_reload() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();

        // Start with one valid and one invalid file
        fs::write(
            workflows_dir.join("good.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo hello"
"#,
        )
        .unwrap();
        fs::write(
            workflows_dir.join("bad.yaml"),
            "actions:\n  a:\n    script: \"unterminated\n",
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "ws".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;

        // Initial: 1 valid action, 1 warning
        let config = mgr.get_config("ws").await.unwrap();
        assert_eq!(config.actions.len(), 1);
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos[0].warnings.len(), 1);

        // Fix the bad file
        fs::write(
            workflows_dir.join("bad.yaml"),
            r#"
actions:
  farewell:
    type: script
    script: "echo bye"
"#,
        )
        .unwrap();

        // Reload
        mgr.reload("ws").await.unwrap();

        // Now: 2 valid actions, 0 warnings
        let config = mgr.get_config("ws").await.unwrap();
        assert_eq!(config.actions.len(), 2);
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos[0].warnings.len(), 0);
        assert!(infos[0].error.is_none());
    }

    /// Warnings should be cleared when workspace transitions to a hard error state.
    #[tokio::test]
    async fn test_warnings_cleared_on_hard_error() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();

        // Start with one valid and one invalid file
        fs::write(
            workflows_dir.join("good.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo hello"
"#,
        )
        .unwrap();
        fs::write(
            workflows_dir.join("bad.yaml"),
            "actions:\n  a:\n    script: \"unterminated\n",
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "ws".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;

        // Initial: loaded with 1 warning
        let infos = mgr.list_workspace_info().await;
        assert!(infos[0].error.is_none());
        assert_eq!(infos[0].warnings.len(), 1);

        // Remove all contents to cause a hard error on reload.
        // We can't remove temp.path() itself (TempDir owns it), so replace the
        // workspace path with a non-existent directory.
        fs::remove_dir_all(temp.path()).unwrap();
        // Recreate temp path as empty — FolderSource will fail because path doesn't
        // exist (TempDir keeps track of it for cleanup, but the dir is gone).

        let _ = mgr.reload("ws").await;

        // Now: hard error, warnings cleared
        let infos = mgr.list_workspace_info().await;
        assert!(infos[0].error.is_some());
        assert!(infos[0].warnings.is_empty());
    }

    #[tokio::test]
    async fn test_workspace_manager_reload_updates_config() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo hello"
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );
        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;

        let config1 = mgr.get_config("default").await.unwrap();
        assert_eq!(config1.actions.len(), 1);

        // Add a second action to the YAML
        fs::write(
            workflows_dir.join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: "echo hello"
  build:
    type: script
    script: "make build"
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        // Before reload, config should still be stale
        let config_before = mgr.get_config("default").await.unwrap();
        assert_eq!(config_before.actions.len(), 1);

        // Reload should update config
        mgr.reload("default").await.unwrap();
        let config2 = mgr.get_config("default").await.unwrap();
        assert_eq!(config2.actions.len(), 2);
        assert!(config2.actions.contains_key("build"));
    }

    #[tokio::test]
    async fn test_workspace_manager_reload_updates_revision() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("test.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo v1\n",
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );
        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let rev1 = mgr.get_revision("default").unwrap();

        // Modify content
        fs::write(
            workflows_dir.join("test.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo v2\n",
        )
        .unwrap();

        // Reload and verify revision changed
        mgr.reload("default").await.unwrap();
        let rev2 = mgr.get_revision("default").unwrap();
        assert_ne!(rev1, rev2, "Revision should change after reload");
    }

    #[tokio::test]
    async fn test_workspace_manager_reload_nonexistent_fails() {
        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );
        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;

        let result = mgr.reload("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_folder_source_load_merges_multiple_files() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();

        // First YAML file with action1 and task1
        fs::write(
            workflows_dir.join("file1.yaml"),
            r#"
actions:
  action1:
    type: script
    script: "echo 1"
tasks:
  task1:
    flow:
      step1:
        action: action1
"#,
        )
        .unwrap();

        // Second YAML file with action2 and task2
        fs::write(
            workflows_dir.join("file2.yaml"),
            r#"
actions:
  action2:
    type: script
    script: "echo 2"
tasks:
  task2:
    flow:
      step1:
        action: action2
"#,
        )
        .unwrap();

        let source = folder::FolderSource::new(temp.path().to_str().unwrap());
        let (config, _) = source.load().await.unwrap();

        // Verify both files were merged
        assert_eq!(config.actions.len(), 2);
        assert_eq!(config.tasks.len(), 2);
        assert!(config.actions.contains_key("action1"));
        assert!(config.actions.contains_key("action2"));
        assert!(config.tasks.contains_key("task1"));
        assert!(config.tasks.contains_key("task2"));
    }

    #[tokio::test]
    async fn test_bad_workspace_does_not_block_good_workspace() {
        let good_temp = create_test_workspace_dir();

        let mut defs = HashMap::new();
        defs.insert(
            "good".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: good_temp.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "bad".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: "/nonexistent/path/12345".to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;

        // Both entries exist, but only good has a usable config
        assert_eq!(mgr.names().len(), 2);
        assert!(mgr.get_config("good").await.is_some());
        assert!(mgr.get_config("bad").await.is_none());

        // Both appear in workspace info
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 2);

        let good_info = infos.iter().find(|i| i.name == "good").unwrap();
        assert!(good_info.error.is_none());
        assert_eq!(good_info.tasks_count, 1);

        let bad_info = infos.iter().find(|i| i.name == "bad").unwrap();
        assert!(bad_info.error.is_some());
        assert_eq!(bad_info.tasks_count, 0);
    }

    #[tokio::test]
    async fn test_all_workspaces_bad() {
        let mut defs = HashMap::new();
        defs.insert(
            "ws1".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: "/nonexistent/a".to_string(),
            },
        );
        defs.insert(
            "ws2".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: "/nonexistent/b".to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert_eq!(mgr.names().len(), 2);

        // Both have errors, neither has usable config
        assert!(mgr.get_config("ws1").await.is_none());
        assert!(mgr.get_config("ws2").await.is_none());

        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 2);
        assert!(infos.iter().all(|i| i.error.is_some()));
    }

    #[tokio::test]
    async fn test_load_error_message_preserved() {
        let mut defs = HashMap::new();
        defs.insert(
            "missing".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: "/nonexistent/path/12345/xyz".to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        let error = infos[0].error.as_ref().unwrap();
        // Error message should contain the path or be meaningful
        assert!(
            error.contains("nonexistent")
                || error.contains("No such file")
                || error.contains("not found"),
            "Error message should be meaningful, got: {}",
            error
        );
    }

    #[tokio::test]
    async fn test_workspace_info_error_field_skipped_when_none() {
        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "ok".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);

        // Serialize to JSON and verify "error" key is absent
        let json = serde_json::to_value(&infos[0]).unwrap();
        assert!(!json.as_object().unwrap().contains_key("error"));
    }

    #[tokio::test]
    async fn test_start_watchers_stops_on_cancellation() {
        use tokio_util::sync::CancellationToken;

        let temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let cancel_token = CancellationToken::new();

        // Spawn watcher tasks with a long poll interval so they don't fire
        // during the test; we only care that they respond to cancellation.
        mgr.start_watchers(cancel_token.clone(), None);

        // Cancel immediately and give the tasks a moment to observe it.
        cancel_token.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // If watchers ignore cancellation they would run forever, causing the
        // test to hang.  Reaching here means the tasks exited (or will exit)
        // cleanly.  The token being cancelled is the definitive assertion.
        assert!(cancel_token.is_cancelled());
    }

    #[tokio::test]
    async fn test_errored_workspace_not_in_get_all_configs() {
        let good_temp = create_test_workspace_dir();
        let mut defs = HashMap::new();
        defs.insert(
            "good".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: good_temp.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "bad".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: "/nonexistent/path/xyz".to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let all = mgr.get_all_configs().await;
        // Only good workspace should be returned
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "good");
    }

    #[tokio::test]
    async fn test_reload_clears_error() {
        // Use a path that doesn't exist yet → load fails
        let temp = TempDir::new().unwrap();
        let ws_path = temp.path().join("workspace");
        let ws_path_str = ws_path.to_str().unwrap().to_string();

        let mut defs = HashMap::new();
        defs.insert(
            "ws".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: ws_path_str,
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        // Entry exists but errored
        assert!(mgr.get_config("ws").await.is_none());
        let infos = mgr.list_workspace_info().await;
        assert!(infos[0].error.is_some());

        // Fix the workspace by creating the directory with valid files
        fs::create_dir_all(&ws_path).unwrap();
        fs::write(
            ws_path.join("test.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo ok\ntasks:\n  t:\n    flow:\n      s:\n        action: a\n",
        ).unwrap();

        // Reload should succeed and clear the error
        mgr.reload("ws").await.unwrap();
        assert!(mgr.get_config("ws").await.is_some());
        assert!(mgr.get_path("ws").is_some());

        let infos = mgr.list_workspace_info().await;
        assert!(infos[0].error.is_none());
        assert_eq!(infos[0].tasks_count, 1);
    }

    #[tokio::test]
    async fn test_reload_sets_error_on_failure() {
        let temp = create_test_workspace_dir();
        let ws_path = temp.path().to_str().unwrap().to_string();
        let mut defs = HashMap::new();
        defs.insert(
            "ws".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: ws_path.clone(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert!(mgr.get_config("ws").await.is_some());

        // Break the workspace by removing the entire directory
        fs::remove_dir_all(&ws_path).unwrap();

        // Reload should fail and set the error
        let result = mgr.reload("ws").await;
        assert!(result.is_err());

        let infos = mgr.list_workspace_info().await;
        assert!(infos[0].error.is_some());
        // get_config should now return None
        assert!(mgr.get_config("ws").await.is_none());
    }

    #[tokio::test]
    async fn test_failed_workspace_watcher_retries() {
        use tokio_util::sync::CancellationToken;

        // Use a path that doesn't exist yet → load fails
        let temp = TempDir::new().unwrap();
        let ws_path = temp.path().join("workspace");
        let ws_path_str = ws_path.to_str().unwrap().to_string();

        let mut defs = HashMap::new();
        defs.insert(
            "retry".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: ws_path_str,
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert!(mgr.get_config("retry").await.is_none());

        // Fix the workspace by creating it with valid content
        fs::create_dir_all(&ws_path).unwrap();
        fs::write(
            ws_path.join("test.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo ok\ntasks:\n  t:\n    flow:\n      s:\n        action: a\n",
        ).unwrap();

        // Start watchers — errored entry should retry immediately (no first-tick skip)
        let cancel_token = CancellationToken::new();
        mgr.start_watchers(cancel_token.clone(), None);

        // Wait for the watcher to pick up the fix (folder poll is 30s default,
        // but errored entries don't skip the first tick, so it fires right away)
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel_token.cancel();

        // Workspace should have recovered
        assert!(mgr.get_config("retry").await.is_some());
        let infos = mgr.list_workspace_info().await;
        let info = infos.iter().find(|i| i.name == "retry").unwrap();
        assert!(info.error.is_none());
        assert_eq!(info.tasks_count, 1);
    }

    /// Tests the full healthy → errored → healthy cycle.
    ///
    /// Strategy: use `reload()` to force the error state (avoids the 30-second
    /// watcher interval for healthy workspaces), fix the files, then start
    /// watchers.  Because the entry is already errored, the watcher fires
    /// immediately (no first-tick skip) and recovers.
    #[tokio::test]
    async fn test_watcher_healthy_to_errored_to_healthy() {
        use tokio_util::sync::CancellationToken;

        let temp = create_test_workspace_dir();
        let ws_path = temp.path().to_path_buf();
        let mut defs = HashMap::new();
        defs.insert(
            "cycle".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: ws_path.to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        // Starts healthy.
        assert!(mgr.get_config("cycle").await.is_some());

        // Break the workspace and drive it into the error state via reload().
        // (The watcher for a healthy workspace would wait up to 30 s before
        // re-checking, making direct use of start_watchers() impractical here.)
        fs::remove_dir_all(&ws_path).unwrap();
        let result = mgr.reload("cycle").await;
        assert!(result.is_err());
        assert!(mgr.get_config("cycle").await.is_none());

        // Fix the workspace.
        fs::create_dir_all(ws_path.join(".workflows")).unwrap();
        fs::write(
            ws_path.join(".workflows").join("test.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo ok\ntasks:\n  t:\n    flow:\n      s:\n        action: a\n",
        )
        .unwrap();

        // Start watchers.  The entry is errored, so the watcher retries
        // immediately without waiting for a full poll interval.
        let cancel_token = CancellationToken::new();
        mgr.start_watchers(cancel_token.clone(), None);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel_token.cancel();

        // Should have recovered.
        assert!(mgr.get_config("cycle").await.is_some());
        let infos = mgr.list_workspace_info().await;
        let info = infos.iter().find(|i| i.name == "cycle").unwrap();
        assert!(info.error.is_none());
        assert_eq!(info.tasks_count, 1);
    }

    /// Tests that the watcher's own error-setting code path is exercised when a
    /// workspace is already broken at startup: the watcher retries immediately,
    /// fails again, and leaves a non-empty error message.
    #[tokio::test]
    async fn test_watcher_sets_error_on_continued_failure() {
        use tokio_util::sync::CancellationToken;

        // Point at a path that does not exist — initial load fails.
        let temp = TempDir::new().unwrap();
        let ws_path = temp.path().join("workspace");
        let ws_path_str = ws_path.to_str().unwrap().to_string();

        let mut defs = HashMap::new();
        defs.insert(
            "broken".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: ws_path_str,
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert!(mgr.get_config("broken").await.is_none());
        let infos = mgr.list_workspace_info().await;
        assert!(infos[0].error.is_some());

        // Start watchers — the errored entry will retry immediately, fail again,
        // and update (or keep) the error message.
        let cancel_token = CancellationToken::new();
        mgr.start_watchers(cancel_token.clone(), None);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel_token.cancel();

        // Still errored after the watcher ran.
        assert!(mgr.get_config("broken").await.is_none());
        let infos = mgr.list_workspace_info().await;
        let info = infos.iter().find(|i| i.name == "broken").unwrap();
        assert!(info.error.is_some());
        assert!(
            !info.error.as_ref().unwrap().is_empty(),
            "error message should be non-empty"
        );
    }

    /// Tests that `get_revision()` returns `None` for a workspace in the error
    /// state, and recovers to `Some` once the workspace is healthy again.
    #[tokio::test]
    async fn test_get_revision_none_for_errored_workspace() {
        let temp = create_test_workspace_dir();
        let ws_path = temp.path().to_str().unwrap().to_string();
        let mut defs = HashMap::new();
        defs.insert(
            "ws".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: ws_path.clone(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        // Initially healthy — revision is available.
        assert!(
            mgr.get_revision("ws").is_some(),
            "healthy workspace should have a revision"
        );

        // Break the workspace and reload to enter the error state.
        fs::remove_dir_all(&ws_path).unwrap();
        let _ = mgr.reload("ws").await;

        // Revision must be None while the workspace is errored.
        assert!(
            mgr.get_revision("ws").is_none(),
            "get_revision should return None for an errored workspace"
        );

        // Fix the workspace and reload to clear the error.
        fs::create_dir_all(format!("{ws_path}/.workflows")).unwrap();
        fs::write(
            format!("{ws_path}/.workflows/test.yaml"),
            "actions:\n  a:\n    type: script\n    script: echo ok\ntasks:\n  t:\n    flow:\n      s:\n        action: a\n",
        )
        .unwrap();
        mgr.reload("ws").await.unwrap();

        // Revision should be available again.
        assert!(
            mgr.get_revision("ws").is_some(),
            "revision should be Some after workspace recovers"
        );
    }

    // ─── reload_for_api ────────────────────────────────────────────────

    /// Helper: build a single-workspace manager backed by a real folder so
    /// reload calls hit the same load path the API uses.
    async fn make_reloadable_manager() -> (WorkspaceManager, TempDir) {
        let temp = create_test_workspace_dir();
        let path = temp.path().to_str().unwrap().to_string();
        let mut defs = HashMap::new();
        defs.insert(
            "ws".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path,
            },
        );
        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        (mgr, temp)
    }

    #[tokio::test]
    async fn test_reload_for_api_returns_not_found_for_unknown_workspace() {
        let (mgr, _temp) = make_reloadable_manager().await;
        let result = mgr.reload_for_api("nope", Duration::ZERO).await;
        match result {
            Err(ReloadApiError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_reload_for_api_succeeds_with_zero_cooldown() {
        let (mgr, _temp) = make_reloadable_manager().await;
        // No previous reload → no cooldown to enforce. Should succeed.
        mgr.reload_for_api("ws", Duration::ZERO)
            .await
            .expect("first reload should succeed");
        // Second back-to-back call with zero cooldown must also succeed.
        mgr.reload_for_api("ws", Duration::ZERO)
            .await
            .expect("second reload with zero cooldown should succeed");
    }

    #[tokio::test]
    async fn test_reload_for_api_enforces_cooldown() {
        let (mgr, _temp) = make_reloadable_manager().await;
        // Prime the timestamp with a successful reload.
        mgr.reload_for_api("ws", Duration::ZERO).await.unwrap();

        // Same workspace within cooldown window → 429-shaped error.
        let result = mgr.reload_for_api("ws", Duration::from_secs(60)).await;
        match result {
            Err(ReloadApiError::Cooldown { retry_after_secs }) => {
                assert!(
                    retry_after_secs > 0 && retry_after_secs <= 60,
                    "retry_after_secs should be in (0, 60], got {retry_after_secs}"
                );
            }
            other => panic!("expected Cooldown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_reload_for_api_failure_still_blocks_cooldown_window() {
        // A reload that fails (e.g. transient git/folder error) must still
        // update the cooldown timestamp — otherwise a tight retry loop on a
        // broken source defeats the rate limit.
        let (mgr, temp) = make_reloadable_manager().await;
        // Break the workspace so the next reload fails.
        let ws_path = temp.path().to_path_buf();
        fs::remove_dir_all(&ws_path).unwrap();
        let _ = mgr.reload_for_api("ws", Duration::ZERO).await; // populates timestamp regardless

        let result = mgr.reload_for_api("ws", Duration::from_secs(60)).await;
        assert!(
            matches!(result, Err(ReloadApiError::Cooldown { .. })),
            "failed reload should still block subsequent calls within cooldown: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_reload_for_api_in_flight_call_surfaces_as_cooldown() {
        // When a reload is already running on another task, a concurrent
        // refresh-API caller must NOT queue behind it (that would tie up a
        // worker on a slow git fetch). Instead it should surface as a
        // Cooldown immediately. We simulate "already in flight" by manually
        // grabbing the per-entry mutex.
        let (mgr, _temp) = make_reloadable_manager().await;
        let entry = mgr.entry("ws").unwrap();
        let _guard = entry.exec().lock_owned().await;

        let result = mgr.reload_for_api("ws", Duration::from_secs(30)).await;
        match result {
            Err(ReloadApiError::Cooldown { retry_after_secs }) => {
                assert!(retry_after_secs >= 1);
            }
            other => panic!("expected Cooldown (in-flight), got {other:?}"),
        }
    }

    // ─── concurrent startup loading ────────────────────────────────────

    /// Minimal copy of `git::tests::create_bare_repo` (private to that
    /// module) — creates a bare git repo with an initial commit on `main`
    /// containing the given files, returning (TempDir, file:// URL).
    fn create_bare_repo_for_test(files: &[(&str, &str)]) -> (TempDir, String) {
        let bare_dir = TempDir::new().unwrap();
        let bare_repo = git2::Repository::init_bare(bare_dir.path()).unwrap();

        let mut tb = bare_repo.treebuilder(None).unwrap();
        for &(name, content) in files {
            let oid = bare_repo.blob(content.as_bytes()).unwrap();
            tb.insert(name, oid, 0o100644).unwrap();
        }
        let tree_oid = tb.write().unwrap();
        let tree = bare_repo.find_tree(tree_oid).unwrap();

        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let commit_oid = bare_repo
            .commit(Some("refs/heads/main"), &sig, &sig, "initial", &tree, &[])
            .unwrap();

        bare_repo
            .reference("HEAD", commit_oid, true, "set HEAD")
            .ok();
        bare_repo.set_head("refs/heads/main").unwrap();

        let url = format!("file://{}", bare_dir.path().display());
        (bare_dir, url)
    }

    /// `WorkspaceManager::new` must load workspaces concurrently: two git
    /// sources (backed by local bare repos, exercising the real
    /// `block_in_place` clone path) plus a folder source pointing at a
    /// non-existent path all load correctly, and none blocks the others.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_new_loads_git_and_folder_workspaces_concurrently() {
        let (_bare1, url1) = create_bare_repo_for_test(&[(
            "deploy.yaml",
            "actions:\n  a:\n    type: script\n    script: echo hi\n",
        )]);
        let (_bare2, url2) = create_bare_repo_for_test(&[(
            "deploy.yaml",
            "actions:\n  b:\n    type: script\n    script: echo hi\n",
        )]);

        // Unique names so the on-disk clone dir (temp_dir/stroem/git/{name})
        // doesn't collide with other tests or runs.
        let git_one = format!("git-one-{}", uuid::Uuid::new_v4());
        let git_two = format!("git-two-{}", uuid::Uuid::new_v4());

        let mut defs = HashMap::new();
        defs.insert(
            git_one.clone(),
            WorkspaceSourceDef::Git {
                triggers: true,
                url: url1,
                git_ref: "main".to_string(),
                poll_interval_secs: 60,
                auth: None,
            },
        );
        defs.insert(
            git_two.clone(),
            WorkspaceSourceDef::Git {
                triggers: true,
                url: url2,
                git_ref: "main".to_string(),
                poll_interval_secs: 60,
                auth: None,
            },
        );
        defs.insert(
            "missing-folder".to_string(),
            WorkspaceSourceDef::Folder {
                triggers: true,
                path: "/nonexistent/path/for/concurrent/test".to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;

        // All three configured, whether healthy or not.
        let mut configured = mgr.configured_names();
        configured.sort();
        let mut expected = vec![
            git_one.clone(),
            git_two.clone(),
            "missing-folder".to_string(),
        ];
        expected.sort();
        assert_eq!(configured, expected);

        // Both git workspaces loaded successfully.
        assert!(
            mgr.get_config(&git_one).await.is_some(),
            "git-one should have loaded"
        );
        assert!(
            mgr.get_config(&git_two).await.is_some(),
            "git-two should have loaded"
        );

        // The folder workspace has a load_error.
        assert!(mgr.get_config("missing-folder").await.is_none());
        let infos = mgr.list_workspace_info().await;
        let missing = infos
            .iter()
            .find(|i| i.name == "missing-folder")
            .expect("missing-folder should appear in workspace info");
        assert!(missing.error.is_some());
    }

    // ─── Task 8: three-way entry state, single writer ───────────────────

    fn cfg_with_action(action: &str) -> WorkspaceConfig {
        serde_yaml::from_str(&format!(
            "actions:\n  {action}:\n    type: script\n    script: echo hi\n"
        ))
        .unwrap()
    }

    fn one_ws(rev: &str) -> WorkspaceManager {
        WorkspaceManager::from_configs(vec![(
            "ws".to_string(),
            cfg_with_action("a"),
            Some(rev.to_string()),
        )])
    }

    #[tokio::test]
    async fn failed_load_publishes_only_the_error() {
        let mgr = one_ws("rev-a");
        let entry = mgr.entry("ws").unwrap();
        let policy = mgr.settings.policy(entry.poll_interval());
        let before = entry.published();
        let err = entry
            .apply_load_result(
                availability::Caller::External,
                None,
                Err(anyhow::anyhow!("secret render failed")),
                &HashMap::new(),
                &policy,
                Instant::now(),
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("secret render failed"));
        let after = entry.published();
        assert!(
            Arc::ptr_eq(&before.config, &after.config),
            "config must not change"
        );
        assert_eq!(
            after.revision.as_deref(),
            Some("rev-a"),
            "revision must not change"
        );
        assert_eq!(after.error.as_deref(), Some("secret render failed"));
        assert!(mgr.get_config("ws").await.is_none());
        assert!(mgr.get_revision("ws").is_none());
        assert!(
            entry.availability().is_errored(),
            "external failure must land in Errored"
        );
    }

    #[tokio::test]
    async fn successful_load_publishes_config_and_revision_together() {
        let mgr = one_ws("rev-a");
        let entry = mgr.entry("ws").unwrap();
        let policy = mgr.settings.policy(entry.poll_interval());
        let ok = entry
            .apply_load_result(
                availability::Caller::Watcher,
                None,
                Ok(LoadOutcome {
                    config: cfg_with_action("b"),
                    warnings: vec!["w".to_string()],
                    revision: Some("rev-b".to_string()),
                }),
                &HashMap::new(),
                &policy,
                Instant::now(),
            )
            .unwrap();
        assert!(ok.revision_changed);
        assert_eq!(mgr.get_revision("ws").as_deref(), Some("rev-b"));
        assert!(mgr
            .get_config("ws")
            .await
            .unwrap()
            .actions
            .contains_key("b"));
        assert_eq!(
            mgr.list_workspace_info().await[0].warnings,
            vec!["w".to_string()]
        );
    }

    #[tokio::test]
    async fn readers_do_not_wait_for_the_execution_mutex() {
        let mgr = one_ws("rev-a");
        let entry = mgr.entry("ws").unwrap();
        let _held = entry.exec().lock_owned().await;
        let cfg = tokio::time::timeout(Duration::from_millis(200), mgr.get_config("ws"))
            .await
            .expect("get_config must not wait for a load");
        assert!(cfg.is_some());
        assert!(mgr.get_path("ws").is_some());
        assert_eq!(mgr.get_revision("ws").as_deref(), Some("rev-a"));
        assert_eq!(mgr.list_workspace_info().await.len(), 1);
    }
}
