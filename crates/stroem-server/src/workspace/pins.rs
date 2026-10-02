//! Pinned workspace snapshots keyed by `(workspace, commit)` — spec § 5.
//!
//! Fully separate from the watcher path: its own bare repository per git
//! workspace under `pin_store.dir`, its own per-workspace write lock, its
//! own load permits. `GitSource`'s working clone, the exec mutex,
//! `Availability` and `apply_load_result` are never touched from here.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use stroem_common::budget::{is_deadline_exceeded, LoadBudget};
use stroem_common::git_ref::{parse_git_ref, short_sha_hint, GitRefSpec};
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_common::template::is_vals_failure;
use stroem_common::workspace_loader::is_sops_failure_warning;
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};

use super::availability::ReloadSettings;
use super::git::{checkout_builder, git_error, GitSource};
use super::library::{merge_libraries_into_workspace, ResolvedLibrary};
use crate::config::{GitAuthConfig, WorkspaceSourceDef};

/// Prefix of a dir under `{ws}/trees/` that is not a published checkout:
/// one in progress, or one moved aside by `evict` for deletion.
const TMP_PREFIX: &str = ".tmp-";
/// `pin_store.keep_recent_per_workspace` default (spec § 10).
pub const DEFAULT_KEEP_RECENT_PER_WORKSPACE: usize = 5;

/// Pin loads in flight at once on this replica. Separate from the
/// watchers' `MAX_CONCURRENT_WORKSPACE_LOADS` so creation-path pin loads
/// never starve them (spec § 5.3).
pub const MAX_CONCURRENT_PIN_LOADS: usize = 4;

/// A checked-out commit. Immutable once published (tmp dir + rename);
/// deleted only by `PinStore::evict`, once no caller holds it.
#[derive(Debug)]
pub struct PinnedTree {
    pub dir: PathBuf,
}

/// The config of one workspace at one commit. Immutable. Holding it
/// holds a lease on its checkout.
pub struct Pinned {
    pub config: Arc<WorkspaceConfig>,
    pub dir: PathBuf,
    /// `secrets` values + `secret: true` properties of connections typed
    /// in this workspace ONLY. Not a redaction set: foreign-typed
    /// connections need live configs. Redact with
    /// `WorkspaceManager::pin_redaction_values` (the complete set).
    pub secret_values: Vec<String>,
    _tree: Arc<PinnedTree>,
}

// Never prints `config` (secrets) or `secret_values`.
impl std::fmt::Debug for Pinned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pinned")
            .field("dir", &self.dir)
            .field(
                "secret_values",
                &format_args!("[REDACTED; {}]", self.secret_values.len()),
            )
            .finish_non_exhaustive()
    }
}

/// `(workspace, commit)`.
type Key = (String, String);

/// One cache entry. `OnceCell` is the single-flight: concurrent callers
/// await one initialisation; a failed one leaves the cell empty, so the
/// next caller retries (errors are never cached).
struct Slot<T> {
    cell: Arc<OnceCell<Arc<T>>>,
    last_used: Instant,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            cell: Arc::new(OnceCell::new()),
            last_used: Instant::now(),
        }
    }

    /// The lease (spec § 5.3). Both handles count: a caller holding the
    /// value, and one holding the cell — an initialisation in flight, or a
    /// single-flight waiter that has not cloned the value yet.
    fn in_use(&self) -> bool {
        Arc::strong_count(&self.cell) > 1
            || self
                .cell
                .get()
                .is_some_and(|value| Arc::strong_count(value) > 1)
    }
}

/// The slot's cell, created on first use, its recency bumped. The map lock
/// is released on return: it is never held across an await.
fn slot_cell<T>(
    map: &Mutex<HashMap<Key, Slot<T>>>,
    ws: &str,
    commit: &str,
) -> Arc<OnceCell<Arc<T>>> {
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    let slot = map
        .entry((ws.to_string(), commit.to_string()))
        .or_insert_with(Slot::new);
    slot.last_used = Instant::now();
    Arc::clone(&slot.cell)
}

/// The `n` most recently used LOADED keys of every workspace. A failed or
/// in-flight slot is never "recent": it would displace a real pin.
fn most_recent<T>(map: &HashMap<Key, Slot<T>>, n: usize) -> HashSet<Key> {
    let mut by_ws: HashMap<&str, Vec<(&Key, Instant)>> = HashMap::new();
    for (key, slot) in map.iter().filter(|(_, slot)| slot.cell.initialized()) {
        by_ws
            .entry(key.0.as_str())
            .or_default()
            .push((key, slot.last_used));
    }
    let mut out = HashSet::new();
    for (_, mut keys) in by_ws {
        keys.sort_by_key(|&(_, used)| std::cmp::Reverse(used));
        out.extend(keys.into_iter().take(n).map(|(k, _)| k.clone()));
    }
    out
}

/// The keys `evict` may drop: not kept, not recent, not leased.
fn evictable<T>(map: &HashMap<Key, Slot<T>>, keep: &HashSet<Key>, keep_recent: usize) -> Vec<Key> {
    let recent = most_recent(map, keep_recent);
    map.iter()
        .filter(|(key, slot)| !keep.contains(*key) && !recent.contains(*key) && !slot.in_use())
        .map(|(key, _)| key.clone())
        .collect()
}

fn result_label<T>(r: &Result<T, PinError>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(PinError::NotGit { .. }) => "not_git",
        Err(PinError::RefNotFound { .. }) => "ref_not_found",
        Err(PinError::CommitNotFound { .. }) => "commit_not_found",
        Err(PinError::PinLoadFailed { .. }) => "load_failed",
        Err(PinError::PinUnavailable { .. }) => "unavailable",
    }
}

/// A ref of one workspace resolved to a commit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pin {
    pub workspace: String,
    /// The ref exactly as written in YAML.
    pub git_ref: String,
    /// 40-hex lowercase commit sha.
    pub commit: String,
}

impl Pin {
    pub fn pin_ref(&self) -> PinRef {
        PinRef {
            git_ref: self.git_ref.clone(),
            commit: self.commit.clone(),
        }
    }
}

/// What a job or step row stores: the ref as written plus its commit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PinRef {
    pub git_ref: String,
    pub commit: String,
}

/// Where one git workspace's pins come from.
#[derive(Clone)]
pub struct PinSource {
    pub url: String,
    pub auth: Option<GitAuthConfig>,
    /// TTL of the cached ref listing (spec § 5.2 — same freshness as the
    /// default branch's watcher).
    pub poll_interval: Duration,
}

// Hand-written: `GitAuthConfig` carries keys and tokens.
impl std::fmt::Debug for PinSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinSource")
            .field("url", &self.url)
            .field("auth", &self.auth.as_ref().map(|a| a.auth_type.as_str()))
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

/// One `PinSource` per configured git workspace. Folder workspaces have no
/// history and get none, so a `ref` on them is `PinError::NotGit`.
pub fn pin_sources(defs: &HashMap<String, WorkspaceSourceDef>) -> HashMap<String, PinSource> {
    defs.iter()
        .filter_map(|(name, def)| match def {
            WorkspaceSourceDef::Git {
                url,
                auth,
                poll_interval_secs,
                ..
            } => Some((
                name.clone(),
                PinSource {
                    url: url.clone(),
                    auth: auth.clone(),
                    poll_interval: Duration::from_secs(*poll_interval_secs),
                },
            )),
            WorkspaceSourceDef::Folder { .. } => None,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct PinStoreConfig {
    pub dir: PathBuf,
    pub keep_recent_per_workspace: usize,
}

impl PinStoreConfig {
    /// `<temp>/stroem/pins` — same lifetime as the watcher's clones.
    pub fn default_dir() -> PathBuf {
        std::env::temp_dir().join("stroem").join("pins")
    }
}

impl Default for PinStoreConfig {
    fn default() -> Self {
        Self {
            dir: Self::default_dir(),
            keep_recent_per_workspace: DEFAULT_KEEP_RECENT_PER_WORKSPACE,
        }
    }
}

/// Typed pin failures. Callers classify with `downcast_ref::<PinError>()`,
/// never by message text (spec § 5.3, § 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    NotGit {
        workspace: String,
    },
    RefNotFound {
        workspace: String,
        git_ref: String,
    },
    CommitNotFound {
        workspace: String,
        commit: String,
    },
    /// The config at that commit does not load. Permanent.
    PinLoadFailed {
        workspace: String,
        commit: String,
        message: String,
    },
    /// Network, auth, timeout, sops/vals. Transient.
    PinUnavailable {
        workspace: String,
        message: String,
    },
}

impl PinError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::PinUnavailable { .. })
    }
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGit { workspace } => write!(
                f,
                "workspace '{workspace}' is not a git workspace; `ref` needs a git source"
            ),
            Self::RefNotFound { workspace, git_ref } => {
                write!(f, "ref '{git_ref}' not found in workspace '{workspace}'")?;
                if let Some(hint) = short_sha_hint(git_ref) {
                    write!(f, " ({hint})")?;
                }
                Ok(())
            }
            Self::CommitNotFound { workspace, commit } => {
                write!(f, "commit {commit} not found in workspace '{workspace}'")
            }
            Self::PinLoadFailed {
                workspace,
                commit,
                message,
            } => write!(
                f,
                "workspace '{workspace}' at commit {commit} does not load: {message}"
            ),
            Self::PinUnavailable { workspace, message } => write!(
                f,
                "workspace '{workspace}' is not available at the requested ref yet: {message}"
            ),
        }
    }
}

impl std::error::Error for PinError {}

/// `{ws}@{ref} ({short sha})`: how a `[pin]` line names a pin.
pub fn pin_label(ws: &str, pin: &PinRef) -> String {
    format!("{ws}@{} ({})", pin.git_ref, short_sha(&pin.commit))
}

fn short_sha(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// `[pin] {ws}@{ref} ({short sha}) cannot be loaded: …` for a pin that does
/// not load (spec § 7.3). `err` comes from
/// [`WorkspaceManager::config_for_user`](super::WorkspaceManager::config_for_user):
/// a withheld load failure is its own fixed sentence, and any other error's
/// text is appended (it carries no config text; callers scrub the line).
pub fn cannot_be_loaded(ws: &str, pin: &PinRef, err: &anyhow::Error) -> String {
    if err.downcast_ref::<PinLoadWithheld>().is_some() {
        err.to_string()
    } else {
        format!("[pin] {} cannot be loaded: {:#}", pin_label(ws, pin), err)
    }
}

/// All a user ever sees of a [`PinError::PinLoadFailed`] (T6 review #9).
/// Its message is the raw loader chain and can quote secret values, so it
/// goes only to the server log, scrubbed
/// (`WorkspaceManager::pin_error_for_user`). Permanent, like the error it
/// replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinLoadWithheld {
    label: String,
}

