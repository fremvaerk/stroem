use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use stroem_common::budget::{DeadlineExceeded, LoadBudget};

use super::source::{LoadOutcome, Peek};
use super::WorkspaceSource;
use crate::config::GitAuthConfig;

/// Git-based workspace source
pub struct GitSource {
    url: String,
    git_ref: String,
    auth: Option<GitAuthConfig>,
    clone_dir: PathBuf,
    poll_interval_secs: u64,
}

impl GitSource {
    pub fn new(
        workspace_name: &str,
        url: &str,
        git_ref: &str,
        auth: Option<GitAuthConfig>,
        poll_interval_secs: u64,
    ) -> Result<Self> {
        let clone_dir = std::env::temp_dir()
            .join("stroem")
            .join("git")
            .join(workspace_name);

        Ok(Self {
            url: url.to_string(),
            git_ref: git_ref.to_string(),
            auth,
            clone_dir,
            poll_interval_secs,
        })
    }

    #[cfg(test)]
    pub fn with_clone_dir(
        url: &str,
        git_ref: &str,
        auth: Option<GitAuthConfig>,
        clone_dir: PathBuf,
    ) -> Self {
        Self {
            url: url.to_string(),
            git_ref: git_ref.to_string(),
            auth,
            clone_dir,
            poll_interval_secs: 60,
        }
    }

    /// Clone or fetch the repository and return the HEAD OID.
    ///
    /// Reuses `clone_dir` only if it both exists AND opens as a valid git
    /// repository — an aborted clone (e.g. a deadline that fired mid-clone)
    /// can leave the directory present but empty or partial, and libgit2
    /// never re-clones into a directory it thinks is already a checkout. A
    /// directory that fails to open is removed and re-cloned instead of
    /// wedging the workspace in a permanently-broken state.
    fn clone_or_fetch(&self, budget: &LoadBudget) -> Result<String> {
        budget.check()?;

        let existing_repo = if self.clone_dir.exists() {
            match git2::Repository::open(&self.clone_dir) {
                Ok(repo) => Some(repo),
                Err(e) => {
                    tracing::warn!(
                        "Workspace clone dir {} is not a usable repository ({e}); re-cloning",
                        self.clone_dir.display()
                    );
                    std::fs::remove_dir_all(&self.clone_dir).with_context(|| {
                        format!(
                            "Failed to remove unusable clone directory {}",
                            self.clone_dir.display()
                        )
                    })?;
                    None
                }
            }
        } else {
            None
        };

        if let Some(repo) = existing_repo {
            // Fetch into the existing checkout.
            let mut remote = repo.find_remote("origin").context("No remote 'origin'")?;

            let mut fetch_options = git2::FetchOptions::new();
            let callbacks = Self::build_remote_callbacks(&self.auth, budget);
            fetch_options.remote_callbacks(callbacks);

            remote
                .fetch(&[&self.git_ref], Some(&mut fetch_options), None)
                .map_err(|e| git_error(e, budget, "Failed to fetch from origin"))?;

            // Get the fetched ref
            let fetch_head = repo
                .find_reference(&format!("refs/remotes/origin/{}", self.git_ref))
                .or_else(|_| repo.find_reference("FETCH_HEAD"))
                .context("Failed to find fetched ref")?;

            let oid = fetch_head.target().context("Ref has no target")?;

            // Reset working directory to fetched ref
            let object = repo.find_object(oid, None)?;
            let mut checkout = checkout_builder(budget);
            repo.reset(&object, git2::ResetType::Hard, Some(&mut checkout))
                .map_err(|e| git_error(e, budget, "Failed to reset to fetched ref"))?;

            Ok(oid.to_string())
        } else {
            // Clone fresh
            std::fs::create_dir_all(&self.clone_dir).context("Failed to create clone directory")?;

            let mut builder = git2::build::RepoBuilder::new();

            let mut fetch_options = git2::FetchOptions::new();
            let callbacks = Self::build_remote_callbacks(&self.auth, budget);
            fetch_options.remote_callbacks(callbacks);
            builder.fetch_options(fetch_options);
            builder.with_checkout(checkout_builder(budget));
            builder.branch(&self.git_ref);

            let repo = builder.clone(&self.url, &self.clone_dir).map_err(|e| {
                // A partial/aborted clone leaves `clone_dir` behind; the next
                // load must re-clone rather than treat it as a valid checkout.
                let _ = std::fs::remove_dir_all(&self.clone_dir);
                git_error(e, budget, "Failed to clone git repository")
            })?;

            let head = repo.head().context("Failed to get HEAD")?;
            let oid = head.target().context("HEAD has no target")?;

            Ok(oid.to_string())
        }
    }

