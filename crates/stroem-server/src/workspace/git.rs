use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use stroem_common::models::workflow::WorkspaceConfig;

use super::WorkspaceSource;
use crate::config::GitAuthConfig;

/// Git-based workspace source
pub struct GitSource {
    url: String,
    git_ref: String,
    auth: Option<GitAuthConfig>,
    clone_dir: PathBuf,
    revision: RwLock<Option<String>>,
}

impl GitSource {
    pub fn new(
        workspace_name: &str,
        url: &str,
        git_ref: &str,
        auth: Option<GitAuthConfig>,
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
            revision: RwLock::new(None),
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
            revision: RwLock::new(None),
        }
    }

    /// Clone or fetch the repository and return the HEAD OID
    fn clone_or_fetch(&self) -> Result<String> {
        if self.clone_dir.exists() {
            // Open existing repo and fetch
            let repo = git2::Repository::open(&self.clone_dir)
                .context("Failed to open existing git clone")?;

            let mut remote = repo.find_remote("origin").context("No remote 'origin'")?;

            let mut fetch_options = git2::FetchOptions::new();
            let callbacks = Self::build_remote_callbacks(&self.auth);
            fetch_options.remote_callbacks(callbacks);

            remote
                .fetch(&[&self.git_ref], Some(&mut fetch_options), None)
                .context("Failed to fetch from origin")?;

            // Get the fetched ref
            let fetch_head = repo
                .find_reference(&format!("refs/remotes/origin/{}", self.git_ref))
                .or_else(|_| repo.find_reference("FETCH_HEAD"))
                .context("Failed to find fetched ref")?;

            let oid = fetch_head.target().context("Ref has no target")?;

            // Reset working directory to fetched ref
            let object = repo.find_object(oid, None)?;
            repo.reset(&object, git2::ResetType::Hard, None)
                .context("Failed to reset to fetched ref")?;

            Ok(oid.to_string())
        } else {
            // Clone fresh
            std::fs::create_dir_all(&self.clone_dir).context("Failed to create clone directory")?;

            let mut builder = git2::build::RepoBuilder::new();

            let mut fetch_options = git2::FetchOptions::new();
            let callbacks = Self::build_remote_callbacks(&self.auth);
            fetch_options.remote_callbacks(callbacks);
            builder.fetch_options(fetch_options);
            builder.branch(&self.git_ref);

            let repo = builder
                .clone(&self.url, &self.clone_dir)
                .context("Failed to clone git repository")?;

            let head = repo.head().context("Failed to get HEAD")?;
            let oid = head.target().context("HEAD has no target")?;

            Ok(oid.to_string())
        }
    }

    fn build_remote_callbacks(auth: &Option<GitAuthConfig>) -> git2::RemoteCallbacks<'_> {
        let mut callbacks = git2::RemoteCallbacks::new();

        // Accept SSH host keys (the container has no known_hosts file)
        callbacks.certificate_check(|_cert, _host| Ok(git2::CertificateCheckStatus::CertificateOk));

        if let Some(auth) = auth {
            match auth.auth_type.as_str() {
                "ssh_key" => {
                    let key_path = auth.key_path.clone();
                    let key_content = auth.key.clone();
                    callbacks.credentials(move |_url, username_from_url, _allowed_types| {
                        let username = username_from_url.unwrap_or("git");
                        if let Some(ref content) = key_content {
                            git2::Cred::ssh_key_from_memory(username, None, content, None)
                        } else if let Some(ref path) = key_path {
                            git2::Cred::ssh_key(username, None, Path::new(path), None)
                        } else {
                            git2::Cred::ssh_key_from_agent(username)
                        }
                    });
                }
                "token" => {
                    let token = auth.token.clone().unwrap_or_default();
                    let username = auth
                        .username
                        .clone()
                        .unwrap_or_else(|| "x-access-token".to_string());
                    callbacks.credentials(move |_url, _username_from_url, _allowed_types| {
                        git2::Cred::userpass_plaintext(&username, &token)
                    });
                }
                _ => {}
            }
        }

        callbacks
    }
}

#[async_trait]
impl WorkspaceSource for GitSource {
    async fn load(&self) -> Result<WorkspaceConfig> {
        let oid = tokio::task::block_in_place(|| self.clone_or_fetch())
            .context("Git clone/fetch failed")?;

        if let Ok(mut rev) = self.revision.write() {
            *rev = Some(oid);
        }

        super::load_folder_workspace(self.clone_dir.to_str().unwrap_or("")).await
    }

    fn path(&self) -> &Path {
        &self.clone_dir
    }

    fn revision(&self) -> Option<String> {
        self.revision.read().ok().and_then(|r| r.clone())
    }