impl PinLoadWithheld {
    pub fn new(ws: &str, pin: &PinRef) -> Self {
        Self {
            label: pin_label(ws, pin),
        }
    }

    /// For a failure whose ref is not known (only the commit).
    pub fn for_commit(ws: &str, commit: &str) -> Self {
        Self {
            label: format!("{ws} ({})", short_sha(commit)),
        }
    }
}

impl std::fmt::Display for PinLoadWithheld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[pin] {} cannot be loaded: its configuration does not load",
            self.label
        )
    }
}

impl std::error::Error for PinLoadWithheld {}

fn unavailable(ws: &str, e: impl std::fmt::Display) -> PinError {
    PinError::PinUnavailable {
        workspace: ws.to_string(),
        message: format!("{e:#}"),
    }
}

/// A full sha in canonical form, or `CommitNotFound`.
fn normalize_commit(ws: &str, commit: &str) -> Result<String, PinError> {
    if commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(commit.to_ascii_lowercase())
    } else {
        Err(PinError::CommitNotFound {
            workspace: ws.to_string(),
            commit: commit.to_string(),
        })
    }
}

struct Listing {
    fetched_at: Instant,
    /// Full ref name → commit sha (annotated tags peeled when advertised).
    refs: Arc<HashMap<String, String>>,
}

/// The ref listing `resolve` decides on.
struct Listed {
    refs: Arc<HashMap<String, String>>,
    /// Served from the cache within the TTL without asking the remote: a ref
    /// missing from it may have been pushed since.
    from_cache: bool,
    /// Set when listing the remote just failed and `refs` is the last
    /// successful listing: a ref missing from it is not proof of absence.
    refresh_error: Option<String>,
}

/// How `commit_for_ref` arrived at its commit.
enum RefCommit {
    /// The advertised object was already in the local repo.
    Advertised(String),
    /// The ref was fetched by name and its local tip adopted.
    Adopted(String),
}

pub struct PinStore {
    cfg: PinStoreConfig,
    sources: HashMap<String, PinSource>,
    libraries: Arc<HashMap<String, ResolvedLibrary>>,
    settings: ReloadSettings,
    /// Held for the store's lifetime: one process per `pin_store.dir`.
    _dir_lock: Option<File>,
    /// Serialises bare-repo writes (init, fetch) per workspace.
    repo_locks: HashMap<String, Arc<Mutex<()>>>,
    /// Serialises ref resolution per workspace — listing check, fetch,
    /// adoption — and with it the ls-remote. Concurrent resolves of a moving
    /// ref can then never hand out an older commit after a newer one, nor
    /// overwrite a newer listing or an adoption with an older listing.
    resolve_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    listings: Mutex<HashMap<String, Listing>>,
    trees: Mutex<HashMap<Key, Slot<PinnedTree>>>,
    configs: Mutex<HashMap<Key, Slot<Pinned>>>,
    /// `MAX_CONCURRENT_PIN_LOADS` — never the watchers' semaphore. A permit
    /// moves into the blocking work it admits, so it bounds real work even
    /// after the caller that started it is dropped.
    permits: Arc<Semaphore>,
    /// Config loads started (test support).
    loads: AtomicUsize,
    /// Test hook: pretend the remote refuses want-by-SHA.
    #[cfg(test)]
    skip_fetch_by_sha: AtomicBool,
    /// Test hook: holds the blocking pin work at its start while closed.
    /// Compiled in (integration tests link this crate without `cfg(test)`);
    /// never closed outside [`PinStore::hold_loads_for_test`].
    gate: Arc<TestGate>,
}

/// Test hook: blocking pin work (fetch + checkout, config load) waits at
/// its start while the gate is closed.
#[derive(Default)]
struct TestGate {
    closed: Mutex<bool>,
    changed: std::sync::Condvar,
    /// Blocking pin work units that reached the gate.
    entered: AtomicUsize,
}

/// Holds every blocking pin load of one store at its start until dropped
/// ([`PinStore::hold_loads_for_test`]). Opening on drop means a failing
/// assertion never leaves a blocking thread parked (the runtime would wait
/// on it forever).
#[doc(hidden)]
pub struct PinLoadHold(Arc<TestGate>);

impl PinLoadHold {
    /// Blocking pin work units that reached the gate since the hold began.
    pub fn entered(&self) -> usize {
        self.0.entered.load(Ordering::SeqCst)
    }
}

impl Drop for PinLoadHold {
    fn drop(&mut self) {
        self.0.set_closed(false);
    }
}

impl TestGate {
    fn pass(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
        while *closed {
            closed = self.changed.wait(closed).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn set_closed(&self, closed: bool) {
        *self.closed.lock().unwrap_or_else(|e| e.into_inner()) = closed;
        self.changed.notify_all();
    }
}

impl std::fmt::Debug for PinStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut workspaces: Vec<&str> = self.sources.keys().map(String::as_str).collect();
        workspaces.sort_unstable();
        f.debug_struct("PinStore")
            .field("dir", &self.cfg.dir)
            .field("workspaces", &workspaces)
            .field("libraries", &self.libraries.len())
            .finish()
    }
}