    fn build_remote_callbacks<'a>(
        auth: &'a Option<GitAuthConfig>,
        budget: &LoadBudget,
    ) -> git2::RemoteCallbacks<'a> {
        let mut callbacks = git2::RemoteCallbacks::new();

        // Accept SSH host keys (the container has no known_hosts file)
        callbacks.certificate_check(|_cert, _host| Ok(git2::CertificateCheckStatus::CertificateOk));

        // Cooperative total deadline for object transfer (spec § 4.5). Fires
        // only between reads; a blocked read is bounded by the global libgit2
        // server timeout instead.
        let budget = *budget;
        callbacks.transfer_progress(move |_| !budget.expired());

        if let Some(auth) = auth {
            match auth.auth_type.as_str() {
                "ssh_key" => {
                    let key_path = auth.key_path.clone();
                    let key_content = auth.key.clone();
                    let attempt = Arc::new(AtomicUsize::new(0));
                    callbacks.credentials(move |url, username_from_url, allowed_types| {
                        match credential_decision(
                            attempt.load(Ordering::SeqCst),
                            allowed_types,
                            "SSH key",
                            url,
                        ) {
                            Ok(CredentialDecision::Username) => {
                                git2::Cred::username(username_from_url.unwrap_or("git"))
                            }
                            Ok(CredentialDecision::Grant) => {
                                attempt.fetch_add(1, Ordering::SeqCst);
                                let username = username_from_url.unwrap_or("git");
                                if let Some(ref content) = key_content {
                                    git2::Cred::ssh_key_from_memory(username, None, content, None)
                                } else if let Some(ref path) = key_path {
                                    git2::Cred::ssh_key(username, None, Path::new(path), None)
                                } else {
                                    git2::Cred::ssh_key_from_agent(username)
                                }
                            }
                            Err(msg) => Err(git2::Error::from_str(&msg)),
                        }
                    });
                }
                "token" => {
                    let token = auth.token.clone().unwrap_or_default();
                    let username = auth
                        .username
                        .clone()
                        .unwrap_or_else(|| "x-access-token".to_string());
                    let attempt = Arc::new(AtomicUsize::new(0));
                    callbacks.credentials(move |url, _username_from_url, allowed_types| {
                        match credential_decision(
                            attempt.load(Ordering::SeqCst),
                            allowed_types,
                            "Token",
                            url,
                        ) {
                            Ok(CredentialDecision::Username) => git2::Cred::username(&username),
                            Ok(CredentialDecision::Grant) => {
                                attempt.fetch_add(1, Ordering::SeqCst);
                                git2::Cred::userpass_plaintext(&username, &token)
                            }
                            Err(msg) => Err(git2::Error::from_str(&msg)),
                        }
                    });
                }
                _ => {}
            }
        }

        callbacks
    }
}

/// Checkout options whose `notify` callback cancels during checkout PLANNING
/// (`checkout_get_actions`) once `budget` expires. libgit2 cannot cancel the
/// write phase that follows — see spec § 4.5. `notify_on` is required:
/// notification types default to none.
fn checkout_builder(budget: &LoadBudget) -> git2::build::CheckoutBuilder<'static> {
    let budget = *budget;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.notify_on(
        git2::CheckoutNotificationType::UPDATED
            | git2::CheckoutNotificationType::CONFLICT
            | git2::CheckoutNotificationType::DIRTY,
    );
    checkout.notify(move |_, _, _, _, _| !budget.expired());
    checkout
}

/// A libgit2 error, reported as `DeadlineExceeded` whenever the budget has
/// already expired by the time libgit2 errors (usually because our own
/// callbacks aborted it) — the original libgit2 message is kept in the
/// context either way.
fn git_error(err: git2::Error, budget: &LoadBudget, msg: &'static str) -> anyhow::Error {
    if budget.expired() {
        anyhow::Error::new(DeadlineExceeded).context(format!("{msg}: {err}"))
    } else {
        anyhow::Error::new(err).context(msg)
    }
}

/// Outcome of [`credential_decision`]: either hand over a real credential, or
/// (for SSH URLs with no embedded username) first answer libgit2's
/// username-only probe.
#[derive(Debug, PartialEq, Eq)]
enum CredentialDecision {
    /// libgit2 is only asking which username to use (typical first round-trip
    /// for `ssh://` URLs without one embedded) — reply with a plain username
    /// credential. This does not count as a real credential attempt.
    Username,
    /// Hand over the real credential (SSH key or token).
    Grant,
}

