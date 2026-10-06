//! Shared fixture for the git-refs integration tests
//! (`docs/superpowers/specs/2026-10-02-git-refs-design.md`).
//!
//! An isolated database (via `stroem_test_support`), a full `AppState` +
//! router, and three workspaces:
//! `etl` (git: `main` + `release/2.3`), `billing` (git: `main` + tag `v4.1.0`
//! on an older commit) and `docs` (in-memory, not git). Git workspaces are local
//! bare repos served over `file://`. Their LIVE entries are real `GitSource`s
//! tracking `main` (watchers not started), each with its own clone dir, so
//! `WorkspaceManager::reload` sees a new `main` commit and the live tarball path
//! has a real clone with `.git`. The PinStore uses `poll_interval = 0`, so
//! every resolve re-lists the remote — a branch move is visible at once, with
//! no expiry hook.
#![allow(dead_code)] // each test file uses a different subset

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use stroem_common::budget::LoadBudget;
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_db::{ApiKeyRepo, JobRepo, UserGroupRepo, UserRepo};
use stroem_server::auth::{generate_api_key, hash_password};
use stroem_server::blob_storage::{BlobArchive, LocalBlobArchive};
use stroem_server::config::{
    AclConfig, AuthConfig, DbConfig, JobDefaults, LogStorageConfig, McpConfig, PinStoreSection,
    RetentionConfig, ServerConfig,
};
use stroem_server::job_creator::{create_job_for_task_pinned, CreationMode};
use stroem_server::log_read::StepFilter;
use stroem_server::log_storage::{JobLogMeta, LogStorage};
use stroem_server::state::AppState;
use stroem_server::state_storage::StateStorage;
use stroem_server::web::build_router;
use stroem_server::workspace::availability::ReloadSettings;
use stroem_server::workspace::folder::load_folder_workspace_with;
use stroem_server::workspace::git::GitSource;
use stroem_server::workspace::pins::{PinSource, PinStore, PinStoreConfig};
use stroem_server::workspace::{
    in_memory_entry, WorkspaceEntry, WorkspaceManager, WorkspaceSource,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

pub const FIXTURE_WORKER_TOKEN: &str = "fixture-worker-token";
const FIXTURE_PASSWORD: &str = "fixture-password";
const FIXTURE_JWT_SECRET: &str = "fixture-jwt-secret-key";
const FIXTURE_REFRESH_SECRET: &str = "fixture-refresh-secret-key";
/// Every fixture repo keeps its whole workspace in one root-level file (the
/// workspace loader scans a folder's own YAML when it has no `.workflows/`).
const WORKFLOW_FILE: &str = "workflow.yaml";

// ─── Default YAML ──────────────────────────────────────────────────────────

/// `etl` main: the manifest. References release/2.3 in several ways.
pub const ETL_MAIN: &str = r#"
actions:
  hello:
    type: script
    runner: local
    script: echo main
  call-nightly:
    type: task
    task: nightly
    ref: release/2.3
tasks:
  manifest:
    flow:
      first:
        action: hello
      later:
        action: hello
        ref: release/2.3
        depends_on: [first]
      call:
        action: call-nightly
        depends_on: [first]
  uses-release-import:
    flow:
      run:
        action: import
        ref: release/2.3
  uses-billing-tag:
    flow:
      run:
        action: billing.export
        ref: v4.1.0
  uses-release-agent:
    flow:
      run:
        action: ask
        ref: release/2.3
  uses-missing-action:
    flow:
      run:
        action: nope
        ref: release/2.3
  uses-missing-branch:
    flow:
      run:
        action: import
        ref: release/9.9
  uses-docs-ref:
    flow:
      run:
        action: docs.render
        ref: main
  uses-release-db:
    flow:
      run:
        action: query
        ref: release/2.3
        input:
          db: release-db
  uses-release-bad-db:
    flow:
      run:
        action: query
        ref: release/2.3
        input:
          db: nope-db
  fan:
    flow:
      each:
        action: hello
        ref: release/2.3
        for_each: [1, 2]
"#;

/// `etl` release/2.3 at its first commit. `# nightly-end` is a splice point
/// tests use to add a step in a later commit. Connection-type properties sit
/// directly under the type name: `ConnectionTypeDef` is `#[serde(transparent)]`
/// (a `properties:` wrapper would make the loader skip the whole file).
pub const ETL_RELEASE: &str = r#"
connection_types:
  pg:
    host:
      type: string
connections:
  release-db:
    type: pg
    host: release-host
actions:
  hello:
    type: script
    runner: local
    script: echo v1
  import:
    type: script
    runner: local
    script: echo import-v1
  notify:
    type: script
    runner: local
    script: echo notify
  query:
    type: script
    runner: local
    script: echo q
    input:
      db:
        type: pg
  call-local:
    type: task
    task: nightly
  call-ref:
    type: task
    task: nightly
    ref: release/2.3
  gate:
    type: approval
    message: proceed?
  ask:
    type: agent
    provider: anthropic
    model: claude-sonnet-5
    prompt: hi
    tools:
      - task: sub
tasks:
  nightly:
    folder: nightlies
    flow:
      a:
        action: hello
      b:
        action: hello
        depends_on: [a]
      # nightly-end
  wrapper:
    flow:
      call:
        action: call-local
  wrapper-foreign:
    flow:
      call:
        action: billing.run
  failing:
    on_error:
      - action: notify
    flow:
      only:
        action: hello
  flaky:
    retry:
      max_attempts: 2
      delay: 1s
    flow:
      only:
        action: hello
  ref-hook:
    on_error:
      - action: notify
        ref: release/2.3
    flow:
      only:
        action: hello
  task-ref-hook:
    on_error:
      - action: call-ref
    flow:
      only:
        action: hello
  approval-root:
    on_suspended:
      - action: notify
    flow:
      wait:
        action: gate
  agent-wrap:
    flow:
      think:
        action: ask
  sub:
    flow:
      s:
        action: hello
"#;

pub const BILLING_MAIN: &str = r#"
actions:
  export:
    type: script
    runner: local
    script: echo export-main
  run:
    type: task
    task: nightly
tasks:
  nightly:
    flow:
      x:
        action: export
"#;

/// The commit tagged `v4.1.0` (billing's main has moved on since).
pub const BILLING_TAGGED: &str = r#"
actions:
  export:
    type: script
    runner: local
    script: echo export-v4
tasks:
  nightly:
    flow:
      x:
        action: export
"#;

/// A folder-like workspace: configured, never pinnable.
pub const DOCS: &str = r#"
actions:
  render:
    type: script
    runner: local
    script: echo render
tasks: {}
"#;

// ─── Options ───────────────────────────────────────────────────────────────

pub struct FixtureUser {
    pub email: &'static str,
    pub groups: Vec<&'static str>,
    pub admin: bool,
}

#[derive(Default)]
pub struct PinnedFixtureOpts {
    /// `None` = ACL off.
    pub acl: Option<AclConfig>,
    /// Auth is enabled iff non-empty. Every user's password is the fixture's.
    pub users: Vec<FixtureUser>,
    pub mcp: bool,
    /// A local state archive under the fixture's TempDir.
    pub state_storage: bool,
    /// Default 5.
    pub keep_recent_per_workspace: Option<usize>,
    pub etl_main: Option<String>,
    pub etl_release: Option<String>,
    pub billing_main: Option<String>,
    pub billing_tagged: Option<String>,
    /// `pin_store.claim_load_budget_secs` (every replica). Default: the
    /// server's (20 s).
    pub claim_load_budget_secs: Option<u64>,
}

// ─── Repos ─────────────────────────────────────────────────────────────────

pub struct FixtureRepo {
    pub path: PathBuf,
    pub url: String,
}

impl FixtureRepo {
    fn init(path: PathBuf) -> Result<Self> {
        git2::Repository::init_bare(&path).context("init bare fixture repo")?;
        let url = format!("file://{}", path.display());
        Ok(FixtureRepo { path, url })
    }

    fn open(&self) -> git2::Repository {
        git2::Repository::open_bare(&self.path)
            .expect("fixture repo is open (did a test leave it broken?)")
    }

    /// Commit `files` (path, content) on `branch`, created from `from`'s tip if
    /// missing (a root commit when neither exists). Paths may contain `/`.
    /// Returns the new commit SHA.
    pub fn commit(&self, branch: &str, from: &str, files: &[(&str, &str)]) -> String {
        let repo = self.open();
        let branch_ref = format!("refs/heads/{branch}");
        let parent = repo
            .find_reference(&branch_ref)
            .or_else(|_| repo.find_reference(&format!("refs/heads/{from}")))
            .ok()
            .map(|r| r.peel_to_commit().unwrap());
        let mut tree_oid = parent.as_ref().map(|c| c.tree_id());
        for (path, content) in files {
            let blob = repo.blob(content.as_bytes()).unwrap();
            let base = tree_oid.map(|oid| repo.find_tree(oid).unwrap());
            let parts: Vec<&str> = path.split('/').collect();
            tree_oid = Some(upsert_path(&repo, base.as_ref(), &parts, blob));
        }
        let tree = repo
            .find_tree(tree_oid.expect("at least one file"))
            .unwrap();
        let sig = git2::Signature::now("fixture", "fixture@test").unwrap();
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let oid = repo
            .commit(
                Some(&branch_ref),
                &sig,
                &sig,
                "fixture commit",
                &tree,
                &parents,
            )
            .unwrap();
        if repo.head().is_err() {
            repo.set_head(&branch_ref).unwrap();
        }
        oid.to_string()
    }

    /// Lightweight tag.
    pub fn tag(&self, name: &str, commit: &str) {
        self.open()
            .reference(
                &format!("refs/tags/{name}"),
                git2::Oid::from_str(commit).unwrap(),
                true,
                "fixture tag",
            )
            .unwrap();
    }

    pub fn annotated_tag(&self, name: &str, commit: &str) {
        let repo = self.open();
        let target = repo
            .find_object(git2::Oid::from_str(commit).unwrap(), None)
            .unwrap();
        let sig = git2::Signature::now("fixture", "fixture@test").unwrap();
        repo.tag(name, &target, &sig, "fixture annotated tag", true)
            .unwrap();
    }

    pub fn delete_branch(&self, name: &str) {
        self.open()
            .find_reference(&format!("refs/heads/{name}"))
            .unwrap()
            .delete()
            .unwrap();
    }

    /// Point `name` at `commit` regardless of history (force-push semantics).
    pub fn force_branch(&self, name: &str, commit: &str) {
        self.open()
            .reference(
                &format!("refs/heads/{name}"),
                git2::Oid::from_str(commit).unwrap(),
                true,
                "fixture force",
            )
            .unwrap();
    }

    /// Rename the bare dir away: every transport to `url` fails until
    /// [`restore_remote`](Self::restore_remote).
    pub fn break_remote(&self) {
        std::fs::rename(&self.path, self.broken_path()).expect("break fixture remote");
    }

    pub fn restore_remote(&self) {
        std::fs::rename(self.broken_path(), &self.path).expect("restore fixture remote");
    }

    fn broken_path(&self) -> PathBuf {
        self.path.with_extension("broken")
    }

    /// F1: the workspace at `commit` must load with NO warnings. The loader
    /// skips a file it cannot parse with only a warning, which would silently
    /// empty a pinned config. Checked on a private checkout under `scratch`,
    /// so no PinStore is warmed (cold-replica tests depend on that).
    fn assert_loads_cleanly(&self, commit: &str, scratch: &Path) -> Result<()> {
        let repo = self.open();
        let tree = repo
            .find_commit(git2::Oid::from_str(commit)?)?
            .tree()
            .context("read the commit's tree")?;
        let dir = scratch.join(commit);
        std::fs::create_dir_all(&dir)?;
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force().update_index(false).target_dir(&dir);
        repo.checkout_tree(tree.as_object(), Some(&mut checkout))
            .context("check out the fixture commit")?;
        let (_, warnings) = load_folder_workspace_with(&dir, &LoadBudget::unbounded())
            .with_context(|| format!("{} at {commit} does not load", self.url))?;
        anyhow::ensure!(
            warnings.is_empty(),
            "{} at {commit} loads with warnings: {warnings:?}",
            self.url
        );
        Ok(())
    }
}

/// Write `blob` at `parts` under `base`, creating intermediate trees.
fn upsert_path(
    repo: &git2::Repository,
    base: Option<&git2::Tree>,
    parts: &[&str],
    blob: git2::Oid,
) -> git2::Oid {
    let mut tb = repo.treebuilder(base).unwrap();
    if parts.len() == 1 {
        tb.insert(parts[0], blob, 0o100644).unwrap();
    } else {
        let sub = base
            .and_then(|t| t.get_name(parts[0]))
            .and_then(|e| e.to_object(repo).ok())
            .and_then(|o| o.into_tree().ok());
        let sub_oid = upsert_path(repo, sub.as_ref(), &parts[1..], blob);
        tb.insert(parts[0], sub_oid, 0o040000).unwrap();
    }
    tb.write().unwrap()
}

// ─── Fixture ───────────────────────────────────────────────────────────────

pub struct Commits {
    pub etl_main: String,
    /// release/2.3 at fixture creation.
    pub etl_release: String,
    pub billing_main: String,
    /// The commit tagged `v4.1.0`.
    pub billing_tag: String,
}

pub struct PinnedFixture {
    pub state: Arc<AppState>,
    pub router: Router,
    pub pool: PgPool,
    pub etl: FixtureRepo,
    pub billing: FixtureRepo,
    pub commits: Commits,
    config: ServerConfig,
    keep_recent: usize,
    state_storage: bool,
    replicas: AtomicUsize,
    root: TempDir,
}

/// A second server replica: same pool and repos, its OWN (cold) PinStore dir,
/// its own live clones, log dir and tarball cache.
pub struct Replica {
    pub state: Arc<AppState>,
    pub router: Router,
}

pub async fn pinned_workspace_fixture(opts: PinnedFixtureOpts) -> Result<PinnedFixture> {
    let test_db = stroem_test_support::test_db().await;
    let pool = test_db.pool;
    let url = test_db.url;

    let root = TempDir::new()?;
    let etl_main_yaml = opts
        .etl_main
        .clone()
        .unwrap_or_else(|| ETL_MAIN.to_string());
    let etl_release_yaml = opts
        .etl_release
        .clone()
        .unwrap_or_else(|| ETL_RELEASE.to_string());
    let billing_main_yaml = opts
        .billing_main
        .clone()
        .unwrap_or_else(|| BILLING_MAIN.to_string());
    let billing_tagged_yaml = opts
        .billing_tagged
        .clone()
        .unwrap_or_else(|| BILLING_TAGGED.to_string());

    let etl = FixtureRepo::init(root.path().join("etl.git"))?;
    let etl_main = etl.commit("main", "main", &[(WORKFLOW_FILE, &etl_main_yaml)]);
    let etl_release = etl.commit("release/2.3", "main", &[(WORKFLOW_FILE, &etl_release_yaml)]);
    let billing = FixtureRepo::init(root.path().join("billing.git"))?;
    let billing_tag = billing.commit("main", "main", &[(WORKFLOW_FILE, &billing_tagged_yaml)]);
    billing.tag("v4.1.0", &billing_tag);
    let billing_main = billing.commit("main", "main", &[(WORKFLOW_FILE, &billing_main_yaml)]);

    let load_check = root.path().join("load-check");
    for (repo, commit) in [
        (&etl, &etl_main),
        (&etl, &etl_release),
        (&billing, &billing_tag),
        (&billing, &billing_main),
    ] {
        repo.assert_loads_cleanly(commit, &load_check)?;
    }

    let password_hash = hash_password(FIXTURE_PASSWORD)?;
    for user in &opts.users {
        let id = Uuid::new_v4();
        UserRepo::create(&pool, id, user.email, Some(&password_hash), None).await?;
        if user.admin {
            UserRepo::set_admin(&pool, id, true).await?;
        }
        for group in &user.groups {
            UserGroupRepo::add(&pool, id, group).await?;
        }
    }

    let log_dir = root.path().join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let config = ServerConfig {
        listen: "127.0.0.1:0".to_string(),
        db: DbConfig { url },
        log_storage: LogStorageConfig {
            local_dir: log_dir.to_string_lossy().to_string(),
            s3: None,
            archive: None,
            read: Default::default(),
        },
        workspaces: HashMap::new(),
        libraries: HashMap::new(),
        git_auth: HashMap::new(),
        worker_token: FIXTURE_WORKER_TOKEN.to_string(),
        auth: (!opts.users.is_empty()).then(|| AuthConfig {
            jwt_secret: FIXTURE_JWT_SECRET.to_string(),
            refresh_secret: FIXTURE_REFRESH_SECRET.to_string(),
            base_url: None,
            providers: HashMap::new(),
            initial_user: None,
            rate_limit: Default::default(),
        }),
        recovery: Default::default(),
        retention: RetentionConfig::default(),
        acl: opts.acl,
        mcp: opts.mcp.then(|| McpConfig {
            enabled: true,
            ..Default::default()
        }),
        metrics: None,
        agents: None,
        state_storage: None,
        artifact_storage: None,
        default_step_timeout: None,
        default_job_timeout: None,
        workspace_reload: Default::default(),
        // The store itself is injected below with `with_pin_store`; the
        // section only carries the claim budget.
        pin_store: opts.claim_load_budget_secs.map(|secs| PinStoreSection {
            claim_load_budget_secs: Some(secs),
            ..Default::default()
        }),
    };

    let keep_recent = opts.keep_recent_per_workspace.unwrap_or(5);
    let mgr = build_manager(root.path(), keep_recent, &etl.url, &billing.url).await?;
    let state_storage = opts.state_storage.then(|| state_storage_at(root.path()));
    let log_storage = LogStorage::new(&config.log_storage.local_dir);
    let state = AppState::new(
        pool.clone(),
        mgr,
        config.clone(),
        log_storage,
        HashMap::new(),
        state_storage,
    );
    let router = build_router(state.clone(), CancellationToken::new());

    Ok(PinnedFixture {
        state: Arc::new(state),
        router,
        pool,
        etl,
        billing,
        commits: Commits {
            etl_main,
            etl_release,
            billing_main,
            billing_tag,
        },
        config,
        keep_recent,
        state_storage: opts.state_storage,
        replicas: AtomicUsize::new(0),
        root,
    })
}

/// One replica's workspaces under `dir`: `etl`/`billing` live over real
/// `GitSource`s (clones in `dir/live/<ws>`), `docs` in memory, and a PinStore
/// in `dir/pins`.
async fn build_manager(
    dir: &Path,
    keep_recent: usize,
    etl_url: &str,
    billing_url: &str,
) -> Result<WorkspaceManager> {
    let source = |url: &str| PinSource {
        url: url.to_string(),
        auth: None,
        poll_interval: Duration::ZERO,
    };
    let store = PinStore::open(
        PinStoreConfig {
            dir: dir.join("pins"),
            keep_recent_per_workspace: keep_recent,
        },
        HashMap::from([
            ("etl".to_string(), source(etl_url)),
            ("billing".to_string(), source(billing_url)),
        ]),
        Arc::new(Default::default()),
        ReloadSettings::default(),
    )?;
    let mut entries = HashMap::new();
    for (name, url) in [("etl", etl_url), ("billing", billing_url)] {
        let entry = live_git_entry(name, url, &dir.join("live").join(name)).await?;
        entries.insert(name.to_string(), entry);
    }
    let docs: WorkspaceConfig = serde_yaml::from_str(DOCS).context("parse DOCS")?;
    entries.insert("docs".to_string(), in_memory_entry("docs", docs, None));
    Ok(WorkspaceManager::from_entries(entries).with_pin_store(store))
}

/// A healthy live entry for `name`: a `GitSource` on `url`'s `main`, cloned
/// into `clone_dir` and loaded once (with no warnings — F1).
async fn live_git_entry(name: &str, url: &str, clone_dir: &Path) -> Result<WorkspaceEntry> {
    let source = Arc::new(GitSource::with_clone_dir(
        url,
        "main",
        None,
        clone_dir.to_path_buf(),
    ));
    let loader = Arc::clone(&source);
    let outcome = tokio::task::spawn_blocking(move || loader.load(&LoadBudget::unbounded()))
        .await?
        .with_context(|| format!("load live {name}"))?;
    anyhow::ensure!(
        outcome.warnings.is_empty(),
        "live {name} loads with warnings: {:?}",
        outcome.warnings
    );
    Ok(WorkspaceEntry::new(
        name,
        source as Arc<dyn WorkspaceSource>,
        outcome.config,
        outcome.revision,
    ))
}

fn state_storage_at(root: &Path) -> StateStorage {
    let archive: Arc<dyn BlobArchive> = Arc::new(LocalBlobArchive::new(root.join("state")));
    StateStorage::new(archive, "state/".to_string(), 5, None)
}

impl PinnedFixture {
    pub fn mgr(&self) -> &WorkspaceManager {
        &self.state.workspaces
    }

    pub async fn second_replica(&self) -> Result<Replica> {
        let n = self.replicas.fetch_add(1, Ordering::SeqCst) + 1;
        let dir = self.root.path().join(format!("replica-{n}"));
        let log_dir = dir.join("logs");
        std::fs::create_dir_all(&log_dir)?;
        let mut config = self.config.clone();
        config.log_storage.local_dir = log_dir.to_string_lossy().to_string();
        let mgr = build_manager(&dir, self.keep_recent, &self.etl.url, &self.billing.url).await?;
        let state_storage = self
            .state_storage
            .then(|| state_storage_at(self.root.path()));
        let log_storage = LogStorage::new(&config.log_storage.local_dir);
        let state = AppState::new(
            self.pool.clone(),
            mgr,
            config,
            log_storage,
            HashMap::new(),
            state_storage,
        );
        let router = build_router(state.clone(), CancellationToken::new());
        Ok(Replica {
            state: Arc::new(state),
            router,
        })
    }

    /// A top-level job of `etl`'s `task`, pinned to `release/2.3` at its
    /// fixture commit (`Commits::etl_release`), created through the pinned
    /// creation path on this (primary) replica — which warms ITS PinStore only.
    pub async fn create_pinned_etl_job(&self, task: &str) -> Result<Uuid> {
        let pinned = self
            .mgr()
            .pins()
            .ensure("etl", &self.commits.etl_release)
            .await?;
        create_job_for_task_pinned(
            self.mgr(),
            &self.pool,
            &pinned.config,
            "etl",
            task,
            json!({}),
            "trigger",
            None,
            &self.commits.etl_release,
            "release/2.3",
            CreationMode::Normal,
            None,
            JobDefaults::default(),
        )
        .await
        .map(|c| c.job_id)
    }

    /// Access JWT for a seeded user (`opts.users`).
    pub async fn login(&self, email: &str) -> String {
        let (status, body) = api_req(
            &self.router,
            "POST",
            "/api/auth/login",
            None,
            Some(json!({"email": email, "password": FIXTURE_PASSWORD})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "login {email}: {body}");
        body["access_token"]
            .as_str()
            .expect("access_token")
            .to_string()
    }

    /// A raw `strm_…` API key for `email` (the user is created if missing), with
    /// the user's admin flag set to `is_admin`.
    pub async fn api_key(&self, email: &str, is_admin: bool) -> String {
        let user_id = match UserRepo::get_by_email(&self.pool, email).await.unwrap() {
            Some(u) => u.user_id,
            None => {
                let id = Uuid::new_v4();
                UserRepo::create(&self.pool, id, email, None, None)
                    .await
                    .unwrap();
                id
            }
        };
        UserRepo::set_admin(&self.pool, user_id, is_admin)
            .await
            .unwrap();
        let (raw, hash) = generate_api_key();
        ApiKeyRepo::create(&self.pool, &hash, user_id, "fixture", &raw[..12], None)
            .await
            .unwrap();
        raw
    }
}

// ─── Request helpers ───────────────────────────────────────────────────────

/// The body as JSON; a non-JSON body becomes a JSON string, an empty one `null`.
pub async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
}

async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = match body {
        Some(v) => builder
            .header("Content-Type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    (status, json_body(resp).await)
}

pub async fn api_req(
    router: &Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    send(router, method, uri, bearer, body).await
}

/// Worker API request authenticated with [`FIXTURE_WORKER_TOKEN`].
pub async fn worker_req(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    send(router, method, uri, Some(FIXTURE_WORKER_TOKEN), body).await
}

/// Capabilities each registered worker claims with (`claim_once` replays them).
fn worker_caps() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static CAPS: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    CAPS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a worker; returns its id.
pub async fn register_worker(router: &Router, capabilities: &[&str]) -> String {
    let (status, body) = worker_req(
        router,
        "POST",
        "/worker/register",
        Some(json!({"name": format!("fixture-worker-{}", Uuid::new_v4()), "capabilities": capabilities})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register worker: {body}");
    let id = body["worker_id"].as_str().expect("worker_id").to_string();
    worker_caps().lock().unwrap().insert(
        id.clone(),
        capabilities.iter().map(|c| c.to_string()).collect(),
    );
    id
}

/// One `POST /worker/jobs/claim` with the worker's registered capabilities.
/// Returns the response body whatever the status; use [`worker_req`] directly
/// when the status matters.
pub async fn claim_once(router: &Router, worker_id: &str) -> Value {
    let caps = worker_caps()
        .lock()
        .unwrap()
        .get(worker_id)
        .cloned()
        .expect("worker registered through register_worker");
    worker_req(
        router,
        "POST",
        "/worker/jobs/claim",
        Some(json!({"worker_id": worker_id, "capabilities": caps})),
    )
    .await
    .1
}

/// Bound for one fixture-based test (testcontainers + git transports).
pub const FIXTURE_TEST_TIMEOUT: Duration = Duration::from_secs(240);

/// Run a test body under [`FIXTURE_TEST_TIMEOUT`], so a hung container or git
/// transport fails that test instead of stalling the run.
pub async fn bounded(body: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    tokio::time::timeout(FIXTURE_TEST_TIMEOUT, body)
        .await
        .map_err(|_| anyhow::anyhow!("test timed out after {FIXTURE_TEST_TIMEOUT:?}"))?
}

// ─── MCP helpers ───────────────────────────────────────────────────────────

/// One MCP `tools/call` over Streamable HTTP: `initialize` first, then the
/// call on the session it opened (if any). `token` goes out as a Bearer;
/// `/mcp` takes an API key ([`PinnedFixture::api_key`]), not a login JWT.
/// Returns the JSON-RPC response body, whatever its status.
pub async fn mcp_call(router: &Router, token: Option<&str>, tool: &str, args: Value) -> Value {
    let build = |body: Value, sid: Option<&str>| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(t) = token {
            b = b.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(s) = sid {
            b = b.header("Mcp-Session-Id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };
    let init = router
        .clone()
        .oneshot(build(
            json!({"jsonrpc": "2.0", "method": "initialize", "id": 0, "params": {
                "protocolVersion": "2025-03-26", "capabilities": {},
                "clientInfo": {"name": "t", "version": "1"}}}),
            None,
        ))
        .await
        .unwrap();
    let sid = init
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let resp = router
        .clone()
        .oneshot(build(
            json!({"jsonrpc": "2.0", "method": "tools/call", "id": 1,
                   "params": {"name": tool, "arguments": args}}),
            sid.as_deref(),
        ))
        .await
        .unwrap();
    json_body(resp).await
}

/// The JSON document a successful [`mcp_call`] returned as its first text block.
pub fn mcp_tool_json(resp: &Value) -> Value {
    serde_json::from_str(
        resp["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text"),
    )
    .unwrap()
}

/// Whether an [`mcp_call`] failed: a JSON-RPC error, or a tool result flagged
/// `isError`.
pub fn mcp_is_error(resp: &Value) -> bool {
    resp.get("error").is_some() || resp["result"]["isError"].as_bool().unwrap_or(false)
}

/// Fire the scheduler trigger `etl/{trigger}` once (bounded) and return the
/// newest job it created.
pub async fn fire_etl_trigger(fx: &PinnedFixture, trigger: &str) -> Result<Uuid> {
    let key = format!("etl/{trigger}");
    tokio::time::timeout(
        Duration::from_secs(120),
        stroem_server::scheduler::fire_trigger_once(&fx.state, fx.mgr(), fx.mgr(), &key),
    )
    .await
    .map_err(|_| anyhow::anyhow!("fire_trigger_once({key}) timed out"))?;
    let id: Uuid = sqlx::query_scalar(
        "SELECT job_id FROM job WHERE source_type = 'trigger' AND source_id = $1 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&key)
    .fetch_one(&fx.pool)
    .await?;
    Ok(id)
}

/// Claim the next ready step with `worker` (it must be `job`'s `step`) and
/// complete it with `result` (a worker `complete` body: `output`,
/// `exit_code`, `error`).
pub async fn claim_and_complete(
    fx: &PinnedFixture,
    worker: &str,
    job: Uuid,
    step: &str,
    result: Value,
) {
    let claimed = claim_once(&fx.router, worker).await;
    assert_eq!(claimed["job_id"], json!(job.to_string()), "{claimed}");
    assert_eq!(claimed["step_name"], json!(step), "{claimed}");
    let (st, resp) = worker_req(
        &fx.router,
        "POST",
        &format!("/worker/jobs/{job}/steps/{step}/complete"),
        Some(result),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "complete {step}: {resp}");
}

/// `POST /api/workspaces/{ws}/tasks/{task}/execute` with `{"input": input}`.
pub async fn execute_task(
    router: &Router,
    ws: &str,
    task: &str,
    input: Value,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    api_req(
        router,
        "POST",
        &format!("/api/workspaces/{ws}/tasks/{task}/execute"),
        bearer,
        Some(json!({ "input": input })),
    )
    .await
}

/// The whole job log (JSONL) as stored on `state`'s replica.
pub async fn job_log_text(pool: &PgPool, state: &AppState, job_id: Uuid) -> String {
    let job = JobRepo::get(pool, job_id)
        .await
        .unwrap()
        .expect("job exists");
    let meta = JobLogMeta {
        workspace: job.workspace.clone(),
        task_name: job.task_name.clone(),
        created_at: job.created_at,
    };
    let terminal = matches!(job.status.as_str(), "completed" | "failed" | "cancelled");
    state
        .log_storage
        .read_tail(job_id, &meta, terminal, StepFilter::All, 16 * 1024 * 1024)
        .await
        .map(|t| t.logs)
        .unwrap_or_default()
}
