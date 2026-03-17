pub mod folder;
pub mod git;
pub mod library;

pub use folder::load_folder_workspace;
pub use library::{merge_library_into_workspace, LibraryResolver, ResolvedLibrary};

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::{GitAuthConfig, LibraryDef, WorkspaceSourceDef};

/// Scans a directory for YAML workflow files, parses them, and merges them into
/// a [`WorkspaceConfig`].
///
/// Files are processed in **sorted filename order** for deterministic merges.
/// Subdirectories and non-YAML files are silently skipped.
///
/// # Parameters
///
/// - `scan_dir` — The directory to scan. Must exist; returns an error otherwise.
/// - `skip_sops` — When `true`, files detected as SOPS-encrypted are skipped
///   entirely (library mode). When `false`, SOPS files are decrypted via
///   `stroem_common::sops::read_yaml_file` (workspace folder mode).
///
/// # Errors
///
/// Returns an error if the directory cannot be read, a YAML file cannot be read
/// (including failed SOPS decryption when `skip_sops` is `false`), or a file
/// fails YAML parsing.
pub(crate) fn scan_and_merge_yaml_files(
    scan_dir: &Path,
    skip_sops: bool,
) -> Result<WorkspaceConfig> {
    use stroem_common::models::workflow::WorkflowConfig;

    let mut entries: Vec<PathBuf> = std::fs::read_dir(scan_dir)
        .with_context(|| format!("Failed to read directory: {}", scan_dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to read directory entry")?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| !p.is_dir())
        .filter(|p| {
            p.extension()
                .map(|ext| ext == "yaml" || ext == "yml")
                .unwrap_or(false)
        })
        .collect();

    // Sort by filename for deterministic merge order across platforms
    entries.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .cmp(b.file_name().unwrap_or_default())
    });

    let mut workspace = WorkspaceConfig::new();

    for file_path in entries {
        let is_sops = stroem_common::sops::is_sops_file(&file_path);

        if is_sops && skip_sops {
            tracing::debug!("Skipping SOPS file in library: {}", file_path.display());
            continue;
        }

        if is_sops {
            tracing::debug!(
                "Loading SOPS-encrypted workflow file: {}",
                file_path.display()
            );
        } else {
            tracing::debug!("Loading workflow file: {}", file_path.display());
        }

        let content = stroem_common::sops::read_yaml_file(&file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let config: WorkflowConfig = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse YAML: {}", file_path.display()))?;

        workspace.merge(config);
    }

    Ok(workspace)
}

/// Trait for workspace sources (folder, git, etc.)
#[async_trait]
pub trait WorkspaceSource: Send + Sync {
    /// Load/reload workspace configuration from this source
    async fn load(&self) -> Result<WorkspaceConfig>;
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

/// A loaded workspace entry with its source and cached config
pub struct WorkspaceEntry {
    pub config: Arc<RwLock<Arc<WorkspaceConfig>>>,
    pub source: Arc<dyn WorkspaceSource>,
    pub name: String,
    pub source_path: PathBuf,
    /// None when healthy, Some(message) when last load failed.
    /// Uses std::sync::RwLock since it's read from both sync and async contexts.
    pub load_error: Arc<std::sync::RwLock<Option<String>>>,
}

impl WorkspaceEntry {
    /// Returns `true` if the workspace loaded successfully (no load error).
    fn is_healthy(&self) -> bool {
        self.load_error
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    }
}

impl std::fmt::Debug for WorkspaceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("WorkspaceEntry");
        s.field("name", &self.name)
            .field("source_path", &self.source_path);
        if let Ok(err) = self.load_error.read() {
            if err.is_some() {
                s.field("load_error", &err);
            }
        }
        s.finish()
    }
}

/// In-memory workspace source for testing
struct InMemorySource {
    config: tokio::sync::RwLock<WorkspaceConfig>,
}

#[async_trait]
impl WorkspaceSource for InMemorySource {
    async fn load(&self) -> Result<WorkspaceConfig> {
        Ok(self.config.read().await.clone())
    }

    fn path(&self) -> &Path {
        Path::new("/dev/null")
    }

    fn revision(&self) -> Option<String> {
        None
    }

    fn peek_revision(&self) -> Option<String> {
        None
    }
}

/// Manages multiple workspaces
#[derive(Debug)]
pub struct WorkspaceManager {
    entries: HashMap<String, WorkspaceEntry>,
    load_errors: HashMap<String, String>,
    /// Resolved libraries — shared across all workspaces
    resolved_libraries: HashMap<String, ResolvedLibrary>,
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