/// Pure decision function for the libgit2 credentials callback: given how
/// many real credential attempts have already been made for this URL and
/// what libgit2 is asking for this round, decide whether to grant the
/// credential, answer a username-only probe, or refuse.
///
/// libgit2 re-invokes the credentials callback on every authentication
/// failure, re-running the full SSH/HTTPS handshake each time — if the
/// remote rejects our key/token, retrying the *same* credential can take
/// well over a minute before libgit2 gives up. The FIRST time libgit2 asks
/// for a real credential (`attempt == 0`) we hand it over; a second request
/// with the same `attempt` counter means the remote already rejected it, so
/// we fail immediately instead of letting libgit2 retry.
fn credential_decision(
    attempt: usize,
    allowed_types: git2::CredentialType,
    auth_kind: &str,
    url: &str,
) -> Result<CredentialDecision, String> {
    let wants_real_credential = allowed_types.contains(git2::CredentialType::SSH_KEY)
        || allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT);

    if !wants_real_credential && allowed_types.contains(git2::CredentialType::USERNAME) {
        return Ok(CredentialDecision::Username);
    }

    if attempt == 0 {
        Ok(CredentialDecision::Grant)
    } else {
        Err(format!(
            "{auth_kind} credential rejected by remote for {url} — check that the deploy key / token is authorized for this repository"
        ))
    }
}

impl WorkspaceSource for GitSource {
    fn load(&self, budget: &LoadBudget) -> Result<LoadOutcome> {
        let oid = self
            .clone_or_fetch(budget)
            .context("Git clone/fetch failed")?;
        let (config, warnings) =
            super::folder::load_folder_workspace_with(&self.clone_dir, budget)?;
        Ok(LoadOutcome {
            config,
            warnings,
            revision: Some(oid),
        })
    }

    fn path(&self) -> &Path {
        &self.clone_dir
    }

    fn peek_revision(&self, budget: &LoadBudget) -> Peek {
        if !self.clone_dir.exists() {
            return Peek::LocalInvalid(anyhow::anyhow!(
                "clone directory {} does not exist",
                self.clone_dir.display()
            ));
        }
        let repo = match git2::Repository::open(&self.clone_dir) {
            Ok(r) => r,
            Err(e) => return Peek::LocalInvalid(anyhow::Error::new(e).context("open local clone")),
        };
        let mut remote = match repo.find_remote("origin") {
            Ok(r) => r,
            Err(e) => {
                return Peek::LocalInvalid(anyhow::Error::new(e).context("find remote 'origin'"))
            }
        };
        if budget.expired() {
            return Peek::Failed(DeadlineExceeded.into());
        }
        let callbacks = Self::build_remote_callbacks(&self.auth, budget);
        let connection = match remote.connect_auth(git2::Direction::Fetch, Some(callbacks), None) {
            Ok(c) => c,
            Err(e) => return Peek::Failed(anyhow::Error::new(e).context("connect to origin")),
        };
        let refs = match connection.list() {
            Ok(r) => r,
            Err(e) => return Peek::Failed(anyhow::Error::new(e).context("list remote refs")),
        };
        let target = format!("refs/heads/{}", self.git_ref);
        match refs.iter().find(|r| r.name() == target) {
            Some(r) => Peek::Revision(r.oid().to_string()),
            None => Peek::Failed(anyhow::anyhow!("{target} is not advertised by origin")),
        }
    }

    fn poll_interval_secs(&self) -> u64 {
        self.poll_interval_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stroem_common::budget::{is_deadline_exceeded, LoadBudget};
    use tempfile::TempDir;

    /// Network test (ignored by default): a private GitHub repository cloned
    /// with an SSH key that is NOT authorized for it must fail within seconds,
    /// not minutes. Before the fail-fast credentials callback, libgit2 kept
    /// re-presenting the rejected key until the remote dropped the connection
    /// (~129s observed in production, 2026-09-07).
    ///
    /// Run with:
    ///   STROEM_TEST_SSH_KEY_PATH=/path/to/unauthorized_key \
    ///   STROEM_TEST_SSH_REPO=git@github.com:org/private-repo.git \
    ///   cargo test -p stroem-server --lib test_rejected_ssh_key_fails_fast -- --ignored --nocapture
    #[test]
    #[ignore = "needs network access and an SSH key that the remote rejects"]
    fn test_rejected_ssh_key_fails_fast() {
        let key_path = match std::env::var("STROEM_TEST_SSH_KEY_PATH") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("STROEM_TEST_SSH_KEY_PATH not set — skipping");
                return;
            }
        };
        let url = std::env::var("STROEM_TEST_SSH_REPO")
            .unwrap_or_else(|_| "git@github.com:allunite/ai-validation.git".to_string());
        let clone_dir = TempDir::new().unwrap();
        let auth = GitAuthConfig {
            auth_type: "ssh_key".to_string(),
            key_path: Some(key_path),
            key: None,
            token: None,
            username: None,
        };
        let source =
            GitSource::with_clone_dir(&url, "main", Some(auth), clone_dir.path().join("repo"));

        let started = std::time::Instant::now();
        let result = source.load(&LoadBudget::unbounded());
        let elapsed = started.elapsed();
        eprintln!(
            "load() finished in {:?}: {:?}",
            elapsed,
            result.as_ref().err().map(|e| format!("{e:#}"))
        );