impl PinStore {
    /// Open the store: create `cfg.dir`, take the exclusive lock on
    /// `{dir}/.lock` (a second process on the same dir refuses to start),
    /// and remove every checkout a previous process left behind.
    pub fn open(
        cfg: PinStoreConfig,
        sources: HashMap<String, PinSource>,
        libraries: Arc<HashMap<String, ResolvedLibrary>>,
        settings: ReloadSettings,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.dir)
            .with_context(|| format!("create pin_store.dir {}", cfg.dir.display()))?;
        let lock_path = cfg.dir.join(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "pin_store.dir {} is in use by another process",
                cfg.dir.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("lock {}", lock_path.display()))
            }
        }
        remove_leftover_checkouts(&cfg.dir);
        let repo_locks = sources
            .keys()
            .map(|name| (name.clone(), Arc::new(Mutex::new(()))))
            .collect();
        let resolve_locks = sources
            .keys()
            .map(|name| (name.clone(), Arc::new(tokio::sync::Mutex::new(()))))
            .collect();
        Ok(Self {
            cfg,
            sources,
            libraries,
            settings,
            _dir_lock: Some(lock),
            repo_locks,
            resolve_locks,
            listings: Mutex::new(HashMap::new()),
            trees: Mutex::new(HashMap::new()),
            configs: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_PIN_LOADS)),
            loads: AtomicUsize::new(0),
            #[cfg(test)]
            skip_fetch_by_sha: AtomicBool::new(false),
            gate: Arc::default(),
        })
    }

    /// A store with no git sources: every pin is `NotGit`. No directory is
    /// created or locked.
    pub fn disabled() -> Self {
        Self {
            cfg: PinStoreConfig::default(),
            sources: HashMap::new(),
            libraries: Arc::new(HashMap::new()),
            settings: ReloadSettings::default(),
            _dir_lock: None,
            repo_locks: HashMap::new(),
            resolve_locks: HashMap::new(),
            listings: Mutex::new(HashMap::new()),
            trees: Mutex::new(HashMap::new()),
            configs: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_PIN_LOADS)),
            loads: AtomicUsize::new(0),
            #[cfg(test)]
            skip_fetch_by_sha: AtomicBool::new(false),
            gate: Arc::default(),
        }
    }

    pub fn is_git(&self, ws: &str) -> bool {
        self.sources.contains_key(ws)
    }

    pub fn has_sources(&self) -> bool {
        !self.sources.is_empty()
    }

    /// Names of the workspaces that have a pin source, sorted.
    pub fn source_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.sources.keys().cloned().collect();
        names.sort();
        names
    }

    fn source(&self, ws: &str) -> Result<PinSource, PinError> {
        self.sources
            .get(ws)
            .cloned()
            .ok_or_else(|| PinError::NotGit {
                workspace: ws.to_string(),
            })
    }

    fn repo_dir(&self, ws: &str) -> PathBuf {
        self.cfg.dir.join(ws).join("repo.git")
    }

    /// `{dir}/{ws}/trees`: one published checkout per key, named by its
    /// commit, plus `TMP_PREFIX` dirs.
    fn trees_dir(&self, ws: &str) -> PathBuf {
        self.cfg.dir.join(ws).join("trees")
    }

    /// Only called after `source(ws)` succeeded, so the entry exists.
    fn repo_lock(&self, ws: &str) -> Arc<Mutex<()>> {
        Arc::clone(&self.repo_locks[ws])
    }

    #[cfg(not(test))]
    fn fetch_by_sha_enabled(&self) -> bool {
        true
    }

    #[cfg(test)]
    fn fetch_by_sha_enabled(&self) -> bool {
        !self.skip_fetch_by_sha.load(Ordering::SeqCst)
    }

    /// Resolve `git_ref` (§ 4.2 forms) of `ws` to a commit that is present
    /// in the local bare repo when this returns.
    #[tracing::instrument(skip_all, fields(workspace = %ws, git_ref = %git_ref))]
    pub async fn resolve(&self, ws: &str, git_ref: &str) -> Result<Pin, PinError> {
        let src = self.source(ws)?;
        let not_found = || PinError::RefNotFound {
            workspace: ws.to_string(),
            git_ref: git_ref.to_string(),
        };
        let candidates = match parse_git_ref(git_ref).map_err(|_| not_found())? {
            GitRefSpec::Commit(sha) => {
                let commit = self.ensure_commit(ws, &src, &sha).await?;
                return Ok(Pin {
                    workspace: ws.to_string(),
                    git_ref: git_ref.to_string(),
                    commit,
                });
            }
            GitRefSpec::Branch(n) => vec![format!("refs/heads/{n}")],
            GitRefSpec::Tag(n) => vec![format!("refs/tags/{n}")],
            GitRefSpec::Name(n) => vec![format!("refs/heads/{n}"), format!("refs/tags/{n}")],
        };
        let lookup = |refs: &HashMap<String, String>| {
            candidates
                .iter()
                .find_map(|c| refs.get(c).map(|oid| (c.clone(), oid.clone())))
        };
        // `source(ws)` succeeded, so the entry exists.
        let serial = Arc::clone(&self.resolve_locks[ws]);
        let _serial = serial.lock().await;
        let mut listed = self.listing(ws, &src, false).await?;
        let mut found = lookup(&listed.refs);
        if found.is_none() && listed.from_cache {
            // The ref may have been pushed since the cached listing: ask the
            // remote once before answering.
            listed = self.listing(ws, &src, true).await?;
            found = lookup(&listed.refs);
        }
        let Some((name, advertised)) = found else {
            // Absence is only proof when the remote was just listed.
            return Err(match listed.refresh_error {
                Some(e) => unavailable(
                    ws,
                    format!(
                        "ref '{git_ref}' is not in the last listing, and listing refs failed: {e}"
                    ),
                ),
                None => not_found(),
            });
        };
        drop(listed);
        let repo_dir = self.repo_dir(ws);
        let lock = self.repo_lock(ws);
        let budget = LoadBudget::from_now(self.settings.load_timeout);
        let (ws_owned, ref_owned, name_owned) = (ws.to_string(), git_ref.to_string(), name.clone());
        let resolved = tokio::task::spawn_blocking(move || {
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            commit_for_ref(
                &ws_owned,
                &ref_owned,
                &repo_dir,
                &src,
                &name_owned,
                &advertised,
                &budget,
            )
        })
        .await
        .map_err(|e| unavailable(ws, e))??;
        let commit = match resolved {
            RefCommit::Advertised(commit) => commit,
            RefCommit::Adopted(commit) => {
                self.adopt_into_listing(ws, &name, &commit);
                commit
            }
        };
        Ok(Pin {
            workspace: ws.to_string(),
            git_ref: git_ref.to_string(),
            commit,
        })
    }

    /// Cached ls-remote listing (TTL = the source's poll interval; `force`
    /// bypasses it). On failure the last listing is served with a warning
    /// and NOT refreshed, so the next call retries; with no listing it is
    /// `PinUnavailable`. Only called under the workspace's resolve lock, so
    /// one ls-remote per workspace runs at a time and listings are written
    /// in order.
    async fn listing(&self, ws: &str, src: &PinSource, force: bool) -> Result<Listed, PinError> {
        if !force {
            let listings = self.listings.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(l) = listings.get(ws) {
                if l.fetched_at.elapsed() < src.poll_interval {
                    return Ok(Listed {
                        refs: Arc::clone(&l.refs),
                        from_cache: true,
                        refresh_error: None,
                    });
                }
            }
        }
        let budget = LoadBudget::from_now(self.settings.peek_timeout);
        let src_owned = src.clone();
        let result = tokio::task::spawn_blocking(move || ls_remote(&src_owned, &budget))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r);
        let mut listings = self.listings.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(refs) => {
                let refs = Arc::new(refs);
                listings.insert(
                    ws.to_string(),
                    Listing {
                        fetched_at: Instant::now(),
                        refs: Arc::clone(&refs),
                    },
                );
                Ok(Listed {
                    refs,
                    from_cache: false,
                    refresh_error: None,
                })
            }
            Err(e) => match listings.get(ws) {
                Some(l) => {
                    tracing::warn!(
                        "Pin store: listing refs of workspace '{ws}' failed ({e:#}); \
                         using the listing from {:?} ago",
                        l.fetched_at.elapsed()
                    );
                    Ok(Listed {
                        refs: Arc::clone(&l.refs),
                        from_cache: false,
                        refresh_error: Some(format!("{e:#}")),
                    })
                }
                None => Err(unavailable(ws, e)),
            },
        }
    }

    /// Record an adopted tip in the cached listing (spec § 5.2), so later
    /// resolutions within the TTL never go back to the stale advertised
    /// object. Runs under the workspace's resolve lock: no listing can have
    /// been written since `resolve` read the one it adopted against.
    fn adopt_into_listing(&self, ws: &str, name: &str, adopted: &str) {
        let mut listings = self.listings.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = listings.get_mut(ws) {
            Arc::make_mut(&mut l.refs).insert(name.to_string(), adopted.to_string());
        }
    }

    /// Make `commit` present in the local bare repo (fetch by SHA, falling
    /// back to all heads and tags) and return the commit it peels to: a
    /// 40-hex SHA may name an annotated tag object.
    async fn ensure_commit(
        &self,
        ws: &str,
        src: &PinSource,
        commit: &str,
    ) -> Result<String, PinError> {
        let commit = normalize_commit(ws, commit)?;
        let repo_dir = self.repo_dir(ws);
        let lock = self.repo_lock(ws);
        let by_sha = self.fetch_by_sha_enabled();
        let budget = LoadBudget::from_now(self.settings.load_timeout);
        let (ws_owned, src) = (ws.to_string(), src.clone());
        tokio::task::spawn_blocking(move || {
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            ensure_commit_local(&ws_owned, &repo_dir, &src, &commit, by_sha, &budget)
        })
        .await
        .map_err(|e| unavailable(ws, e))?
    }

    /// A permit for one pin load, waiting at most until `budget`'s deadline.
    /// Owned: the caller moves it into its `spawn_blocking` closure, so it is
    /// released when the blocking work returns, not when the caller is
    /// dropped (the live path's rule).
    async fn permit(
        &self,
        ws: &str,
        budget: &LoadBudget,
    ) -> Result<OwnedSemaphorePermit, PinError> {
        let acquire = Arc::clone(&self.permits).acquire_owned();
        let acquired = match budget.deadline() {
            Some(deadline) => tokio::time::timeout_at(deadline.into(), acquire)
                .await
                .map_err(|_elapsed| unavailable(ws, "pin load queue saturated"))?,
            None => acquire.await,
        };
        acquired.map_err(|_closed| unavailable(ws, "pin load permits closed"))
    }

    /// The commit's files at `{dir}/{ws}/trees/{commit}`, checked out on
    /// first use. No config load, no sops/vals (spec § 5.3 `ensure_tree`).
    #[tracing::instrument(skip_all, fields(workspace = %ws, commit = %commit))]
    pub async fn ensure_tree(&self, ws: &str, commit: &str) -> Result<Arc<PinnedTree>, PinError> {
        self.tree(ws, commit, LoadBudget::from_now(self.settings.load_timeout))
            .await
    }

    /// `ensure_tree` under the caller's deadline: `ensure` passes the one
    /// budget that covers its whole load.
    async fn tree(
        &self,
        ws: &str,
        commit: &str,
        budget: LoadBudget,
    ) -> Result<Arc<PinnedTree>, PinError> {
        let src = self.source(ws)?;
        let commit = normalize_commit(ws, commit)?;
        let cell = slot_cell(&self.trees, ws, &commit);
        let tree = cell
            .get_or_try_init(|| async move {
                // Handed the cell after a failed attempt, a waiter may
                // already be past the deadline it started with.
                budget.check().map_err(|e| unavailable(ws, e))?;
                let permit = self.permit(ws, &budget).await?;
                let repo_dir = self.repo_dir(ws);
                let trees_dir = self.trees_dir(ws);
                let lock = self.repo_lock(ws);
                let by_sha = self.fetch_by_sha_enabled();
                let ws_owned = ws.to_string();
                let gate = Arc::clone(&self.gate);
                let dir = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    gate.pass();
                    let peeled = {
                        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        ensure_commit_local(&ws_owned, &repo_dir, &src, &commit, by_sha, &budget)?
                    };
                    // Checkout reads objects only: outside the write lock.
                    checkout_commit(&ws_owned, &repo_dir, &trees_dir, &commit, &peeled, &budget)
                })
                .await
                .map_err(|e| unavailable(ws, e))??;
                Ok::<_, PinError>(Arc::new(PinnedTree { dir }))
            })
            .await?;
        Ok(Arc::clone(tree))
    }

    /// The config of `ws` at `commit` (immutable, cached, single-flight).
    #[tracing::instrument(skip_all, fields(workspace = %ws, commit = %commit))]
    pub async fn ensure(&self, ws: &str, commit: &str) -> Result<Arc<Pinned>, PinError> {
        let commit = normalize_commit(ws, commit)?;
        // ONE deadline for the whole call — fetch, checkout, config — and it
        // runs while the call waits behind another caller's load, too.
        let budget = LoadBudget::from_now(self.settings.load_timeout);
        let cell = slot_cell(&self.configs, ws, &commit);
        let pinned = cell
            .get_or_try_init(|| async move {
                let result = match budget.check() {
                    // Handed the cell after a failed attempt, past its deadline.
                    Err(e) => Err(unavailable(ws, e)),
                    Ok(()) => self.load_pinned(ws, &commit, budget).await,
                };
                metrics::counter!(
                    crate::metrics::STROEM_PIN_LOADS_TOTAL,
                    "workspace" => ws.to_owned(),
                    "result" => result_label(&result),
                )
                .increment(1);
                result
            })
            .await?;
        Ok(Arc::clone(pinned))
    }

    async fn load_pinned(
        &self,
        ws: &str,
        commit: &str,
        budget: LoadBudget,
    ) -> Result<Arc<Pinned>, PinError> {
        // Tree first, permit second: the tree phase takes its own permit,
        // and nesting them could deadlock the semaphore.
        let tree = self.tree(ws, commit, budget).await?;
        let permit = self.permit(ws, &budget).await?;
        self.loads.fetch_add(1, Ordering::Relaxed);
        // The permit and a lease on the checkout move into the blocking
        // load: both outlive a caller dropped mid-load.
        let lease = Arc::clone(&tree);
        let gate = Arc::clone(&self.gate);
        let loaded = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            gate.pass();
            super::folder::load_folder_workspace_with(&lease.dir, &budget)
        })
        .await
        .map_err(|e| unavailable(ws, e))?;
        let mut config = classify_load(ws, commit, loaded)?;
        merge_libraries_into_workspace(&mut config, &self.libraries);
        let secret_values = local_secret_values(ws, &config);
        Ok(Arc::new(Pinned {
            config: Arc::new(config),
            dir: tree.dir.clone(),
            secret_values,
            _tree: tree,
        }))
    }

    /// Drop every cached config and checkout that is not in `keep`, not
    /// among the `keep_recent_per_workspace` most recently used loaded pins
    /// of its workspace, and not leased (spec § 10). Configs go first, so
    /// the tree leases they held are released in the same call.
    ///
    /// A checkout dir is moved aside (`.tmp-evict-*`) under the trees lock,
    /// in the same critical section that removes its entry: a later
    /// `ensure_tree` of that commit finds no dir and checks out afresh,
    /// never reusing one about to be deleted. The moved dirs are deleted
    /// after the lock is released. Blocking (dir deletion): call it on
    /// `spawn_blocking`, never on a runtime thread.
    pub fn evict(&self, keep: &HashSet<(String, String)>) {
        let n = self.cfg.keep_recent_per_workspace;
        let dropped: Vec<Slot<Pinned>> = {
            let mut configs = self.configs.lock().unwrap_or_else(|e| e.into_inner());
            evictable(&configs, keep, n)
                .into_iter()
                .filter_map(|key| configs.remove(&key))
                .collect()
        };
        let configs_dropped = dropped.len();
        // Releases their tree leases before the trees are examined.
        drop(dropped);
        let trash: Vec<PathBuf> = {
            let mut trees = self.trees.lock().unwrap_or_else(|e| e.into_inner());
            evictable(&trees, keep, n)
                .into_iter()
                .filter_map(|key| {
                    trees.remove(&key);
                    self.move_aside(&key.0, &key.1)
                })
                .collect()
        };
        for dir in &trash {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                tracing::warn!("Pin store: could not delete {}: {e}", dir.display());
            }
        }
        if configs_dropped > 0 || !trash.is_empty() {
            tracing::debug!(
                "Pin store: evicted {configs_dropped} config(s), {} checkout(s)",
                trash.len()
            );
        }
    }

    /// Move the published checkout of `(ws, commit)` to a `.tmp-evict-*`
    /// name. `None` when there is none, or when it cannot be moved: it then
    /// stays published, complete, and a later `ensure_tree` reuses it.
    fn move_aside(&self, ws: &str, commit: &str) -> Option<PathBuf> {
        let trees = self.trees_dir(ws);
        let dir = trees.join(commit);
        let trash = trees.join(format!("{TMP_PREFIX}evict-{}", uuid::Uuid::new_v4()));
        match std::fs::rename(&dir, &trash) {
            Ok(()) => Some(trash),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(
                    "Pin store: could not move {} aside for deletion: {e}",
                    dir.display()
                );
                None
            }
        }
    }

    /// Loaded configs per workspace, sorted by name (the
    /// `stroem_pins_cached` gauge). A workspace with none is absent.
    pub fn cached_counts(&self) -> Vec<(String, usize)> {
        let configs = self.configs.lock().unwrap_or_else(|e| e.into_inner());
        let mut counts: HashMap<String, usize> = HashMap::new();
        for ((ws, _), slot) in configs.iter() {
            if slot.cell.initialized() {
                *counts.entry(ws.clone()).or_default() += 1;
            }
        }
        let mut out: Vec<(String, usize)> = counts.into_iter().collect();
        out.sort();
        out
    }

    /// Commits of `ws` whose config is loaded, sorted.
    pub fn cached_commits(&self, ws: &str) -> Vec<String> {
        let configs = self.configs.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<String> = configs
            .iter()
            .filter(|((w, _), slot)| w == ws && slot.cell.initialized())
            .map(|((_, c), _)| c.clone())
            .collect();
        out.sort();
        out
    }

    /// Config loads started so far (test support).
    #[doc(hidden)]
    pub fn load_count(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }

    /// Test hook: hold every blocking pin load of this store at its start
    /// (fetch + checkout, config load) until the returned guard drops. Not
    /// `#[cfg(test)]`: integration test binaries link this crate without it.
    #[doc(hidden)]
    pub fn hold_loads_for_test(&self) -> PinLoadHold {
        self.gate.entered.store(0, Ordering::SeqCst);
        self.gate.set_closed(true);
        PinLoadHold(Arc::clone(&self.gate))
    }
}