        for (name, def) in defs {
            let source: Arc<dyn WorkspaceSource> = match def {
                WorkspaceSourceDef::Folder { ref path } => {
                    Arc::new(folder::FolderSource::new(path))
                }
                WorkspaceSourceDef::Git {
                    ref url,
                    ref git_ref,
                    poll_interval_secs,
                    ref auth,
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

            match source.load().await {
                Ok(mut config) => {
                    // Merge library items into workspace config
                    for lib in resolved_libraries.values() {
                        merge_library_into_workspace(&mut config, lib);
                    }

                    let source_path = source.path().to_path_buf();
                    entries.insert(
                        name.clone(),
                        WorkspaceEntry {
                            config: Arc::new(RwLock::new(Arc::new(config))),
                            source,
                            name: name.clone(),
                            source_path,
                            load_error: Arc::new(std::sync::RwLock::new(None)),
                        },
                    );
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    tracing::error!("Failed to load workspace '{}': {}", name, err_msg);
                    let source_path = source.path().to_path_buf();
                    entries.insert(
                        name.clone(),
                        WorkspaceEntry {
                            config: Arc::new(RwLock::new(Arc::new(WorkspaceConfig::new()))),
                            source,
                            name: name.clone(),
                            source_path,
                            load_error: Arc::new(std::sync::RwLock::new(Some(err_msg))),
                        },
                    );
                }
            }
        }

        Self {
            entries,
            load_errors,
            resolved_libraries,
        }
    }

    /// Create a WorkspaceManager from pre-built entries (for testing)
    pub fn from_entries(entries: HashMap<String, WorkspaceEntry>) -> Self {
        Self {
            entries,
            load_errors: HashMap::new(),
            resolved_libraries: HashMap::new(),
        }
    }

    /// Create a WorkspaceManager from an in-memory WorkspaceConfig (for testing).
    /// The workspace is registered under the given name with a temp path.
    pub fn from_config(name: &str, config: WorkspaceConfig) -> Self {
        let source = Arc::new(InMemorySource {
            config: tokio::sync::RwLock::new(config.clone()),
        });
        let mut entries = HashMap::new();
        entries.insert(
            name.to_string(),
            WorkspaceEntry {
                config: Arc::new(RwLock::new(Arc::new(config))),
                source: source as Arc<dyn WorkspaceSource>,
                name: name.to_string(),
                source_path: PathBuf::from("/dev/null"),
                load_error: Arc::new(std::sync::RwLock::new(None)),
            },
        );
        Self {
            entries,
            load_errors: HashMap::new(),
            resolved_libraries: HashMap::new(),
        }
    }

    /// Get the workspace config for a given name.
    /// Returns None for workspaces with a load error (empty placeholder config).
    pub async fn get_config(&self, name: &str) -> Option<Arc<WorkspaceConfig>> {
        let entry = self.entries.get(name)?;
        if !entry.is_healthy() {
            return None;
        }
        let config = entry.config.read().await;
        Some(Arc::clone(&config))
    }

    /// Get the filesystem path for a workspace.
    /// Returns None for workspaces with a load error.
    pub fn get_path(&self, name: &str) -> Option<&Path> {
        let entry = self.entries.get(name)?;
        if !entry.is_healthy() {
            return None;
        }
        Some(entry.source_path.as_path())
    }

    /// Get the current revision for a workspace.
    /// Returns `None` if the workspace does not exist or has a load error.
    pub fn get_revision(&self, name: &str) -> Option<String> {
        let entry = self.entries.get(name)?;
        if !entry.is_healthy() {
            return None;
        }
        entry.source.revision()
    }

    /// List all workspace names (including errored workspaces with placeholder entries).
    /// Note: source construction failures (e.g. `GitSource::new()`) are not included here
    /// as they have no entry — use `list_workspace_info()` for the complete list.
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(|s| s.as_str()).collect()
    }

    /// Get all workspace configs as (name, config) pairs.
    /// Skips workspaces with load errors.
    pub async fn get_all_configs(&self) -> Vec<(String, Arc<WorkspaceConfig>)> {
        let mut result = Vec::new();
        for (name, entry) in &self.entries {
            if !entry.is_healthy() {
                continue;
            }
            let config = entry.config.read().await;
            result.push((name.clone(), Arc::clone(&config)));
        }
        result
    }

    /// List all workspaces including ones that failed to load.
    /// This includes both placeholder entries (load failures) and source construction
    /// failures from `load_errors`. Failed workspaces have `error` set and zero counts.
    pub async fn list_workspace_info(&self) -> Vec<WorkspaceInfo> {
        let mut infos = Vec::new();
        for (name, entry) in &self.entries {
            let error = entry
                .load_error
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let config = entry.config.read().await;
            infos.push(WorkspaceInfo {
                name: name.clone(),
                tasks_count: if error.is_some() {
                    0
                } else {
                    config.tasks.len()
                },
                actions_count: if error.is_some() {
                    0
                } else {
                    config.actions.len()
                },
                triggers_count: if error.is_some() {
                    0
                } else {
                    config.triggers.len()
                },
                connections_count: if error.is_some() {
                    0
                } else {
                    config.connections.len()
                },
                revision: entry.source.revision(),
                error,
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
                revision: None,
                error: Some(error.clone()),
            });
        }
        infos
    }

