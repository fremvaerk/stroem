//! Pinned workspace snapshots keyed by `(workspace, commit)` — spec § 5.
//!
//! Fully separate from the watcher path: its own bare repository per git
//! workspace under `pin_store.dir`, its own per-workspace write lock, its
//! own load permits. `GitSource`'s working clone, the exec mutex,
//! `Availability` and `apply_load_result` are never touched from here.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use stroem_common::budget::LoadBudget;
use stroem_common::git_ref::{parse_git_ref, short_sha_hint, GitRefSpec};

use super::availability::ReloadSettings;
use super::git::{git_error, GitSource};
use super::library::ResolvedLibrary;
use crate::config::{GitAuthConfig, WorkspaceSourceDef};

/// Prefix of an in-progress checkout directory under `{ws}/trees/`.
const TMP_PREFIX: &str = ".tmp-";
/// `pin_store.keep_recent_per_workspace` default (spec § 10).
pub const DEFAULT_KEEP_RECENT_PER_WORKSPACE: usize = 5;

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
    /// Test hook: pretend the remote refuses want-by-SHA.
    #[cfg(test)]
    skip_fetch_by_sha: AtomicBool,
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
            #[cfg(test)]
            skip_fetch_by_sha: AtomicBool::new(false),
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
            #[cfg(test)]
            skip_fetch_by_sha: AtomicBool::new(false),
        }
    }

    pub fn is_git(&self, ws: &str) -> bool {
        self.sources.contains_key(ws)
    }

    pub fn has_sources(&self) -> bool {
        !self.sources.is_empty()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::git_test_support::*;
    use tempfile::TempDir;

    const HOUR: Duration = Duration::from_secs(3600);

    fn store(url: &str, poll: Duration) -> (TempDir, PinStore) {
        let dir = TempDir::new().unwrap();
        let store = PinStore::open(
            PinStoreConfig {
                dir: dir.path().join("pins"),
                keep_recent_per_workspace: 0,
            },
            HashMap::from([(
                "w".to_string(),
                PinSource {
                    url: url.to_string(),
                    auth: None,
                    poll_interval: poll,
                },
            )]),
            Arc::new(HashMap::new()),
            ReloadSettings::default(),
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
}