impl PinRef {
    /// A pinned job's pin: `job.git_ref` + `job.revision` (spec § 6).
    pub fn of_job(job: &stroem_db::JobRow) -> Option<PinRef> {
        Some(PinRef {
            git_ref: job.git_ref.clone()?,
            commit: job.revision.clone()?,
        })
    }

    /// A step whose action was resolved through `ref:`. A live
    /// cross-workspace step also stamps `action_revision` but has no
    /// `action_ref`, so it is not pinned.
    pub fn of_step_action(step: &stroem_db::JobStepRow) -> Option<PinRef> {
        Some(PinRef {
            git_ref: step.action_ref.clone()?,
            commit: step.action_revision.clone()?,
        })
    }

    /// A `type: task` step's task owner and pin (`task_*`, spec § 6).
    pub fn of_step_task(step: &stroem_db::JobStepRow) -> Option<(String, PinRef)> {
        Some((
            step.task_workspace.clone()?,
            PinRef {
                git_ref: step.task_ref.clone()?,
                commit: step.task_revision.clone()?,
            },
        ))
    }
}

impl PinStoreConfig {
    pub fn from_section(section: Option<&crate::config::PinStoreSection>) -> PinStoreConfig {
        let default = PinStoreConfig::default();
        let Some(s) = section else {
            return default;
        };
        PinStoreConfig {
            dir: s.dir.as_deref().map(PathBuf::from).unwrap_or(default.dir),
            keep_recent_per_workspace: s
                .keep_recent_per_workspace
                .unwrap_or(default.keep_recent_per_workspace),
        }
    }
}

// ---- blocking git helpers (always run on `spawn_blocking`) ----

/// Remove every entry of every `{root}/{ws}/trees/`. Called at `open`,
/// under the dir lock, when no pin of this process is in use yet: published
/// checkouts are rebuilt on demand, interrupted (`.tmp-*`) ones are garbage.
/// The bare repos are kept.
fn remove_leftover_checkouts(root: &Path) {
    let Ok(workspaces) = std::fs::read_dir(root) else {
        return;
    };
    for ws in workspaces.flatten() {
        let Ok(entries) = std::fs::read_dir(ws.path().join("trees")) else {
            continue;
        };
        let (mut removed, mut interrupted) = (0usize, 0usize);
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            let result = if is_dir {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => {
                    removed += 1;
                    if entry.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
                        interrupted += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!("Pin store: could not remove {}: {e}", path.display())
                }
            }
        }
        if removed > 0 {
            tracing::info!(
                "Pin store: removed {removed} leftover checkout(s) of '{}' \
                 ({interrupted} interrupted)",
                ws.file_name().to_string_lossy()
            );
        }
    }
}

/// Open the bare repo, or (re)create it. An unusable repo is removed and
/// re-created — the rule `GitSource` applies to its clone dir
/// (`workspace/git.rs:69-88`).
fn open_or_init_bare(dir: &Path, url: &str) -> anyhow::Result<git2::Repository> {
    if dir.exists() {
        match git2::Repository::open_bare(dir) {
            Ok(repo) => {
                match repo.find_remote("origin") {
                    Ok(remote) if remote.url().ok() == Some(url) => {}
                    Ok(_) => repo
                        .remote_set_url("origin", url)
                        .context("update remote 'origin'")?,
                    Err(_) => {
                        repo.remote("origin", url).context("add remote 'origin'")?;
                    }
                }
                return Ok(repo);
            }
            Err(e) => {
                tracing::warn!(
                    "Pin repository {} is not usable ({e}); re-creating",
                    dir.display()
                );
                std::fs::remove_dir_all(dir)
                    .with_context(|| format!("remove unusable pin repository {}", dir.display()))?;
            }
        }
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create pin repository {}", dir.display()))?;
    let repo = git2::Repository::init_bare(dir).context("init bare pin repository")?;
    repo.remote("origin", url).context("add remote 'origin'")?;
    Ok(repo)
}

fn fetch(
    repo: &git2::Repository,
    src: &PinSource,
    refspecs: &[&str],
    budget: &LoadBudget,
) -> anyhow::Result<()> {
    budget.check()?;
    let mut remote = repo.find_remote("origin").context("no remote 'origin'")?;
    let mut options = git2::FetchOptions::new();
    options.remote_callbacks(GitSource::build_remote_callbacks(&src.auth, budget));
    remote
        .fetch(refspecs, Some(&mut options), None)
        .map_err(|e| git_error(e, budget, "fetch from origin"))
        .with_context(|| format!("fetch {}", refspecs.join(" ")))
}

/// `git ls-remote`: every advertised ref → sha. A peeled entry
/// (`refs/tags/x^{}`) overrides the tag object's own sha.
fn ls_remote(src: &PinSource, budget: &LoadBudget) -> anyhow::Result<HashMap<String, String>> {
    budget.check()?;
    let mut remote =
        git2::Remote::create_detached(src.url.as_str()).context("create detached remote")?;
    let callbacks = GitSource::build_remote_callbacks(&src.auth, budget);
    let connection = remote
        .connect_auth(git2::Direction::Fetch, Some(callbacks), None)
        .map_err(|e| git_error(e, budget, "connect to origin"))?;
    let mut refs = HashMap::new();
    for head in connection
        .list()
        .map_err(|e| git_error(e, budget, "list remote refs"))?
    {
        // Only heads and tags are resolvable (spec § 4.2); a host's other
        // refs (`refs/pull/*`, `HEAD`, …) would only bloat the cache.
        if !(head.name().starts_with("refs/heads/") || head.name().starts_with("refs/tags/")) {
            continue;
        }
        let oid = head.oid().to_string();
        match head.name().strip_suffix("^{}") {
            Some(base) => {
                refs.insert(base.to_string(), oid);
            }
            None => {
                refs.entry(head.name().to_string()).or_insert(oid);
            }
        }
    }
    Ok(refs)
}

/// The commit `oid` peels to, if that object is in `repo`.
fn local_commit(repo: &git2::Repository, oid: &str) -> Option<String> {
    let oid = git2::Oid::from_str(oid).ok()?;
    let object = repo.find_object(oid, None).ok()?;
    object.peel_to_commit().ok().map(|c| c.id().to_string())
}

/// Make `commit` present in the bare repo and return the commit it peels to
/// (a 40-hex SHA may name an annotated tag object; a pin always records the
/// commit).
fn ensure_commit_local(
    ws: &str,
    repo_dir: &Path,
    src: &PinSource,
    commit: &str,
    by_sha: bool,
    budget: &LoadBudget,
) -> Result<String, PinError> {
    let repo = open_or_init_bare(repo_dir, &src.url).map_err(|e| unavailable(ws, e))?;
    if let Some(peeled) = local_commit(&repo, commit) {
        return Ok(peeled);
    }
    if by_sha {
        if let Err(e) = fetch(&repo, src, &[commit], budget) {
            tracing::debug!(
                "Pin store: fetching {commit} by SHA failed for '{ws}' ({e:#}); \
                 falling back to all heads and tags"
            );
        }
        if let Some(peeled) = local_commit(&repo, commit) {
            return Ok(peeled);
        }
    }
    fetch(
        &repo,
        src,
        &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"],
        budget,
    )
    .map_err(|e| unavailable(ws, e))?;
    local_commit(&repo, commit).ok_or_else(|| PinError::CommitNotFound {
        workspace: ws.to_string(),
        commit: commit.to_string(),
    })
}

/// Commit for an advertised ref. If the advertised object is already here,
/// use it. Otherwise fetch the ref BY NAME and adopt whatever tip arrives:
/// the ref may have moved or been force-pushed since the listing, and the
/// advertised object may no longer be fetchable at all (spec § 5.2). The
/// commit returned is therefore always present locally.
fn commit_for_ref(
    ws: &str,
    git_ref: &str,
    repo_dir: &Path,
    src: &PinSource,
    name: &str,
    advertised: &str,
    budget: &LoadBudget,
) -> Result<RefCommit, PinError> {
    let not_found = || PinError::RefNotFound {
        workspace: ws.to_string(),
        git_ref: git_ref.to_string(),
    };
    let repo = open_or_init_bare(repo_dir, &src.url).map_err(|e| unavailable(ws, e))?;
    if let Some(commit) = local_commit(&repo, advertised) {
        return Ok(RefCommit::Advertised(commit));
    }
    // A fetch of a ref the remote no longer has succeeds and transfers
    // nothing, so an older local value of `name` must not survive it and
    // pass for the fetched tip. Nothing else reads local refs.
    if let Ok(mut stale) = repo.find_reference(name) {
        stale.delete().map_err(|e| unavailable(ws, e))?;
    }
    let refspec = format!("+{name}:{name}");
    fetch(&repo, src, &[refspec.as_str()], budget).map_err(|e| unavailable(ws, e))?;
    let tip = repo.refname_to_id(name).map_err(|_| not_found())?;
    local_commit(&repo, &tip.to_string())
        .map(RefCommit::Adopted)
        .ok_or_else(not_found)
}

/// Check `commit`'s tree out to `{trees_dir}/{name}` via a tmp dir +
/// rename, so a published dir is always complete. No `.git`, no index
/// update. Reuses a dir published earlier. `name` is the store key, the
/// dir `evict` deletes; `commit` is the commit it peels to.
fn checkout_commit(
    ws: &str,
    repo_dir: &Path,
    trees_dir: &Path,
    name: &str,
    commit: &str,
    budget: &LoadBudget,
) -> Result<PathBuf, PinError> {
    let final_dir = trees_dir.join(name);
    if final_dir.is_dir() {
        return Ok(final_dir);
    }
    std::fs::create_dir_all(trees_dir).map_err(|e| unavailable(ws, e))?;
    let tmp = trees_dir.join(format!("{TMP_PREFIX}{name}-{}", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let repo = git2::Repository::open_bare(repo_dir).context("open pin repository")?;
        let oid = git2::Oid::from_str(commit).context("parse commit")?;
        let tree = repo
            .find_commit(oid)
            .and_then(|c| c.tree())
            .context("read the commit's tree")?;
        std::fs::create_dir_all(&tmp).context("create checkout dir")?;
        let mut checkout = checkout_builder(budget);
        checkout.force().update_index(false).target_dir(&tmp);
        repo.checkout_tree(tree.as_object(), Some(&mut checkout))
            .map_err(|e| git_error(e, budget, "checkout pinned tree"))?;
        std::fs::rename(&tmp, &final_dir).context("publish checkout")
    })();
    match result {
        Ok(()) => Ok(final_dir),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            // A cancelled earlier caller's detached checkout of the same
            // commit may have published first; its content is identical.
            if final_dir.is_dir() {
                Ok(final_dir)
            } else {
                Err(unavailable(ws, e))
            }
        }
    }
}