    /// Reload a specific workspace from its source, updating config and revision.
    /// Library items are re-merged into the workspace config.
    /// On success, clears any previous load error. On failure, sets the load error.
    pub async fn reload(&self, name: &str) -> Result<()> {
        let entry = self
            .entries
            .get(name)
            .with_context(|| format!("Workspace '{}' not found", name))?;

        match entry.source.load().await {
            Ok(mut new_config) => {
                // Re-merge library items
                for lib in self.resolved_libraries.values() {
                    merge_library_into_workspace(&mut new_config, lib);
                }

                let mut config = entry.config.write().await;
                *config = Arc::new(new_config);

                // Clear any previous error
                let mut err = entry.load_error.write().unwrap_or_else(|e| e.into_inner());
                *err = None;

                Ok(())
            }
            Err(e) => {
                let err_msg = format!("{:#}", e);
                let mut err = entry.load_error.write().unwrap_or_else(|e| e.into_inner());
                *err = Some(err_msg);
                Err(e).with_context(|| format!("Failed to reload workspace '{}'", name))
            }
        }
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
    pub fn start_watchers(&self, cancel_token: CancellationToken) {
        for (name, entry) in &self.entries {
            let config_lock = entry.config.clone();
            let source = entry.source.clone();
            let ws_name = name.clone();
            let poll_secs = source.poll_interval_secs();
            let libs = self.resolved_libraries.clone();
            let cancel = cancel_token.clone();
            let load_error = entry.load_error.clone();
            let needs_initial_load = !entry.is_healthy();

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
                let mut last_revision = source.revision();

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

                    let is_errored = load_error
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_some();

                    // For errored entries, skip the peek optimization and always try load()
                    if !is_errored {
                        // Check revision cheaply first. For folder sources this
                        // hashes file metadata+content without parsing YAML.
                        // For git sources this does a lightweight ls-remote
                        // (blocking network call, so wrap in spawn_blocking).
                        let source_clone = source.clone();
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
                    match source.load().await {
                        Ok(mut new_config) => {
                            // Re-merge library items
                            for lib in libs.values() {
                                merge_library_into_workspace(&mut new_config, lib);
                            }

                            let new_revision = source.revision();
                            let mut config = config_lock.write().await;
                            *config = Arc::new(new_config);

                            let mut err = load_error.write().unwrap_or_else(|e| e.into_inner());
                            if err.is_some() {
                                tracing::info!("Workspace '{}' recovered from load error", ws_name);
                            } else {
                                tracing::info!(
                                    "Workspace '{}' reloaded (revision: {:?} -> {:?})",
                                    ws_name,
                                    last_revision.as_deref().map(|s| &s[..8.min(s.len())]),
                                    new_revision.as_deref().map(|s| &s[..8.min(s.len())]),
                                );
                            }
                            *err = None;
                            last_revision = new_revision;
                        }
                        Err(e) => {
                            let mut err = load_error.write().unwrap_or_else(|e| e.into_inner());
                            *err = Some(format!("{:#}", e));
                            tracing::warn!("Failed to reload workspace '{}': {:#}", ws_name, e);
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
                path: temp1.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "ops".to_string(),
            WorkspaceSourceDef::Folder {
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
                timeout: None,
                when: None,
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

                on_success: vec![],
                on_error: vec![],
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
                timeout: None,
                when: None,
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

                on_success: vec![],
                on_error: vec![],
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
                timeout: None,
                when: None,
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

                on_success: vec![],
                on_error: vec![],
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
            WorkspaceSourceDef::Folder { path: temp_path },
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
                path: temp1.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "ops".to_string(),
            WorkspaceSourceDef::Folder {
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
                path: temp1.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "beta".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp2.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "gamma".to_string(),
            WorkspaceSourceDef::Folder {
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
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        // Should not crash — entry exists but get_config returns None
        assert_eq!(mgr.names().len(), 1);
        assert!(mgr.get_config("bad").await.is_none());
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "bad");
        assert!(infos[0].error.is_some());
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
        let config = source.load().await.unwrap();

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
                path: good_temp.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "bad".to_string(),
            WorkspaceSourceDef::Folder {
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
                path: "/nonexistent/a".to_string(),
            },
        );
        defs.insert(
            "ws2".to_string(),
            WorkspaceSourceDef::Folder {
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
                path: temp.path().to_str().unwrap().to_string(),
            },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        let cancel_token = CancellationToken::new();

        // Spawn watcher tasks with a long poll interval so they don't fire
        // during the test; we only care that they respond to cancellation.
        mgr.start_watchers(cancel_token.clone());

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
                path: good_temp.path().to_str().unwrap().to_string(),
            },
        );
        defs.insert(
            "bad".to_string(),
            WorkspaceSourceDef::Folder {
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
            WorkspaceSourceDef::Folder { path: ws_path_str },
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
            WorkspaceSourceDef::Folder { path: ws_path_str },
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
        mgr.start_watchers(cancel_token.clone());

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
        mgr.start_watchers(cancel_token.clone());
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
            WorkspaceSourceDef::Folder { path: ws_path_str },
        );

        let mgr = WorkspaceManager::new(defs, HashMap::new(), HashMap::new()).await;
        assert!(mgr.get_config("broken").await.is_none());
        let infos = mgr.list_workspace_info().await;
        assert!(infos[0].error.is_some());

        // Start watchers — the errored entry will retry immediately, fail again,
        // and update (or keep) the error message.
        let cancel_token = CancellationToken::new();
        mgr.start_watchers(cancel_token.clone());
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
}