    fn peek_revision(&self) -> Option<String> {
        // Git sources detect changes via fetch in load(), so peek returns
        // None to always trigger a full reload cycle (which does git fetch).
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clone_local_repo_loads_config() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "deploy.yaml",
            "actions:\n  greet:\n    type: shell\n    cmd: echo hello\ntasks:\n  hello:\n    flow:\n      step1:\n        action: greet\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let config = source.load().await.unwrap();
        assert_eq!(config.actions.len(), 1);
        assert!(config.actions.contains_key("greet"));
        assert_eq!(config.tasks.len(), 1);
        assert!(config.tasks.contains_key("hello"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clone_sets_revision_to_git_oid() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: shell\n    cmd: echo hi\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        assert!(source.revision().is_none());
        source.load().await.unwrap();

        let rev = source.revision().unwrap();
        assert_eq!(rev.len(), 40, "Git SHA-1 should be 40 hex chars");
        assert!(
            rev.chars().all(|c| c.is_ascii_hexdigit()),
            "Revision should be hex: {}",
            rev
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_fetch_detects_new_commit() {
        let (bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: shell\n    cmd: echo v1\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        source.load().await.unwrap();
        let rev1 = source.revision().unwrap();

        // Push a new commit to the bare repo
        add_commit(
            bare_dir.path(),
            "main",
            &[(
                "test.yaml",
                "actions:\n  a:\n    type: shell\n    cmd: echo v2\n",
            )],
            "update",
        );

        source.load().await.unwrap();
        let rev2 = source.revision().unwrap();

        assert_ne!(rev1, rev2, "Revision should change after new commit");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reload_no_changes_same_revision() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  a:\n    type: shell\n    cmd: echo stable\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        source.load().await.unwrap();
        let rev1 = source.revision().unwrap();

        source.load().await.unwrap();
        let rev2 = source.revision().unwrap();

        assert_eq!(rev1, rev2, "Revision should not change without new commits");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_config_updates_after_commit() {
        let (bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  greet:\n    type: shell\n    cmd: echo hello\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let config1 = source.load().await.unwrap();
        assert_eq!(config1.actions.len(), 1);

        // Add a second action
        add_commit(
            bare_dir.path(),
            "main",
            &[(
                "test.yaml",
                "actions:\n  greet:\n    type: shell\n    cmd: echo hello\n  build:\n    type: shell\n    cmd: make\ntasks:\n  t1:\n    flow:\n      s1:\n        action: greet\n",
            )],
            "add build action",
        );

        let config2 = source.load().await.unwrap();
        assert_eq!(config2.actions.len(), 2);
        assert!(config2.actions.contains_key("greet"));
        assert!(config2.actions.contains_key("build"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clone_specific_branch() {
        let (bare_dir, url) = create_bare_repo(&[(
            "test.yaml",
            "actions:\n  main_action:\n    type: shell\n    cmd: echo main\n",
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
                "actions:\n  dev_action:\n    type: shell\n    cmd: echo develop\n",
            )],
            "develop commit",
        );

        let clone_dir = TempDir::new().unwrap();
        let source =
            GitSource::with_clone_dir(&url, "develop", None, clone_dir.path().join("repo"));

        let config = source.load().await.unwrap();
        assert!(
            config.actions.contains_key("dev_action"),
            "Should have develop branch action"
        );
        assert!(
            !config.actions.contains_key("main_action"),
            "Should not have main branch action"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clone_creates_working_directory() {
        let (_bare_dir, url) = create_bare_repo(&[(
            "deploy.yaml",
            "actions:\n  a:\n    type: shell\n    cmd: echo hi\n",
        )]);

        let clone_dir = TempDir::new().unwrap();
        let repo_dir = clone_dir.path().join("repo");
        assert!(!repo_dir.exists());

        let source = GitSource::with_clone_dir(&url, "main", None, repo_dir.clone());
        source.load().await.unwrap();

        assert!(repo_dir.exists(), "Clone dir should exist after load");
        assert!(
            repo_dir.join("deploy.yaml").exists(),
            "Cloned file should exist"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multiple_yaml_files_merged() {
        let (_bare_dir, url) = create_bare_repo(&[
            (
                "actions.yaml",
                "actions:\n  greet:\n    type: shell\n    cmd: echo hi\n  build:\n    type: shell\n    cmd: make\n",
            ),
            (
                "tasks.yaml",
                "tasks:\n  deploy:\n    flow:\n      s1:\n        action: greet\n  ci:\n    flow:\n      s1:\n        action: build\n",
            ),
        ]);

        let clone_dir = TempDir::new().unwrap();
        let source = GitSource::with_clone_dir(&url, "main", None, clone_dir.path().join("repo"));

        let config = source.load().await.unwrap();
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
                GitSource::new(name, "https://example.com/repo.git", "main", None).unwrap();

            let expected_path = std::env::temp_dir().join("stroem").join("git").join(name);
            assert_eq!(source.clone_dir, expected_path);
        }
    }

    #[test]
    fn test_revision_before_load_returns_none() {
        let source = GitSource::new(
            "test-workspace",
            "https://github.com/example/repo.git",
            "main",
            None,
        )
        .unwrap();

        assert_eq!(source.revision(), None);
    }

    #[test]
    fn test_path_returns_clone_dir() {
        let source = GitSource::new(
            "test-workspace",
            "https://github.com/example/repo.git",
            "main",
            None,
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
        let _callbacks = GitSource::build_remote_callbacks(&auth);
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
        let _callbacks = GitSource::build_remote_callbacks(&auth);
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
        let _callbacks = GitSource::build_remote_callbacks(&auth);
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
        let _callbacks = GitSource::build_remote_callbacks(&auth);
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
        let _callbacks = GitSource::build_remote_callbacks(&auth);
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
        let _callbacks = GitSource::build_remote_callbacks(&auth);
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
        let _callbacks = GitSource::build_remote_callbacks(&auth);
    }

    #[test]
    fn test_clone_or_fetch_with_nonexistent_url_errors() {
        let workspace_name = format!("test-nonexistent-{}", uuid::Uuid::new_v4());

        let source = GitSource::new(
            &workspace_name,
            "https://github.com/nonexistent-user-12345/nonexistent-repo-67890.git",
            "main",
            None,
        )
        .unwrap();

        let result = source.clone_or_fetch();
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
}