/// Map a folder-loader result to the pin taxonomy (spec § 5.3) by the
/// loader's own markers, never by message text:
/// - `DeadlineExceeded`, or a failure of the `vals` filter, is transient;
/// - any other load error is permanent;
/// - a SOPS file the loader could not decrypt is a WARNING, and the load
///   succeeds without it. Caching that config would freeze a possibly
///   transient decryption failure into an immutable pin, so it is
///   transient too. Any other warning (a YAML parse error) keeps live-load
///   semantics: the config loads without that file.
fn classify_load(
    ws: &str,
    commit: &str,
    loaded: anyhow::Result<(WorkspaceConfig, Vec<String>)>,
) -> Result<WorkspaceConfig, PinError> {
    match loaded {
        Ok((config, warnings)) => {
            if let Some(w) = warnings.iter().find(|w| is_sops_failure_warning(w)) {
                return Err(unavailable(ws, w));
            }
            if !warnings.is_empty() {
                tracing::warn!(
                    "Pinned workspace '{ws}' at {commit}: {} file(s) skipped due to errors",
                    warnings.len()
                );
            }
            Ok(config)
        }
        Err(e) if is_deadline_exceeded(&e) || is_vals_failure(&e) => Err(unavailable(ws, e)),
        Err(e) => Err(PinError::PinLoadFailed {
            workspace: ws.to_string(),
            commit: commit.to_string(),
            message: format!("{e:#}"),
        }),
    }
}