        let err = result.expect_err("an unauthorized key must not clone");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rejected by remote"),
            "expected the fail-fast message, got: {msg}"
        );
        assert!(
            elapsed.as_secs() < 30,
            "rejection took {:?}; the callback should abort on the first retry",
            elapsed
        );
    }

    /// Create a bare git repo with an initial commit on `main` containing the given files.
    /// Returns (TempDir, file:// URL).
    fn create_bare_repo(files: &[(&str, &str)]) -> (TempDir, String) {
        let bare_dir = TempDir::new().unwrap();
        let bare_repo = git2::Repository::init_bare(bare_dir.path()).unwrap();

        // Build a tree from the given files
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

        // Set HEAD to main
        bare_repo
            .reference("HEAD", commit_oid, true, "set HEAD")
            .ok();
        bare_repo.set_head("refs/heads/main").unwrap();

        let url = format!("file://{}", bare_dir.path().display());
        (bare_dir, url)
    }

    /// Add a new commit on top of the given branch with new/modified files.
    fn add_commit(
        repo_path: &Path,
        branch: &str,
        files: &[(&str, &str)],
        message: &str,
    ) -> git2::Oid {
        let repo = git2::Repository::open_bare(repo_path).unwrap();
        let parent_ref = format!("refs/heads/{}", branch);
        let parent_commit = repo
            .find_reference(&parent_ref)
            .unwrap()
            .peel_to_commit()
            .unwrap();

        // Start from the parent tree and apply changes
        let parent_tree = parent_commit.tree().unwrap();
        let mut tb = repo.treebuilder(Some(&parent_tree)).unwrap();
        for &(name, content) in files {
            let oid = repo.blob(content.as_bytes()).unwrap();
            tb.insert(name, oid, 0o100644).unwrap();
        }
        let tree_oid = tb.write().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();

        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(
            Some(&parent_ref),
            &sig,
            &sig,
            message,
            &tree,
            &[&parent_commit],
        )
        .unwrap()
    }

    #[test]
    fn test_clone_local_repo_loads_config() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "deploy.yaml",
            "actions:\n  greet:\n    type: script\n    script: echo hello\ntasks:\n  hello:\n    flow:\n      step1:\n        action: greet\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let config = source.load(&LoadBudget::unbounded()).unwrap().config;
        assert_eq!(config.actions.len(), 1);
        assert!(config.actions.contains_key("greet"));
        assert_eq!(config.tasks.len(), 1);
        assert!(config.tasks.contains_key("hello"));
    }

    #[test]
    fn test_clone_sets_revision_to_git_oid() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: script\n    script: echo hi\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let out = source.load(&LoadBudget::unbounded()).unwrap();
        let rev = out.revision.unwrap();
        assert_eq!(rev.len(), 40, "Git SHA-1 should be 40 hex chars");
        assert!(
            rev.chars().all(|c| c.is_ascii_hexdigit()),
            "Revision should be hex: {}",
            rev
        );
    }

    #[test]
    fn test_fetch_detects_new_commit() {
        let (bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: script\n    script: echo v1\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let rev1 = source
            .load(&LoadBudget::unbounded())
            .unwrap()
            .revision
            .unwrap();

        // Push a new commit to the bare repo
        add_commit(
            bare_dir.path(),
            "main",
            &[(
                "test.yaml",
                "actions:\n  a:\n    type: script\n    script: echo v2\n",
            )],
            "update",
        );

        let rev2 = source
            .load(&LoadBudget::unbounded())
            .unwrap()
            .revision
            .unwrap();

        assert_ne!(rev1, rev2, "Revision should change after new commit");
    }

    #[test]
    fn test_reload_no_changes_same_revision() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: script\n    script: echo stable\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let rev1 = source
            .load(&LoadBudget::unbounded())
            .unwrap()
            .revision
            .unwrap();
        let rev2 = source
            .load(&LoadBudget::unbounded())
            .unwrap()
            .revision
            .unwrap();

        assert_eq!(rev1, rev2, "Revision should not change without new commits");
    }

    #[test]
    fn test_config_updates_after_commit() {
        let (bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  greet:\n    type: script\n    script: echo hello\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let config1 = source.load(&LoadBudget::unbounded()).unwrap().config;
        assert_eq!(config1.actions.len(), 1);

        // Add a second action
        add_commit(
            bare_dir.path(),
            "main",
            &[(
                "test.yaml",
                "actions:\n  greet:\n    type: script\n    script: echo hello\n  build:\n    type: script\n    script: make\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
            )],
            "add build action",
        );

        let config2 = source.load(&LoadBudget::unbounded()).unwrap().config;
        assert_eq!(config2.actions.len(), 2);
        assert!(config2.actions.contains_key("greet"));
        assert!(config2.actions.contains_key("build"));
    }

    #[test]
    fn test_clone_specific_branch() {
        let (bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  main_action:\n    type: script\n    script: echo main\n",
        )]);

        // Create a "develop" branch with different content
        let bare_repo = git2::Repository::open_bare(bare_dir.path()).unwrap();
        let main_commit = bare_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        bare_repo
            .reference(
                "refs/heads/develop",
                main_commit.id(),
                false,
                "create develop",
            )
            .unwrap();

        add_commit(
            bare_dir.path(),
            "develop",
            &[(
                "test.yaml",
                "actions:\n  dev_action:\n    type: script\n    script: echo develop\n",
            )],
            "develop commit",
        );

        let clone_dir = TempDir::new().unwrap();
        let source =
            GitSource::with_clone_dir(&url, "develop", None, clone_dir.path().join("repo"));

        let config = source.load(&LoadBudget::unbounded()).unwrap().config;
        assert!(
            config.actions.contains_key("dev_action"),
            "Should have develop branch action"
        );
        assert!(
            !config.actions.contains_key("main_action"),
            "Should not have main branch action"
        );
    }

    #[test]
    fn test_clone_creates_working_directory() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "deploy.yaml",
            "actions:\n  a:\n    type: script\n    script: echo hi\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let repo_dir = clone_dir.path().join("repo");
        assert!(!repo_dir.exists());

        let source = GitSource::with_clone_dir(&url, "main", None, repo_dir.clone());
        source.load(&LoadBudget::unbounded()).unwrap();

        assert!(repo_dir.exists(), "Clone dir should exist after load");
        assert!(
            repo_dir.join("deploy.yaml").exists(),
            "Cloned file should exist"
        );
    }

    #[test]
    fn test_multiple_yaml_files_merged() {
        let (_bare_dir, url) = create_bare_repo(&[
            (
                "actions.yaml",
                "actions:\n  greet:\n    type: script\n    script: echo hi\n  build:\n    type: script\n    script: make\n",
            ),
            (
                "tasks.yaml",
                "tasks:\n  deploy:\n    flow:\n      s1:\n        action: greet\n  ci:\n    flow:\n      s1:\n        action: build\n",
            ),
        ]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let config = source.load(&LoadBudget::unbounded()).unwrap().config;
        assert_eq!(config.actions.len(), 2, "Both actions should be merged");
        assert!(config.actions.contains_key("greet"));
        assert!(config.actions.contains_key("build"));
        assert_eq!(config.tasks.len(), 2, "Both tasks should be merged");
        assert!(config.tasks.contains_key("deploy"));
        assert!(config.tasks.contains_key("ci"));
    }

    #[test]
    fn test_new_construction_calculates_clone_dir_correctly() {
        let source = GitSource::new(
            "test-workspace",
            "https://github.com/example/repo.git",
            "main",
            None,
            60,
        )
        .unwrap();

        let expected_path = std::env::temp_dir()
            .join("stroem")
            .join("git")
            .join("test-workspace");

        assert_eq!(source.clone_dir, expected_path);
        assert_eq!(source.url, "https://github.com/example/repo.git");
        assert_eq!(source.git_ref, "main");
        assert!(source.auth.is_none());
    }

    #[test]
    fn test_new_with_special_characters_in_workspace_name() {
        let workspace_names = vec![
            "test-workspace",
            "test_workspace",
            "test.workspace",
            "test-workspace-123",
            "my-very-long-workspace-name-with-many-hyphens-and-numbers-12345",
        ];

        for name in workspace_names {
            let source =
                GitSource::new(name, "https://example.com/repo.git", "main", None, 60).unwrap();

            let expected_path = std::env::temp_dir().join("stroem").join("git").join(name);
            assert_eq!(source.clone_dir, expected_path);
        }
    }

    #[test]
    fn test_path_returns_clone_dir() {
        let source = GitSource::new(
            "test-workspace",
            "https://github.com/example/repo.git",
            "main",
            None,
            60,
        )
        .unwrap();

        let expected_path = std::env::temp_dir()
            .join("stroem")
            .join("git")
            .join("test-workspace");

        assert_eq!(source.path(), expected_path.as_path());
    }

    #[test]
    fn test_build_remote_callbacks_with_ssh_key_auth_valid_config() {
        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join("id_rsa");
        std::fs::write(&key_path, "fake key content").unwrap();

        let auth = GitAuthConfig {
            auth_type: "ssh_key".to_string(),
            key_path: Some(key_path.to_str().unwrap().to_string()),
            key: None,
            token: None,
            username: Some("git".to_string()),
        };

        // Should not panic
        let auth = Some(auth);
        let _callbacks = GitSource::build_remote_callbacks(&auth, &LoadBudget::unbounded());
    }

    #[test]
    fn test_build_remote_callbacks_with_token_auth() {
        let auth = GitAuthConfig {
            auth_type: "token".to_string(),
            key_path: None,
            key: None,
            token: Some("ghp_fake_token".to_string()),
            username: Some("oauth2".to_string()),
        };

        // Should not panic
        let auth = Some(auth);
        let _callbacks = GitSource::build_remote_callbacks(&auth, &LoadBudget::unbounded());
    }

    #[test]
    fn test_build_remote_callbacks_with_unknown_auth_type() {
        let auth = GitAuthConfig {
            auth_type: "unknown".to_string(),
            key_path: None,
            key: None,
            token: None,
            username: None,
        };

        // Should not panic (no-op for unknown types)
        let auth = Some(auth);
        let _callbacks = GitSource::build_remote_callbacks(&auth, &LoadBudget::unbounded());
    }

    #[test]
    fn test_build_remote_callbacks_with_ssh_key_from_memory() {
        let auth = GitAuthConfig {
            auth_type: "ssh_key".to_string(),
            key_path: None,
            key: Some(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----"
                    .to_string(),
            ),
            token: None,
            username: Some("git".to_string()),
        };

        // Should not panic — uses ssh_key_from_memory
        let auth = Some(auth);
        let _callbacks = GitSource::build_remote_callbacks(&auth, &LoadBudget::unbounded());
    }

    #[test]
    fn test_build_remote_callbacks_with_ssh_key_but_no_key_path() {
        let auth = GitAuthConfig {
            auth_type: "ssh_key".to_string(),
            key_path: None,
            key: None,
            token: None,
            username: Some("git".to_string()),
        };

        // Should not panic, falls back to agent
        let auth = Some(auth);
        let _callbacks = GitSource::build_remote_callbacks(&auth, &LoadBudget::unbounded());
    }

    #[test]
    fn test_build_remote_callbacks_with_token_but_no_token_value() {
        let auth = GitAuthConfig {
            auth_type: "token".to_string(),
            key_path: None,
            key: None,
            token: None,
            username: None,
        };

        // Should not panic, defaults to empty string for token
        let auth = Some(auth);
        let _callbacks = GitSource::build_remote_callbacks(&auth, &LoadBudget::unbounded());
    }

    #[test]
    fn test_build_remote_callbacks_with_token_defaults_username() {
        let auth = GitAuthConfig {
            auth_type: "token".to_string(),
            key_path: None,
            key: None,
            token: Some("token123".to_string()),
            username: None,
        };

        // Should not panic, defaults username to "x-access-token"
        let auth = Some(auth);
        let _callbacks = GitSource::build_remote_callbacks(&auth, &LoadBudget::unbounded());
    }

    #[test]
    fn test_clone_or_fetch_with_nonexistent_url_errors() {
        let workspace_name = format!("test-nonexistent-{}", uuid::Uuid::new_v4());

        let source = GitSource::new(
            &workspace_name,
            "https://github.com/nonexistent-user-12345/nonexistent-repo-67890.git",
            "main",
            None,
            60,
        )
        .unwrap();

        let result = source.clone_or_fetch(&LoadBudget::unbounded());
        assert!(result.is_err());

        let err_msg = result.unwrap_err().to_string();
        // Should contain git2 error about failed clone
        assert!(
            err_msg.contains("Failed to clone") || err_msg.contains("git"),
            "Expected git clone error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_new_with_auth_config() {
        let auth = GitAuthConfig {
            auth_type: "token".to_string(),
            key_path: None,
            key: None,
            token: Some("test_token".to_string()),
            username: Some("test_user".to_string()),
        };

        let source = GitSource::new(
            "test-workspace",
            "https://github.com/example/repo.git",
            "develop",
            Some(auth),
            60,
        )
        .unwrap();

        assert!(source.auth.is_some());
        let auth_config = source.auth.as_ref().unwrap();
        assert_eq!(auth_config.auth_type, "token");
        assert_eq!(auth_config.token, Some("test_token".to_string()));
        assert_eq!(auth_config.username, Some("test_user".to_string()));
    }

    #[test]
    fn test_path_is_consistent_with_new() {
        let workspace_name = "consistent-test";
        let source = GitSource::new(
            workspace_name,
            "https://github.com/example/repo.git",
            "main",
            None,
            60,
        )
        .unwrap();

        // path() should return the same as what we calculated in new()
        assert_eq!(
            source.path(),
            std::env::temp_dir()
                .join("stroem")
                .join("git")
                .join(workspace_name)
                .as_path()
        );
    }

    #[test]
    fn test_peek_revision_before_clone_returns_local_invalid() {
        let clone_dir = TempDir::new().unwrap();
        // Point to a subdirectory that doesn't exist yet
        let source = GitSource::with_clone_dir(
            "file:///nonexistent",
            "main",
            None,
            clone_dir.path().join("not-cloned"),
        );

        assert!(matches!(
            source.peek_revision(&LoadBudget::unbounded()),
            Peek::LocalInvalid(_)
        ));
    }

    #[test]
    fn test_peek_revision_matches_revision_after_load() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: script\n    script: echo hi\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let stored_revision = source
            .load(&LoadBudget::unbounded())
            .unwrap()
            .revision
            .unwrap();
        let peeked_revision = match source.peek_revision(&LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("expected Revision, got {other:?}"),
        };

        assert_eq!(
            stored_revision, peeked_revision,
            "peek_revision should match stored revision when no changes"
        );
    }

    #[test]
    fn test_peek_revision_detects_new_commit() {
        let (bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: script\n    script: echo v1\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let stored_revision = source
            .load(&LoadBudget::unbounded())
            .unwrap()
            .revision
            .unwrap();

        // Push a new commit to the bare repo
        add_commit(
            bare_dir.path(),
            "main",
            &[(
                "test.yaml",
                "actions:\n  a:\n    type: script\n    script: echo v2\n",
            )],
            "update",
        );

        let peeked_revision = match source.peek_revision(&LoadBudget::unbounded()) {
            Peek::Revision(r) => r,
            other => panic!("expected Revision, got {other:?}"),
        };
        assert_ne!(
            stored_revision, peeked_revision,
            "peek_revision should detect new remote commit"
        );
    }

    #[test]
    fn test_poll_interval_secs_default() {
        let source =
            GitSource::new("test-ws", "https://example.com/repo.git", "main", None, 60).unwrap();

        assert_eq!(source.poll_interval_secs(), 60);
    }

    #[test]
    fn test_poll_interval_secs_custom() {
        let source =
            GitSource::new("test-ws", "https://example.com/repo.git", "main", None, 300).unwrap();

        assert_eq!(source.poll_interval_secs(), 300);
    }

    // ─── credential_decision (fail-fast on rejected credentials) ──────────

    #[test]
    fn test_credential_decision_first_ssh_attempt_grants() {
        let result = credential_decision(
            0,
            git2::CredentialType::SSH_KEY,
            "SSH key",
            "git@github.com:example/repo.git",
        );
        assert!(
            matches!(result, Ok(CredentialDecision::Grant)),
            "expected Grant on first attempt, got {result:?}"
        );
    }

    #[test]
    fn test_credential_decision_username_only_request_grants_username_without_consuming_attempt() {
        // libgit2 asks for USERNAME alone (no SSH_KEY/USER_PASS_PLAINTEXT bit)
        // before it knows what username to use on ssh:// URLs with none
        // embedded. This must not be treated as a real credential attempt.
        let result = credential_decision(
            0,
            git2::CredentialType::USERNAME,
            "SSH key",
            "ssh://github.com/example/repo.git",
        );
        assert!(
            matches!(result, Ok(CredentialDecision::Username)),
            "expected Username, got {result:?}"
        );
    }

    #[test]
    fn test_credential_decision_second_ssh_attempt_is_rejected() {
        let result = credential_decision(
            1,
            git2::CredentialType::SSH_KEY,
            "SSH key",
            "git@github.com:example/repo.git",
        );
        let err = result.expect_err("second attempt must be rejected");
        assert!(
            err.contains("rejected by remote"),
            "unexpected message: {err}"
        );
        assert!(
            err.contains("git@github.com:example/repo.git"),
            "error should name the URL so operators can tell which repo is misconfigured: {err}"
        );
        assert!(
            err.contains("SSH key"),
            "error should name the auth type: {err}"
        );
    }

    #[test]
    fn test_credential_decision_first_token_attempt_grants() {
        let result = credential_decision(
            0,
            git2::CredentialType::USER_PASS_PLAINTEXT,
            "Token",
            "https://github.com/example/repo.git",
        );
        assert!(
            matches!(result, Ok(CredentialDecision::Grant)),
            "expected Grant on first attempt, got {result:?}"
        );
    }

    #[test]
    fn test_credential_decision_second_token_attempt_is_rejected() {
        let result = credential_decision(
            1,
            git2::CredentialType::USER_PASS_PLAINTEXT,
            "Token",
            "https://github.com/example/repo.git",
        );
        let err = result.expect_err("second attempt must be rejected");
        assert!(
            err.contains("rejected by remote"),
            "unexpected message: {err}"
        );
        assert!(
            err.contains("https://github.com/example/repo.git"),
            "error should name the URL: {err}"
        );
        assert!(
            err.contains("Token"),
            "error should name the auth type: {err}"
        );
    }

    #[test]
    fn test_local_bare_repo_clone_has_no_auth_callback_installed() {
        // Regression guard: local file:// clones (used throughout this test
        // module) pass `auth: None`, so `build_remote_callbacks` must not
        // install a `credentials` callback at all — confirming the fail-fast
        // logic above is scoped to configured ssh_key/token auth and never
        // interferes with unauthenticated sources.
        let (_bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: script\n    script: echo hi\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        // Would fail if a credentials callback were installed and invoked
        // unexpectedly for a no-auth local clone.
        source.load(&LoadBudget::unbounded()).unwrap();
    }

    /// git2 0.21 made ssh/https opt-in (`default = []`). Production remotes
    /// are ssh:// and https://; a build without them fails only at runtime.
    #[test]
    fn libgit2_is_built_with_ssh_and_https_transports() {
        let version = git2::Version::get();
        assert!(version.ssh(), "libgit2 built without SSH transport");
        assert!(version.https(), "libgit2 built without HTTPS transport");
    }

    // ─── Task 9: Peek classification and git deadlines ─────────────────

    const YAML_V1: &str = "actions:\n  a:\n    type: script\n    script: echo v1\n";
    const YAML_V2: &str = "actions:\n  a:\n    type: script\n    script: echo v2\n";

    fn unbounded() -> LoadBudget {
        LoadBudget::unbounded()
    }

    #[test]
    fn load_runs_outside_any_tokio_runtime() {
        // block_in_place would panic here: loading must be plain blocking code.
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, dir.path().join("repo"));
        let out = source.load(&unbounded()).unwrap();
        assert_eq!(out.revision.as_deref().map(str::len), Some(40));
        assert_eq!(out.config.actions.len(), 1);
    }

    #[test]
    fn peek_classifies_a_missing_clone_as_local_invalid() {
        let dir = TempDir::new().unwrap();
        let source =
            GitSource::with_clone_dir("file:///nowhere", "main", None, dir.path().join("repo"));
        assert!(matches!(
            source.peek_revision(&unbounded()),
            Peek::LocalInvalid(_)
        ));
    }

    #[test]
    fn peek_classifies_a_corrupt_checkout_as_local_invalid() {
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(clone.join("not-a-repo"), "x").unwrap();
        let source = GitSource::with_clone_dir("file:///nowhere", "main", None, clone);
        assert!(matches!(
            source.peek_revision(&unbounded()),
            Peek::LocalInvalid(_)
        ));
    }

    #[test]
    fn peek_classifies_a_missing_origin_as_local_invalid() {
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        let source = GitSource::with_clone_dir(&url, "main", None, clone.clone());
        source.load(&unbounded()).unwrap();
        git2::Repository::open(&clone)
            .unwrap()
            .remote_delete("origin")
            .unwrap();
        assert!(matches!(
            source.peek_revision(&unbounded()),
            Peek::LocalInvalid(_)
        ));
    }

    #[test]
    fn peek_classifies_an_unreachable_remote_as_failed() {
        let (bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, dir.path().join("repo"));
        source.load(&unbounded()).unwrap();
        drop(bare); // the remote disappears
        assert!(matches!(
            source.peek_revision(&unbounded()),
            Peek::Failed(_)
        ));
    }

    #[test]
    fn peek_classifies_a_missing_branch_as_failed() {
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        GitSource::with_clone_dir(&url, "main", None, clone.clone())
            .load(&unbounded())
            .unwrap();
        let other = GitSource::with_clone_dir(&url, "no-such-branch", None, clone);
        assert!(matches!(other.peek_revision(&unbounded()), Peek::Failed(_)));
    }

    #[test]
    fn peek_revision_matches_the_loaded_revision() {
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, dir.path().join("repo"));
        let loaded = source.load(&unbounded()).unwrap().revision.unwrap();
        match source.peek_revision(&unbounded()) {
            Peek::Revision(r) => assert_eq!(r, loaded),
            other => panic!("expected Revision, got {other:?}"),
        }
    }

    #[test]
    fn expired_budget_fails_before_touching_the_checkout() {
        let (bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        let source = GitSource::with_clone_dir(&url, "main", None, clone.clone());
        source.load(&unbounded()).unwrap();
        add_commit(bare.path(), "main", &[("test.yaml", YAML_V2)], "v2");
        let expired = LoadBudget::until(std::time::Instant::now());
        let err = source.load(&expired).unwrap_err();
        assert!(is_deadline_exceeded(&err), "{err:#}");
        let on_disk = std::fs::read_to_string(clone.join("test.yaml")).unwrap();
        assert!(on_disk.contains("echo v1"), "checkout must be untouched");
    }

    #[test]
    fn an_empty_clone_dir_is_recloned() {
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        // Simulate an aborted clone: the directory exists (libgit2 created it)
        // but has nothing in it — not a usable git repository.
        std::fs::create_dir_all(&clone).unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone);

        let out = source.load(&unbounded()).unwrap();
        assert!(out.revision.is_some());
        assert_eq!(out.config.actions.len(), 1);
    }

    #[test]
    fn a_corrupt_clone_dir_is_recloned() {
        let (_bare, url) = create_bare_repo(&[("test.yaml", YAML_V1)]);
        let dir = TempDir::new().unwrap();
        let clone = dir.path().join("repo");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(clone.join("junk"), "not a repo").unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone);

        let out = source.load(&unbounded()).unwrap();
        assert!(out.revision.is_some());
        assert_eq!(out.config.actions.len(), 1);
    }
}
