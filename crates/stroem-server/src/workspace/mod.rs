pub mod folder;
pub mod git;

pub use folder::load_folder_workspace;

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use tokio::sync::RwLock;

use crate::config::WorkspaceSourceDef;

/// Trait for workspace sources (folder, git, etc.)
#[async_trait]
pub trait WorkspaceSource: Send + Sync {
    /// Load/reload workspace configuration from this source
    async fn load(&self) -> Result<WorkspaceConfig>;
    /// Filesystem path where the workspace files reside
    fn path(&self) -> &Path;
    /// Current revision identifier (content hash for folder, git OID for git)
    fn revision(&self) -> Option<String>;
}

/// A loaded workspace entry with its source and cached config
pub struct WorkspaceEntry {
    pub config: Arc<RwLock<WorkspaceConfig>>,
    pub source: Arc<dyn WorkspaceSource>,
    pub name: String,
    pub source_path: PathBuf,
}

impl std::fmt::Debug for WorkspaceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceEntry")
            .field("name", &self.name)
            .field("source_path", &self.source_path)
            .finish()
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
}

/// Manages multiple workspaces
#[derive(Debug)]
pub struct WorkspaceManager {
    entries: HashMap<String, WorkspaceEntry>,
}

impl WorkspaceManager {
    /// Create a new WorkspaceManager from config definitions
    pub async fn new(defs: HashMap<String, WorkspaceSourceDef>) -> Result<Self> {
        let mut entries = HashMap::new();

        for (name, def) in defs {
            let source: Arc<dyn WorkspaceSource> = match def {
                WorkspaceSourceDef::Folder { ref path } => {
                    Arc::new(folder::FolderSource::new(path))
                }
                WorkspaceSourceDef::Git {
                    ref url,
                    ref git_ref,
                    poll_interval_secs: _,
                    ref auth,
                } => Arc::new(git::GitSource::new(&name, url, git_ref, auth.clone())?),
            };

            let config = source
                .load()
                .await
                .with_context(|| format!("Failed to load workspace '{}'", name))?;

            let source_path = source.path().to_path_buf();

            entries.insert(
                name.clone(),
                WorkspaceEntry {
                    config: Arc::new(RwLock::new(config)),
                    source,
                    name: name.clone(),
                    source_path,
                },
            );
        }

        Ok(Self { entries })
    }