/// `secrets` + same-workspace typed `secret: true` connection properties,
/// from a `WorkspaceSet` holding only this config.
fn local_secret_values(ws: &str, config: &WorkspaceConfig) -> Vec<String> {
    let set = crate::workspace_set::WorkspaceSet::from_parts(
        ws,
        Some(config),
        Vec::new(),
        vec![ws.to_string()],
    );
    crate::workspace_set::collect_redaction_values(&set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::git_test_support::*;
    use tempfile::TempDir;

    #[test]
    fn pin_refs_come_from_the_job_and_step_columns() {
        let mut job = stroem_db::JobRow::test_default();
        assert_eq!(PinRef::of_job(&job), None, "unpinned job");
        job.git_ref = Some("release/2.3".into());
        job.revision = Some("a".repeat(40));
        assert_eq!(
            PinRef::of_job(&job),
            Some(PinRef {
                git_ref: "release/2.3".into(),
                commit: "a".repeat(40)
            })
        );

        let mut step = stroem_db::JobStepRow::test_default(job.job_id, "s");
        assert_eq!(PinRef::of_step_action(&step), None);
        assert_eq!(PinRef::of_step_task(&step), None);
        step.action_workspace = Some("w".into());
        step.action_revision = Some("b".repeat(40));
        assert_eq!(
            PinRef::of_step_action(&step),
            None,
            "cross-workspace live step is not pinned"
        );
        step.action_ref = Some("v4.1.0".into());
        assert_eq!(
            PinRef::of_step_action(&step),
            Some(PinRef {
                git_ref: "v4.1.0".into(),
                commit: "b".repeat(40)
            })
        );
        step.task_workspace = Some("billing".into());
        step.task_ref = Some("v4".into());
        step.task_revision = Some("c".repeat(40));
        assert_eq!(
            PinRef::of_step_task(&step),
            Some((
                "billing".into(),
                PinRef {
                    git_ref: "v4".into(),
                    commit: "c".repeat(40)
                }
            ))
        );
    }

    #[test]
    fn pin_store_config_from_section_applies_defaults() {
        let d = PinStoreConfig::from_section(None);
        assert_eq!(d.dir, PinStoreConfig::default_dir());
        assert_eq!(
            d.keep_recent_per_workspace,
            DEFAULT_KEEP_RECENT_PER_WORKSPACE
        );
        let s = crate::config::PinStoreSection {
            dir: Some("/var/lib/stroem/pins".into()),
            keep_recent_per_workspace: Some(2),
            claim_load_budget_secs: None,
        };
        let c = PinStoreConfig::from_section(Some(&s));
        assert_eq!(c.dir, PathBuf::from("/var/lib/stroem/pins"));
        assert_eq!(c.keep_recent_per_workspace, 2);
    }

    const HOUR: Duration = Duration::from_secs(3600);

    fn store(url: &str, poll: Duration) -> (TempDir, PinStore) {
        open_store(url, poll, 0, HashMap::new(), ReloadSettings::default())
    }

    fn open_store(
        url: &str,
        poll: Duration,
        keep_recent: usize,
        libraries: HashMap<String, ResolvedLibrary>,
        settings: ReloadSettings,
    ) -> (TempDir, PinStore) {
        let dir = TempDir::new().unwrap();
        let store = PinStore::open(
            PinStoreConfig {
                dir: dir.path().join("pins"),
                keep_recent_per_workspace: keep_recent,
            },
            HashMap::from([(
                "w".to_string(),
                PinSource {
                    url: url.to_string(),
                    auth: None,
                    poll_interval: poll,
                },
            )]),
            Arc::new(libraries),
            settings,
        )
        .unwrap();
        (dir, store)
    }

    /// Every reference of workspace `w`'s local bare repo, as
    /// (full name, commit it peels to).
    fn local_refs(d: &TempDir) -> Vec<(String, String)> {
        let repo = git2::Repository::open_bare(d.path().join("pins/w/repo.git")).unwrap();
        let mut refs: Vec<(String, String)> = repo
            .references()
            .unwrap()
            .flatten()
            .filter_map(|r| {
                let name = r.name().ok()?.to_string();
                let commit = r.peel_to_commit().ok()?.id().to_string();
                Some((name, commit))
            })
            .collect();
        refs.sort();
        refs
    }

    #[test]
    fn open_takes_an_exclusive_dir_lock() {
        let dir = TempDir::new().unwrap();
        let cfg = || PinStoreConfig {
            dir: dir.path().join("pins"),
            keep_recent_per_workspace: 0,
        };
        let first = PinStore::open(
            cfg(),
            HashMap::new(),
            Arc::new(HashMap::new()),
            ReloadSettings::default(),
        )
        .expect("first store opens");
        let err = PinStore::open(
            cfg(),
            HashMap::new(),
            Arc::new(HashMap::new()),
            ReloadSettings::default(),
        )
        .expect_err("second store on the same dir must fail");
        assert!(
            format!("{err:#}").contains("in use by another process"),
            "{err:#}"
        );
        drop(first);
        PinStore::open(
            cfg(),
            HashMap::new(),
            Arc::new(HashMap::new()),
            ReloadSettings::default(),
        )
        .expect("lock is released with the store");
    }

    /// No pin is in use at startup, so every checkout left by the previous
    /// process goes — interrupted (`.tmp-*`) and published alike. The bare
    /// repo stays: it is only ever fetched into.
    #[test]
    fn open_removes_every_leftover_checkout_and_keeps_the_bare_repo() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("pins").join("w");
        let trees = ws.join("trees");
        std::fs::create_dir_all(trees.join(".tmp-abc-123")).unwrap();
        std::fs::create_dir_all(trees.join(".tmp-evict-456")).unwrap();
        std::fs::create_dir_all(trees.join("0123456789abcdef0123456789abcdef01234567")).unwrap();
        std::fs::write(trees.join("stray-file"), "x").unwrap();
        std::fs::create_dir_all(ws.join("repo.git")).unwrap();
        let _store = PinStore::open(
            PinStoreConfig {
                dir: dir.path().join("pins"),
                keep_recent_per_workspace: 0,
            },
            HashMap::new(),
            Arc::new(HashMap::new()),
            ReloadSettings::default(),
        )
        .unwrap();
        let left: Vec<_> = std::fs::read_dir(&trees)
            .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "leftover checkouts survived: {left:?}");
        assert!(ws.join("repo.git").exists(), "the bare repo is kept");
    }

    #[tokio::test]
    async fn resolve_branch_returns_its_tip() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        let pin = store.resolve("w", "main").await.unwrap();
        assert_eq!(
            pin,
            Pin {
                workspace: "w".into(),
                git_ref: "main".into(),
                commit: c1
            }
        );
    }

    #[tokio::test]
    async fn resolve_peels_lightweight_and_annotated_tags_to_the_commit() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        lightweight_tag(remote.path(), "v1.0.0", &c1);
        let tag_object = annotated_tag(remote.path(), "v1.0.1", &c1);
        assert_ne!(tag_object, c1, "fixture must create a real tag object");
        let (_d, store) = store(&url, HOUR);
        assert_eq!(store.resolve("w", "v1.0.0").await.unwrap().commit, c1);
        assert_eq!(store.resolve("w", "v1.0.1").await.unwrap().commit, c1);
        assert_eq!(
            store.resolve("w", "refs/tags/v1.0.1").await.unwrap().commit,
            c1
        );
    }

    #[tokio::test]
    async fn resolve_bare_name_prefers_the_branch_over_a_same_named_tag() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let c2 = commit_on(remote.path(), "both", &[("wf.yaml", &workflow("v2"))]);
        lightweight_tag(remote.path(), "both", &c1);
        let (_d, store) = store(&url, HOUR);
        assert_eq!(store.resolve("w", "both").await.unwrap().commit, c2);
        assert_eq!(
            store.resolve("w", "refs/tags/both").await.unwrap().commit,
            c1
        );
    }

    #[tokio::test]
    async fn resolve_full_sha_fetches_a_commit_no_fetched_ref_points_at() {
        let (remote, url, _c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let c2 = commit_on(remote.path(), "release", &[("wf.yaml", &workflow("v2"))]);
        let (d, store) = store(&url, HOUR);
        let pin = store.resolve("w", &c2).await.unwrap();
        assert_eq!(pin.commit, c2);
        assert_eq!(pin.git_ref, c2);
        // Fetched by SHA, not by the heads/tags fallback: that fallback
        // would have created `refs/heads/release` → c2 locally.
        //
        // Over `file://` this proves the code path (the by-SHA fetch ran,
        // returned Ok and created no ref), not the wire negotiation: libgit2's
        // local transport sends every object reachable from any advertised
        // ref whatever is wanted, and answers an unknown-SHA want with Ok.
        // Whether a smart server grants want-by-SHA is not testable here;
        // the fallback covers a refusal.
        let refs = local_refs(&d);
        assert!(
            refs.iter().all(|(_, commit)| *commit != c2),
            "no local ref may point at the SHA-fetched commit: {refs:?}"
        );
    }

    #[tokio::test]
    async fn resolve_sha_falls_back_to_all_heads_and_tags_when_want_by_sha_is_refused() {
        let (remote, url, _c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let c2 = commit_on(remote.path(), "release", &[("wf.yaml", &workflow("v2"))]);
        let (d, store) = store(&url, HOUR);
        store.skip_fetch_by_sha.store(true, Ordering::SeqCst);
        assert_eq!(store.resolve("w", &c2).await.unwrap().commit, c2);
        assert!(
            local_refs(&d).contains(&("refs/heads/release".to_string(), c2.clone())),
            "the fallback fetches every head: {:?}",
            local_refs(&d)
        );
    }

    #[tokio::test]
    async fn resolve_unknown_sha_is_commit_not_found() {
        let (_r, url, _c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            store.resolve("w", sha).await.unwrap_err(),
            PinError::CommitNotFound {
                workspace: "w".into(),
                commit: sha.into()
            }
        );
    }

    #[tokio::test]
    async fn resolve_caches_the_listing_until_the_poll_interval() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        assert_eq!(store.resolve("w", "main").await.unwrap().commit, c1);
        let _c2 = commit_on(remote.path(), "main", &[("wf.yaml", &workflow("v2"))]);
        assert_eq!(
            store.resolve("w", "main").await.unwrap().commit,
            c1,
            "within the poll interval the cached listing is used"
        );
    }

    #[tokio::test]
    async fn resolve_picks_up_a_branch_move_after_the_poll_interval() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, Duration::ZERO);
        assert_eq!(store.resolve("w", "main").await.unwrap().commit, c1);
        let c2 = commit_on(remote.path(), "main", &[("wf.yaml", &workflow("v2"))]);
        assert_eq!(store.resolve("w", "main").await.unwrap().commit, c2);
    }

    #[tokio::test]
    async fn resolve_uses_the_last_listing_when_the_remote_is_unreachable() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, Duration::ZERO);
        assert_eq!(store.resolve("w", "main").await.unwrap().commit, c1);
        std::fs::remove_dir_all(remote.path()).unwrap();
        assert_eq!(store.resolve("w", "main").await.unwrap().commit, c1);
    }

    /// Spec § 5.2: `RefNotFound` is permanent and only an ls-remote that
    /// SUCCEEDED can prove absence. A ref missing from the last listing while
    /// the remote is unreachable (a branch pushed since) is transient.
    #[tokio::test]
    async fn resolve_ref_missing_from_a_stale_listing_while_unreachable_is_unavailable() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, Duration::ZERO);
        assert_eq!(store.resolve("w", "main").await.unwrap().commit, c1);
        commit_on(remote.path(), "late", &[("wf.yaml", &workflow("v2"))]);
        std::fs::remove_dir_all(remote.path()).unwrap();
        let err = store.resolve("w", "late").await.unwrap_err();
        assert!(matches!(err, PinError::PinUnavailable { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn resolve_unreachable_remote_without_a_listing_is_unavailable() {
        let gone = TempDir::new().unwrap();
        let url = format!("file://{}", gone.path().join("missing").display());
        let (_d, store) = store(&url, HOUR);
        let err = store.resolve("w", "main").await.unwrap_err();
        assert!(err.is_transient(), "{err}");
        assert!(matches!(err, PinError::PinUnavailable { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn resolve_deleted_branch_is_ref_not_found() {
        let (remote, url, _c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        commit_on(remote.path(), "feat", &[("wf.yaml", &workflow("v2"))]);
        let (_d, store) = store(&url, Duration::ZERO);
        store.resolve("w", "feat").await.unwrap();
        delete_branch(remote.path(), "feat");
        assert_eq!(
            store.resolve("w", "feat").await.unwrap_err(),
            PinError::RefNotFound {
                workspace: "w".into(),
                git_ref: "feat".into()
            }
        );
    }

    #[tokio::test]
    async fn resolve_adopts_the_fetched_tip_when_the_advertised_commit_is_not_local() {
        let (remote, url, _c0) = bare_remote(&[("wf.yaml", &workflow("v0"))]);
        let c1 = commit_on(remote.path(), "release", &[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        // Lists every ref (release → c1) and fetches nothing. Resolving
        // `main` here would not do: libgit2's `file://` transport sends every
        // object reachable from ANY advertised ref, whatever the refspec, so
        // c1 would already be local.
        assert!(
            matches!(
                store.resolve("w", "missing").await,
                Err(PinError::RefNotFound { .. })
            ),
            "fixture: list without fetching"
        );
        // The branch moves; the cached listing still advertises c1, which is
        // not in the local repo.
        let c2 = commit_on(remote.path(), "release", &[("wf.yaml", &workflow("v2"))]);
        let pin = store.resolve("w", "release").await.unwrap();
        assert_eq!(
            pin.commit, c2,
            "fetched tip adopted, not the stale advertised {c1}"
        );
        // Still inside the TTL: the adopted tip replaced the listing entry.
        // c1 is local now too (an ancestor of c2), so a stale entry would
        // resolve to it.
        assert_eq!(
            store.resolve("w", "release").await.unwrap().commit,
            c2,
            "the listing entry must not go back to the stale advertised {c1}"
        );
    }

    /// A by-name fetch of a ref the remote no longer has succeeds and
    /// transfers nothing. A local ref left by an earlier fetch must not then
    /// pass for "the fetched tip".
    #[tokio::test]
    async fn resolve_never_adopts_a_stale_local_ref_of_a_deleted_branch() {
        let (remote, url, _c0) = bare_remote(&[("wf.yaml", &workflow("v0"))]);
        commit_on(remote.path(), "release", &[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        // An earlier fetch left refs/heads/release → c1 in the pin repo.
        let repo = open_or_init_bare(&store.repo_dir("w"), &url).unwrap();
        fetch(
            &repo,
            &store.source("w").unwrap(),
            &["+refs/heads/release:refs/heads/release"],
            &LoadBudget::unbounded(),
        )
        .unwrap();
        // The branch moves to a commit that is not local, the listing
        // records it, and then the branch is deleted.
        commit_on(remote.path(), "release", &[("wf.yaml", &workflow("v2"))]);
        assert!(matches!(
            store.resolve("w", "missing").await,
            Err(PinError::RefNotFound { .. })
        ));
        delete_branch(remote.path(), "release");
        assert_eq!(
            store.resolve("w", "release").await.unwrap_err(),
            PinError::RefNotFound {
                workspace: "w".into(),
                git_ref: "release".into()
            }
        );
    }

    /// A cached-listing miss asks the remote once before answering: a branch
    /// pushed after the last ls-remote is not a 400 for a poll interval.
    #[tokio::test]
    async fn resolve_branch_pushed_after_the_cached_listing_is_found_within_the_ttl() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        assert_eq!(store.resolve("w", "main").await.unwrap().commit, c1);
        let late = commit_on(remote.path(), "late", &[("wf.yaml", &workflow("v2"))]);
        assert_eq!(store.resolve("w", "late").await.unwrap().commit, late);
    }

    #[tokio::test]
    async fn resolve_missing_ref_within_the_ttl_is_ref_not_found_after_one_refresh() {
        let (_r, url, _c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        store.resolve("w", "main").await.unwrap();
        let listed_at = || store.listings.lock().unwrap()["w"].fetched_at;
        let before = listed_at();
        assert_eq!(
            store.resolve("w", "nope").await.unwrap_err(),
            PinError::RefNotFound {
                workspace: "w".into(),
                git_ref: "nope".into()
            }
        );
        assert!(listed_at() > before, "the miss refreshed the listing");
    }

    #[tokio::test]
    async fn resolve_missing_ref_within_the_ttl_is_unavailable_when_the_refresh_fails() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        assert_eq!(store.resolve("w", "main").await.unwrap().commit, c1);
        commit_on(remote.path(), "late", &[("wf.yaml", &workflow("v2"))]);
        std::fs::remove_dir_all(remote.path()).unwrap();
        let err = store.resolve("w", "late").await.unwrap_err();
        assert!(matches!(err, PinError::PinUnavailable { .. }), "{err:?}");
        assert_eq!(
            store.resolve("w", "main").await.unwrap().commit,
            c1,
            "a cached hit needs no remote"
        );
    }

    /// Two resolves racing over one stale listing entry: the first fetches
    /// and adopts c2; without serialisation the second still reads c1, finds
    /// it local (c2's parent) and hands out the OLDER commit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_resolves_after_a_branch_move_return_the_same_commit() {
        let (remote, url, _c0) = bare_remote(&[("wf.yaml", &workflow("v0"))]);
        commit_on(remote.path(), "release", &[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        // Lists release → c1 and fetches nothing.
        assert!(matches!(
            store.resolve("w", "missing").await,
            Err(PinError::RefNotFound { .. })
        ));
        let c2 = commit_on(remote.path(), "release", &[("wf.yaml", &workflow("v2"))]);
        let (a, b) = tokio::join!(store.resolve("w", "release"), store.resolve("w", "release"));
        assert_eq!(a.unwrap().commit, c2);
        assert_eq!(b.unwrap().commit, c2);
    }

    #[tokio::test]
    async fn resolve_full_sha_of_an_annotated_tag_object_pins_its_commit() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let tag_object = annotated_tag(remote.path(), "v1", &c1);
        let (_d, store) = store(&url, HOUR);
        let pin = store.resolve("w", &tag_object).await.unwrap();
        assert_eq!(pin.commit, c1, "Pin.commit is always a commit");
        assert_eq!(pin.git_ref, tag_object, "the ref stays as written");
    }

    /// Only heads and tags are ever resolved (§ 4.2). A host's other refs
    /// (GitHub's `refs/pull/*`, `HEAD`) are not kept in the cached listing.
    #[tokio::test]
    async fn the_cached_listing_holds_only_heads_and_tags() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        git2::Repository::open_bare(remote.path())
            .unwrap()
            .reference(
                "refs/pull/1/head",
                git2::Oid::from_str(&c1).unwrap(),
                true,
                "pr",
            )
            .unwrap();
        annotated_tag(remote.path(), "v1", &c1);
        let (_d, store) = store(&url, HOUR);
        store.resolve("w", "main").await.unwrap();
        let listings = store.listings.lock().unwrap();
        let mut names: Vec<&str> = listings["w"].refs.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["refs/heads/main", "refs/tags/v1"]);
    }

    /// `resolve` is awaited from axum handlers, which need `Send` futures;
    /// `#[tokio::test]`'s current-thread runtime would not notice.
    #[test]
    fn resolve_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let store = PinStore::disabled();
        let fut = store.resolve("w", "main");
        assert_send(&fut);
    }

    #[tokio::test]
    async fn resolve_on_a_non_git_workspace_is_not_git() {
        let (_r, url, _c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        assert_eq!(
            store.resolve("folder_ws", "main").await.unwrap_err(),
            PinError::NotGit {
                workspace: "folder_ws".into()
            }
        );
        assert!(!store.is_git("folder_ws"));
        assert!(store.is_git("w"));
        assert_eq!(
            PinStore::disabled().resolve("w", "main").await.unwrap_err(),
            PinError::NotGit {
                workspace: "w".into()
            }
        );
    }

    #[test]
    fn ref_not_found_message_hints_at_short_shas() {
        let msg = PinError::RefNotFound {
            workspace: "w".into(),
            git_ref: "3f2a9c0".into(),
        }
        .to_string();
        assert!(
            msg.contains("ref '3f2a9c0' not found in workspace 'w'"),
            "{msg}"
        );
        assert!(msg.contains("40-character"), "{msg}");
        let plain = PinError::RefNotFound {
            workspace: "w".into(),
            git_ref: "release/2.3".into(),
        }
        .to_string();
        assert!(!plain.contains("40-character"), "{plain}");
    }

    #[test]
    fn only_pin_unavailable_is_transient() {
        let w = || "w".to_string();
        assert!(PinError::PinUnavailable {
            workspace: w(),
            message: "x".into()
        }
        .is_transient());
        assert!(!PinError::NotGit { workspace: w() }.is_transient());
        assert!(!PinError::RefNotFound {
            workspace: w(),
            git_ref: "r".into()
        }
        .is_transient());
        assert!(!PinError::CommitNotFound {
            workspace: w(),
            commit: "c".into()
        }
        .is_transient());
        assert!(!PinError::PinLoadFailed {
            workspace: w(),
            commit: "c".into(),
            message: "m".into()
        }
        .is_transient());
    }

    #[test]
    fn cannot_be_loaded_appends_the_error_or_is_the_withheld_sentence() {
        let pin = PinRef {
            git_ref: "release/2.3".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
        };
        let gone = anyhow::Error::new(PinError::CommitNotFound {
            workspace: "etl".into(),
            commit: pin.commit.clone(),
        });
        assert_eq!(
            cannot_be_loaded("etl", &pin, &gone),
            format!(
                "[pin] etl@release/2.3 (0123456) cannot be loaded: commit {} not found in \
                 workspace 'etl'",
                pin.commit
            )
        );
        let withheld = anyhow::Error::new(PinLoadWithheld::new("etl", &pin));
        assert_eq!(
            cannot_be_loaded("etl", &pin, &withheld),
            "[pin] etl@release/2.3 (0123456) cannot be loaded: its configuration does not load"
        );
    }

    fn store_with(
        url: &str,
        keep_recent: usize,
        libraries: HashMap<String, ResolvedLibrary>,
    ) -> (TempDir, PinStore) {
        open_store(url, HOUR, keep_recent, libraries, ReloadSettings::default())
    }

    fn store_with_settings(url: &str, settings: ReloadSettings) -> (TempDir, PinStore) {
        open_store(url, HOUR, 0, HashMap::new(), settings)
    }

    fn script_of(p: &Pinned) -> String {
        p.config.actions["greet"].script.clone().unwrap_or_default()
    }

    /// Entries of `w`'s trees dir that are in-progress checkouts or
    /// renamed-for-deletion dirs.
    fn tmp_entries(d: &TempDir) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(d.path().join("pins/w/trees"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name())
                    .filter(|n| n.to_string_lossy().starts_with(TMP_PREFIX))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn ensure_tree_checks_out_an_immutable_dir_without_dot_git() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (d, store) = store(&url, HOUR);
        let tree = store.ensure_tree("w", &c1).await.unwrap();
        assert_eq!(tree.dir, d.path().join("pins/w/trees").join(&c1));
        assert!(tree.dir.join("wf.yaml").is_file());
        assert!(!tree.dir.join(".git").exists());
        let again = store.ensure_tree("w", &c1).await.unwrap();
        assert!(Arc::ptr_eq(&tree, &again));
        assert!(
            tmp_entries(&d).is_empty(),
            "no tmp dir may survive a checkout"
        );
    }

    /// The checkout dir is named by the requested key (eviction derives the
    /// path from it); its content is the commit the SHA peels to.
    #[tokio::test]
    async fn ensure_tree_of_an_annotated_tag_object_checks_out_its_commit() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let tag_object = annotated_tag(remote.path(), "v1", &c1);
        let (d, store) = store(&url, HOUR);
        let tree = store.ensure_tree("w", &tag_object).await.unwrap();
        assert_eq!(tree.dir, d.path().join("pins/w/trees").join(&tag_object));
        assert!(tree.dir.join("wf.yaml").is_file());
    }

    #[tokio::test]
    async fn ensure_loads_the_config_of_the_pinned_commit_not_the_tip() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let _c2 = commit_on(remote.path(), "main", &[("wf.yaml", &workflow("v2"))]);
        let (_d, store) = store(&url, HOUR);
        let pinned = store.ensure("w", &c1).await.unwrap();
        assert_eq!(script_of(&pinned), "echo v1");
    }

    #[tokio::test]
    async fn ensure_merges_server_libraries() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let lib_config: WorkspaceConfig = serde_yaml::from_str(
            "actions:\n  common.ping:\n    type: script\n    script: echo ping\n",
        )
        .unwrap();
        let libs = HashMap::from([(
            "common".to_string(),
            ResolvedLibrary {
                config: lib_config,
                path: PathBuf::from("/lib"),
            },
        )]);
        let (_d, store) = store_with(&url, 0, libs);
        let pinned = store.ensure("w", &c1).await.unwrap();
        assert!(pinned.config.actions.contains_key("common.ping"));
        assert!(pinned.config.actions.contains_key("greet"));
    }

    #[tokio::test]
    async fn ensure_collects_the_commits_secret_values() {
        let yaml = format!(
            "{}secrets:\n  db_pass: pinned-secret-value\n",
            workflow("v1")
        );
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &yaml)]);
        let (_d, store) = store(&url, HOUR);
        let pinned = store.ensure("w", &c1).await.unwrap();
        assert!(pinned
            .secret_values
            .contains(&"pinned-secret-value".to_string()));
        let debug = format!("{pinned:?}");
        assert!(
            !debug.contains("pinned-secret-value"),
            "Debug must not print secrets: {debug}"
        );
    }

    #[tokio::test]
    async fn ensure_unrenderable_connection_is_a_permanent_load_failure() {
        let yaml = format!(
            "{}connections:\n  db:\n    host: \"{{{{ secret.nope }}}}\"\n",
            workflow("v1")
        );
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &yaml)]);
        let (_d, store) = store(&url, HOUR);
        // The fixture must make the loader's load ITSELF fail — not a
        // per-file warning, which would load (F33).
        let tree = store.ensure_tree("w", &c1).await.unwrap();
        assert!(
            super::super::folder::load_folder_workspace_with(&tree.dir, &LoadBudget::unbounded())
                .is_err(),
            "the folder loader itself must reject this commit"
        );
        let err = store.ensure("w", &c1).await.unwrap_err();
        assert!(matches!(err, PinError::PinLoadFailed { .. }), "{err:?}");
        assert!(!err.is_transient());
    }

    /// Transience comes from the loader's typed markers, never from words in
    /// the message: this error names `vals` and `sops` and is neither.
    #[tokio::test]
    async fn ensure_error_text_naming_vals_and_sops_is_still_permanent() {
        let yaml = format!(
            "{}connections:\n  db:\n    host: \"{{{{ secret.vals_and_sops }}}}\"\n",
            workflow("v1")
        );
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &yaml)]);
        let (_d, store) = store(&url, HOUR);
        let err = store.ensure("w", &c1).await.unwrap_err();
        assert!(err.to_string().contains("vals_and_sops"), "{err}");
        assert!(matches!(err, PinError::PinLoadFailed { .. }), "{err:?}");
    }

    /// A per-file YAML parse error keeps live-load semantics: the file is
    /// skipped with a warning, the rest loads, and the pin is cached.
    #[tokio::test]
    async fn ensure_yaml_parse_warning_loads_the_rest_and_is_cached() {
        let (_r, url, c1) = bare_remote(&[
            ("wf.yaml", &workflow("v1")),
            ("broken.yaml", "actions: [unclosed\n"),
        ]);
        let (_d, store) = store(&url, HOUR);
        let pinned = store.ensure("w", &c1).await.unwrap();
        assert_eq!(script_of(&pinned), "echo v1");
        let again = store.ensure("w", &c1).await.unwrap();
        assert!(Arc::ptr_eq(&pinned, &again));
        assert_eq!(store.load_count(), 1);
    }

    #[tokio::test]
    async fn ensure_failed_sops_file_is_transient_and_not_cached() {
        let (_r, url, c1) = bare_remote(&[
            ("wf.yaml", &workflow("v1")),
            (
                "creds.sops.yaml",
                "secrets:\n  k: ENC[AES256_GCM,data:abc]\nsops:\n  version: 3\n",
            ),
        ]);
        let (_d, store) = store(&url, HOUR);
        let err = store.ensure("w", &c1).await.unwrap_err();
        assert!(err.is_transient(), "{err:?}");
        let _ = store.ensure("w", &c1).await;
        assert_eq!(
            store.load_count(),
            2,
            "a transient failure is retried, not cached"
        );
    }

    /// Fails whether or not `vals` is installed: the CLI is missing, or the
    /// file it is asked to read is.
    #[tokio::test]
    async fn ensure_failed_vals_reference_is_transient_and_not_cached() {
        let yaml = format!(
            "{}secrets:\n  token: \"{{{{ 'ref+file:///definitely/missing/stroem-pin-test' | vals }}}}\"\n",
            workflow("v1")
        );
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &yaml)]);
        let (_d, store) = store(&url, HOUR);
        let err = store.ensure("w", &c1).await.unwrap_err();
        assert!(err.is_transient(), "{err:?}");
        let _ = store.ensure("w", &c1).await;
        assert_eq!(store.load_count(), 2);
    }

    #[tokio::test]
    async fn ensure_past_its_deadline_is_transient() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store_with_settings(
            &url,
            ReloadSettings {
                load_timeout: Duration::ZERO,
                ..ReloadSettings::default()
            },
        );
        let err = store.ensure("w", &c1).await.unwrap_err();
        assert!(err.is_transient(), "{err:?}");
    }

    /// One deadline covers a whole `ensure`: the config phase runs under
    /// the budget the call started with, never a fresh one of its own.
    #[tokio::test]
    async fn the_config_load_runs_under_the_callers_deadline() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        store.ensure_tree("w", &c1).await.unwrap();
        let err = store
            .load_pinned("w", &c1, LoadBudget::until(Instant::now()))
            .await
            .unwrap_err();
        assert!(err.is_transient(), "{err:?}");
    }

    #[tokio::test]
    async fn a_pin_load_waits_for_a_permit_at_most_until_its_deadline() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store_with_settings(
            &url,
            ReloadSettings {
                load_timeout: Duration::from_millis(200),
                ..ReloadSettings::default()
            },
        );
        let _all = store
            .permits
            .acquire_many(MAX_CONCURRENT_PIN_LOADS as u32)
            .await
            .unwrap();
        let started = Instant::now();
        let err = store.ensure("w", &c1).await.unwrap_err();
        assert!(
            matches!(&err, PinError::PinUnavailable { message, .. } if message.contains("saturated")),
            "{err:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn ensure_is_single_flight_per_commit() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        let (a, b) = tokio::join!(store.ensure("w", &c1), store.ensure("w", &c1));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(store.load_count(), 1);
    }

    #[tokio::test]
    async fn ensure_unknown_commit_is_commit_not_found() {
        let (_r, url, _c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert!(matches!(
            store.ensure("w", sha).await.unwrap_err(),
            PinError::CommitNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn evict_honours_the_keep_set_held_handles_and_recent_pins() {
        let (remote, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let c2 = commit_on(remote.path(), "main", &[("wf.yaml", &workflow("v2"))]);
        let c3 = commit_on(remote.path(), "main", &[("wf.yaml", &workflow("v3"))]);
        let (d, store) = store_with(&url, 1, HashMap::new());
        drop(store.ensure("w", &c1).await.unwrap());
        let held = store.ensure("w", &c2).await.unwrap();
        drop(store.ensure("w", &c3).await.unwrap()); // most recently used

        let keep = HashSet::from([("w".to_string(), c1.clone())]);
        store.evict(&keep);
        let mut expected = vec![c1.clone(), c2.clone(), c3.clone()];
        expected.sort();
        assert_eq!(
            store.cached_commits("w"),
            expected,
            "keep-set, held handle, recent"
        );

        drop(held);
        store.evict(&HashSet::new());
        assert_eq!(
            store.cached_commits("w"),
            vec![c3.clone()],
            "only the recent pin survives"
        );
        assert!(
            !d.path().join("pins/w/trees").join(&c2).exists(),
            "evicted checkout deleted"
        );
        assert!(d.path().join("pins/w/trees").join(&c3).exists());
        assert!(
            tmp_entries(&d).is_empty(),
            "renamed-for-deletion dirs are gone"
        );
    }

    #[tokio::test]
    async fn an_evicted_pin_is_checked_out_again_on_demand() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store_with(&url, 0, HashMap::new());
        drop(store.ensure("w", &c1).await.unwrap());
        store.evict(&HashSet::new());
        assert!(store.cached_commits("w").is_empty());
        let pinned = store.ensure("w", &c1).await.unwrap();
        assert!(pinned.dir.join("wf.yaml").is_file());
        assert_eq!(store.load_count(), 2);
    }

    /// Leases (spec § 5.3): a checkout a caller holds is never deleted; once
    /// released it is evicted, and a later request checks it out afresh.
    #[tokio::test]
    async fn evict_never_deletes_a_checkout_a_caller_holds() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (d, store) = store(&url, HOUR);
        let tree = store.ensure_tree("w", &c1).await.unwrap();
        store.evict(&HashSet::new());
        assert!(
            tree.dir.join("wf.yaml").is_file(),
            "a held checkout survives eviction"
        );
        let dir = tree.dir.clone();
        drop(tree);
        store.evict(&HashSet::new());
        assert!(!dir.exists(), "a released checkout is evicted");
        let again = store.ensure_tree("w", &c1).await.unwrap();
        assert_eq!(again.dir, dir);
        assert!(again.dir.join("wf.yaml").is_file(), "checked out afresh");
        assert!(tmp_entries(&d).is_empty());
    }

    /// F22: once the cell is initialised, a single-flight waiter may hold the
    /// cell without a clone of the value yet. That is a lease too.
    #[test]
    fn a_slot_is_in_use_while_anyone_holds_its_cell_or_its_value() {
        let slot: Slot<u8> = Slot::new();
        assert!(!slot.in_use());
        let waiter = Arc::clone(&slot.cell);
        assert!(slot.in_use(), "an initialisation in flight");
        slot.cell.set(Arc::new(1)).unwrap();
        assert!(
            slot.in_use(),
            "initialised, and a waiter still holds the cell"
        );
        drop(waiter);
        assert!(!slot.in_use());
        let value = Arc::clone(slot.cell.get().unwrap());
        assert!(slot.in_use(), "a caller holds the value");
        drop(value);
        assert!(!slot.in_use());
    }

    /// F34: a failed load is never "recently used". Here it is the most
    /// recent entry, and with `keep_recent = 1` it must not displace c1.
    #[tokio::test]
    async fn failed_loads_do_not_count_toward_keep_recent() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (d, store) = store_with(&url, 1, HashMap::new());
        drop(store.ensure("w", &c1).await.unwrap());
        let missing = "0123456789abcdef0123456789abcdef01234567";
        assert!(store.ensure("w", missing).await.is_err());
        store.evict(&HashSet::new());
        assert_eq!(store.cached_commits("w"), vec![c1.clone()]);
        assert!(d.path().join("pins/w/trees").join(&c1).exists());
        assert_eq!(
            store.configs.lock().unwrap().len(),
            1,
            "failed slot dropped"
        );
        assert_eq!(store.trees.lock().unwrap().len(), 1, "failed slot dropped");
    }

    #[tokio::test]
    async fn cached_counts_reports_loaded_configs_per_workspace() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        assert!(store.cached_counts().is_empty());
        let _p = store.ensure("w", &c1).await.unwrap();
        assert_eq!(store.cached_counts(), vec![("w".to_string(), 1)]);
    }

    /// Awaited from axum handlers and the claim path, which need `Send`.
    #[test]
    fn ensure_futures_are_send() {
        fn assert_send<T: Send>(_: &T) {}
        let store = PinStore::disabled();
        assert_send(&store.ensure("w", "x"));
        assert_send(&store.ensure_tree("w", "x"));
    }

    fn close_gate(store: &PinStore) -> PinLoadHold {
        store.hold_loads_for_test()
    }

    /// Poll `fut` until its blocking work is parked at the gate, then drop
    /// it: a client disconnect or a caller-side timeout mid-load.
    async fn abandon_when_gated<F: std::future::Future>(store: &PinStore, fut: F) {
        let mut fut = Box::pin(fut);
        let gated = async {
            while store.gate.entered.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = &mut fut => panic!("the load finished while gated"),
                () = gated => {}
            }
        })
        .await
        .expect("the blocking work reaches the gate");
    }

    async fn all_permits_return(store: &PinStore) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while store.permits.available_permits() < MAX_CONCURRENT_PIN_LOADS {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the permit returns once the blocking work finishes");
    }

    /// The deadline starts when a caller asks, not when the cell is handed
    /// to it: a waiter queued behind a load that fails after its deadline
    /// fails fast instead of starting a fresh full-budget load.
    #[tokio::test]
    async fn a_waiter_whose_deadline_passed_in_the_queue_fails_fast() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store_with_settings(
            &url,
            ReloadSettings {
                load_timeout: Duration::from_millis(300),
                ..ReloadSettings::default()
            },
        );
        let gate = close_gate(&store);
        let release = async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            drop(gate);
        };
        let (first, waiter, ()) =
            tokio::join!(store.ensure("w", &c1), store.ensure("w", &c1), release);
        assert!(
            matches!(first, Err(PinError::PinUnavailable { .. })),
            "{first:?}"
        );
        assert!(
            matches!(waiter, Err(PinError::PinUnavailable { .. })),
            "the waiter's deadline passed while it was queued: {waiter:?}"
        );
        assert_eq!(store.load_count(), 0, "no load starts past the deadline");
    }

    /// `MAX_CONCURRENT_PIN_LOADS` bounds real work: a dropped caller's
    /// fetch + checkout keeps its permit until the blocking work returns.
    #[tokio::test]
    async fn a_dropped_caller_keeps_the_permit_until_its_checkout_returns() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        let gate = close_gate(&store);
        abandon_when_gated(&store, store.ensure_tree("w", &c1)).await;
        assert_eq!(
            store.permits.available_permits(),
            MAX_CONCURRENT_PIN_LOADS - 1,
            "the detached checkout still holds its permit"
        );
        drop(gate);
        all_permits_return(&store).await;
    }

    #[tokio::test]
    async fn a_dropped_caller_keeps_the_permit_until_its_config_load_returns() {
        let (_r, url, c1) = bare_remote(&[("wf.yaml", &workflow("v1"))]);
        let (_d, store) = store(&url, HOUR);
        // Warm checkout: only the config load reaches the gate.
        let _tree = store.ensure_tree("w", &c1).await.unwrap();
        let gate = close_gate(&store);
        abandon_when_gated(&store, store.ensure("w", &c1)).await;
        assert_eq!(
            store.permits.available_permits(),
            MAX_CONCURRENT_PIN_LOADS - 1,
            "the detached config load still holds its permit"
        );
        drop(gate);
        all_permits_return(&store).await;
    }
}