    /// Create a WorkspaceManager from pre-built entries (for testing)
    pub fn from_entries(entries: HashMap<String, WorkspaceEntry>) -> Self {
        Self { entries }
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
                config: Arc::new(RwLock::new(config)),
                source: source as Arc<dyn WorkspaceSource>,
                name: name.to_string(),
                source_path: PathBuf::from("/dev/null"),
            },
        );
        Self { entries }
    }

    /// Get the workspace config for a given name
    pub async fn get_config(&self, name: &str) -> Option<WorkspaceConfig> {
        self.entries
            .get(name)
            .map(|e| e.config.blocking_read().clone())
    }

    /// Get the workspace config (async-friendly)
    pub async fn get_config_async(&self, name: &str) -> Option<WorkspaceConfig> {
        match self.entries.get(name) {
            Some(entry) => Some(entry.config.read().await.clone()),
            None => None,
        }
    }

    /// Get the filesystem path for a workspace
    pub fn get_path(&self, name: &str) -> Option<&Path> {
        self.entries.get(name).map(|e| e.source_path.as_path())
    }

    /// Get the current revision for a workspace
    pub fn get_revision(&self, name: &str) -> Option<String> {
        self.entries.get(name).and_then(|e| e.source.revision())
    }

    /// List all workspace names
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(|s| s.as_str()).collect()
    }

    /// List all tasks across all workspaces: (workspace_name, task_name, task_count)
    pub async fn list_workspace_info(&self) -> Vec<WorkspaceInfo> {
        let mut infos = Vec::new();
        for (name, entry) in &self.entries {
            let config = entry.config.read().await;
            infos.push(WorkspaceInfo {
                name: name.clone(),
                tasks_count: config.tasks.len(),
                actions_count: config.actions.len(),
                revision: entry.source.revision(),
            });
        }
        infos
    }

    /// Reload a specific workspace from its source, updating config and revision
    pub async fn reload(&self, name: &str) -> Result<()> {
        let entry = self
            .entries
            .get(name)
            .with_context(|| format!("Workspace '{}' not found", name))?;
        let new_config = entry
            .source
            .load()
            .await
            .with_context(|| format!("Failed to reload workspace '{}'", name))?;
        let mut config = entry.config.write().await;
        *config = new_config;
        Ok(())
    }

    /// Start background watchers for hot-reload (folder watchers + git pollers)
    pub fn start_watchers(&self) {
        for (name, entry) in &self.entries {
            let config_lock = entry.config.clone();
            let source = entry.source.clone();
            let ws_name = name.clone();

            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    match source.load().await {
                        Ok(new_config) => {
                            let mut config = config_lock.write().await;
                            *config = new_config;
                            tracing::debug!("Reloaded workspace '{}'", ws_name);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reload workspace '{}': {}", ws_name, e);
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
    pub revision: Option<String>,
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
    type: shell
    cmd: "echo hello"
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
        assert_eq!(mgr.names().len(), 1);

        let config = mgr.get_config_async("default").await.unwrap();
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
    type: shell
    cmd: "make build"
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
        assert_eq!(mgr.names().len(), 2);

        let default_config = mgr.get_config_async("default").await.unwrap();
        assert!(default_config.tasks.contains_key("hello"));

        let ops_config = mgr.get_config_async("ops").await.unwrap();
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
        assert!(mgr.get_config_async("nonexistent").await.is_none());
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
        let infos = mgr.list_workspace_info().await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "default");
        assert_eq!(infos[0].tasks_count, 1);
        assert_eq!(infos[0].actions_count, 1);
    }

    #[tokio::test]
    async fn test_workspace_manager_empty() {
        let mgr = WorkspaceManager::new(HashMap::new()).await.unwrap();
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
                action_type: "shell".to_string(),
                cmd: Some("echo test".to_string()),
                script: None,
                image: None,
                command: None,
                env: None,
                workdir: None,
                resources: None,
                input: HashMap::new(),
                output: None,
            },
        );

        let mut flow = HashMap::new();
        flow.insert(
            "step1".to_string(),
            FlowStep {
                action: "test-action".to_string(),
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );

        config.tasks.insert(
            "test-task".to_string(),
            TaskDef {
                mode: "distributed".to_string(),
                input: HashMap::new(),
                flow,
            },
        );

        let mgr = WorkspaceManager::from_config("test", config);
        assert_eq!(mgr.names().len(), 1);
        assert!(mgr.names().contains(&"test"));

        let loaded_config = mgr.get_config_async("test").await.unwrap();
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
                action_type: "shell".to_string(),
                cmd: Some("echo 1".to_string()),
                script: None,
                image: None,
                command: None,
                env: None,
                workdir: None,
                resources: None,
                input: HashMap::new(),
                output: None,
            },
        );

        let mut flow1 = HashMap::new();
        flow1.insert(
            "step1".to_string(),
            FlowStep {
                action: "action1".to_string(),
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        config1.tasks.insert(
            "task1".to_string(),
            TaskDef {
                mode: "distributed".to_string(),
                input: HashMap::new(),
                flow: flow1,
            },
        );

        let mut config2 = WorkspaceConfig::new();
        config2.actions.insert(
            "action2".to_string(),
            ActionDef {
                action_type: "shell".to_string(),
                cmd: Some("echo 2".to_string()),
                script: None,
                image: None,
                command: None,
                env: None,
                workdir: None,
                resources: None,
                input: HashMap::new(),
                output: None,
            },
        );

        let mut flow2 = HashMap::new();
        flow2.insert(
            "step1".to_string(),
            FlowStep {
                action: "action2".to_string(),
                depends_on: vec![],
                input: HashMap::new(),
                continue_on_failure: false,
            },
        );
        config2.tasks.insert(
            "task2".to_string(),
            TaskDef {
                mode: "distributed".to_string(),
                input: HashMap::new(),
                flow: flow2,
            },
        );

        let mgr1 = WorkspaceManager::from_config("ws1", config1);
        let mgr2 = WorkspaceManager::from_config("ws2", config2);

        // Verify they're independent
        assert_eq!(mgr1.names().len(), 1);
        assert_eq!(mgr2.names().len(), 1);
        assert!(mgr1.get_config_async("ws1").await.is_some());
        assert!(mgr1.get_config_async("ws2").await.is_none());
        assert!(mgr2.get_config_async("ws2").await.is_some());
        assert!(mgr2.get_config_async("ws1").await.is_none());

        let config1_loaded = mgr1.get_config_async("ws1").await.unwrap();
        let config2_loaded = mgr2.get_config_async("ws2").await.unwrap();
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
        let revision = mgr.get_revision("default");
        assert!(revision.is_some());
        // Verify it's a valid hex string (blake2 hash)
        let rev_str = revision.unwrap();
        assert!(rev_str.len() > 0);
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
        let revision1 = mgr.get_revision("default").unwrap();

        // Modify the file content
        let workflows_dir = temp.path().join(".workflows");
        fs::write(
            workflows_dir.join("test.yaml"),
            r#"
actions:
  greet:
    type: shell
    cmd: "echo goodbye"
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
        let mgr2 = WorkspaceManager::new(defs2).await.unwrap();
        let revision2 = mgr2.get_revision("default").unwrap();

        // Revision should have changed
        assert_ne!(revision1, revision2);
    }

    #[tokio::test]
    async fn test_workspace_manager_nonexistent_folder_fails() {
        let mut defs = HashMap::new();
        defs.insert(
            "nonexistent".to_string(),
            WorkspaceSourceDef::Folder {
                path: "/nonexistent/path/12345/xyz".to_string(),
            },
        );

        let result = WorkspaceManager::new(defs).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
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
    type: shell
    cmd: "make build"
  deploy:
    type: shell
    cmd: "make deploy"
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
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

        let mgr = WorkspaceManager::new(defs).await.unwrap();
        let names = mgr.names();

        assert_eq!(names.len(), 3);
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(names.contains(&"gamma"));
    }

    #[tokio::test]
    async fn test_malformed_yaml_fails() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("bad.yaml"),
            r#"
actions:
  greet:
    type: shell
    cmd: "echo hello
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

        let result = WorkspaceManager::new(defs).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = err.to_string();
        // Should mention either YAML parsing or the workspace name
        assert!(err_str.contains("bad") || err_str.contains("parse") || err_str.contains("YAML"));
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
    type: shell
    cmd: "echo hello"
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
        let mgr = WorkspaceManager::new(defs).await.unwrap();

        let config1 = mgr.get_config_async("default").await.unwrap();
        assert_eq!(config1.actions.len(), 1);

        // Add a second action to the YAML
        fs::write(
            workflows_dir.join("test.yaml"),
            r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
  build:
    type: shell
    cmd: "make build"
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        // Before reload, config should still be stale
        let config_before = mgr.get_config_async("default").await.unwrap();
        assert_eq!(config_before.actions.len(), 1);

        // Reload should update config
        mgr.reload("default").await.unwrap();
        let config2 = mgr.get_config_async("default").await.unwrap();
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
            "actions:\n  a:\n    type: shell\n    cmd: echo v1\n",
        )
        .unwrap();

        let mut defs = HashMap::new();
        defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: temp.path().to_str().unwrap().to_string(),
            },
        );
        let mgr = WorkspaceManager::new(defs).await.unwrap();
        let rev1 = mgr.get_revision("default").unwrap();

        // Modify content
        fs::write(
            workflows_dir.join("test.yaml"),
            "actions:\n  a:\n    type: shell\n    cmd: echo v2\n",
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
        let mgr = WorkspaceManager::new(defs).await.unwrap();

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
    type: shell
    cmd: "echo 1"
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
    type: shell
    cmd: "echo 2"
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
}
