use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use stroem_common::models::workflow::WorkspaceConfig;

use crate::config::{GitAuthConfig, LibraryDef};
use stroem_common::workspace_loader::scan_and_merge_yaml_files;

/// Validate that a library name is safe for filesystem paths, tar entries, and URL segments.
/// Must start with an alphanumeric char, followed by alphanumeric, underscore, or hyphen.
/// No dots (dots are the namespace separator), no path separators, no traversal.
fn is_valid_library_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Resolved library — extracted config + source path for tarball overlay
#[derive(Debug, Clone)]
pub struct ResolvedLibrary {
    pub config: WorkspaceConfig,
    pub path: PathBuf,
}

/// Resolves library sources (folder and git) into workspace configs.
pub struct LibraryResolver {
    cache_dir: PathBuf,
    git_auth: HashMap<String, GitAuthConfig>,
}

impl LibraryResolver {
    pub fn new(cache_dir: PathBuf, git_auth: HashMap<String, GitAuthConfig>) -> Self {
        Self {
            cache_dir,
            git_auth,
        }
    }

    /// Resolve all libraries from their definitions.
    /// Returns map of library name → resolved library (config + source path).
    #[tracing::instrument(skip(self, libraries))]
    pub fn resolve_all(
        &self,
        libraries: &HashMap<String, LibraryDef>,
    ) -> Result<HashMap<String, ResolvedLibrary>> {
        let mut resolved = HashMap::new();

        for (lib_name, lib_def) in libraries {
            // Validate library name: must be safe for filesystem paths, tar entries, and URL segments
            if !is_valid_library_name(lib_name) {
                tracing::error!(
                    "Invalid library name '{}': must match [a-zA-Z0-9][a-zA-Z0-9_-]*",
                    lib_name
                );
                continue;
            }

            match self.resolve_one(lib_name, lib_def) {
                Ok(lib) => {
                    tracing::info!(
                        "Library '{}': {} actions, {} tasks, {} connection_types",
                        lib_name,
                        lib.config.actions.len(),
                        lib.config.tasks.len(),
                        lib.config.connection_types.len(),
                    );
                    resolved.insert(lib_name.clone(), lib);
                }
                Err(e) => {
                    tracing::error!("Failed to resolve library '{}': {:#}", lib_name, e);
                }
            }
        }

        Ok(resolved)
    }

    fn resolve_one(&self, lib_name: &str, lib_def: &LibraryDef) -> Result<ResolvedLibrary> {
        let (raw_config, warnings, lib_path) = match lib_def {
            LibraryDef::Folder { path } => {
                let p = PathBuf::from(path);
                if !p.exists() {
                    bail!("Library folder does not exist: {}", path);
                }
                let (config, warnings) = load_library_workspace(&p)
                    .with_context(|| format!("Failed to load folder library '{}'", lib_name))?;
                (config, warnings, p)
            }
            LibraryDef::Git { url, git_ref, auth } => {
                let auth_config = self.resolve_auth(auth.as_deref())?;
                let clone_dir = self.cache_dir.join(lib_name);
                let (config, warnings) =
                    clone_and_load(url, git_ref, auth_config.as_ref(), &clone_dir)
                        .with_context(|| format!("Failed to load git library '{}'", lib_name))?;
                (config, warnings, clone_dir)
            }
        };

        // Log any per-file warnings from the library load.
        // Library warnings are intentionally not surfaced in the WorkspaceInfo API response
        // because libraries are shared across workspaces — attaching them to one workspace
        // would be misleading. Operators should check server logs for library issues.
        for w in &warnings {
            tracing::warn!("Library '{}': {}", lib_name, w);
        }

        // Prefix and rewrite references
        let config = prefix_library(lib_name, raw_config);

        Ok(ResolvedLibrary {
            config,
            path: lib_path,
        })
    }

    fn resolve_auth(&self, auth_name: Option<&str>) -> Result<Option<GitAuthConfig>> {
        match auth_name {
            None => Ok(None),
            Some(name) => {
                let config = self
                    .git_auth
                    .get(name)
                    .with_context(|| format!("Git auth '{}' not found in git_auth config", name))?;
                Ok(Some(config.clone()))
            }
        }
    }
}

/// Load workspace config from a library path (same as folder source but without secrets/connections rendering).
/// Per-file warnings (files skipped due to parse errors) are prefixed with `"library:"` and returned.
fn load_library_workspace(path: &Path) -> Result<(WorkspaceConfig, Vec<String>)> {
    let workflows_dir = path.join(".workflows");
    let scan_dir = if workflows_dir.exists() && workflows_dir.is_dir() {
        workflows_dir
    } else {
        path.to_path_buf()
    };

    if !scan_dir.exists() {
        return Ok((WorkspaceConfig::new(), Vec::new()));
    }

    // Scan YAML files, skipping SOPS-encrypted files (libraries don't decrypt secrets)
    let (workspace, raw_warnings) = scan_and_merge_yaml_files(&scan_dir, true, false)
        .with_context(|| format!("Failed to scan library directory: {}", scan_dir.display()))?;

    // Prefix warnings so callers can distinguish library warnings from workspace warnings
    let warnings = raw_warnings
        .into_iter()
        .map(|w| format!("library: {}", w))
        .collect();

    // Don't render secrets or connections for libraries — those are workspace-local
    Ok((workspace, warnings))
}

/// Clone (or fetch) a git library and load its workspace config.
fn clone_and_load(
    url: &str,
    git_ref: &str,
    auth: Option<&GitAuthConfig>,
    clone_dir: &Path,
) -> Result<(WorkspaceConfig, Vec<String>)> {
    clone_or_fetch(url, git_ref, auth, clone_dir)?;
    load_library_workspace(clone_dir)
}

/// Clone or fetch a git repository into `clone_dir`.
fn clone_or_fetch(
    url: &str,
    git_ref: &str,
    auth: Option<&GitAuthConfig>,
    clone_dir: &Path,
) -> Result<String> {
    if clone_dir.exists() {
        let repo =
            git2::Repository::open(clone_dir).context("Failed to open existing library clone")?;

        let mut remote = repo.find_remote("origin").context("No remote 'origin'")?;
        let mut fetch_options = git2::FetchOptions::new();
        let callbacks = build_remote_callbacks(auth);
        fetch_options.remote_callbacks(callbacks);

        remote
            .fetch(&[git_ref], Some(&mut fetch_options), None)
            .context("Failed to fetch library from origin")?;

        let fetch_head = repo
            .find_reference(&format!("refs/remotes/origin/{}", git_ref))
            .or_else(|_| repo.find_reference("FETCH_HEAD"))
            .context("Failed to find fetched ref")?;

        let oid = fetch_head.target().context("Ref has no target")?;
        let object = repo.find_object(oid, None)?;
        repo.reset(&object, git2::ResetType::Hard, None)
            .context("Failed to reset to fetched ref")?;

        Ok(oid.to_string())
    } else {
        std::fs::create_dir_all(clone_dir).context("Failed to create library clone directory")?;

        let mut builder = git2::build::RepoBuilder::new();
        let mut fetch_options = git2::FetchOptions::new();
        let callbacks = build_remote_callbacks(auth);
        fetch_options.remote_callbacks(callbacks);
        builder.fetch_options(fetch_options);
        builder.branch(git_ref);

        let repo = builder
            .clone(url, clone_dir)
            .context("Failed to clone library git repository")?;

        let head = repo.head().context("Failed to get HEAD")?;
        let oid = head.target().context("HEAD has no target")?;

        Ok(oid.to_string())
    }
}

fn build_remote_callbacks(auth: Option<&GitAuthConfig>) -> git2::RemoteCallbacks<'_> {
    let mut callbacks = git2::RemoteCallbacks::new();

    // Accept all TLS certificates. Library URLs come from the server admin's
    // config (not user input), and git2's default cert store detection is
    // unreliable across platforms. This mirrors the existing GitSource behavior.
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
            other => {
                tracing::warn!("Unknown git auth type '{}', skipping credentials", other);
            }
        }
    }

    callbacks
}

/// Prefix all library items and rewrite internal references.
///
/// - Actions: `slack-notify` → `common.slack-notify`
/// - Tasks: `canary-deploy` → `common.canary-deploy`
/// - Connection types: `postgres` → `common.postgres`
/// - Internal references within tasks are rewritten to use prefixed names.
fn prefix_library(lib_name: &str, raw: WorkspaceConfig) -> WorkspaceConfig {
    let prefix = format!("{}.", lib_name);

    // Collect the original action/task/connection-type names for reference rewriting
    let original_action_names: std::collections::HashSet<String> =
        raw.actions.keys().cloned().collect();
    let original_task_names: std::collections::HashSet<String> =
        raw.tasks.keys().cloned().collect();
    let original_conn_type_names: std::collections::HashSet<String> =
        raw.connection_types.keys().cloned().collect();

    let mut result = WorkspaceConfig::new();

    // Prefix actions
    for (name, action) in raw.actions {
        result.actions.insert(format!("{}{}", prefix, name), action);
    }

    // Prefix connection types
    for (name, conn_type) in raw.connection_types {
        result
            .connection_types
            .insert(format!("{}{}", prefix, name), conn_type);
    }

    // Prefix tasks and rewrite internal references
    for (name, mut task) in raw.tasks {
        // Rewrite action references in flow steps
        for step in task.flow.values_mut() {
            if original_action_names.contains(&step.action) {
                step.action = format!("{}{}", prefix, step.action);
            }
        }

        // Rewrite hook action references
        for hook in &mut task.on_success {
            if original_action_names.contains(&hook.action) {
                hook.action = format!("{}{}", prefix, hook.action);
            }
        }
        for hook in &mut task.on_error {
            if original_action_names.contains(&hook.action) {
                hook.action = format!("{}{}", prefix, hook.action);
            }
        }
        for hook in &mut task.on_suspended {
            if original_action_names.contains(&hook.action) {
                hook.action = format!("{}{}", prefix, hook.action);
            }
        }
        for hook in &mut task.on_cancel {
            if original_action_names.contains(&hook.action) {
                hook.action = format!("{}{}", prefix, hook.action);
            }
        }

        // Rewrite connection-type input field references in task inputs
        for input_field in task.input.values_mut() {
            if original_conn_type_names.contains(&input_field.field_type) {
                input_field.field_type = format!("{}{}", prefix, input_field.field_type);
            }
        }

        result.tasks.insert(format!("{}{}", prefix, name), task);
    }

    // Rewrite type:task references and connection-type input references in actions
    for action in result.actions.values_mut() {
        // Rewrite task references
        if let Some(ref task_ref) = action.task {
            if original_task_names.contains(task_ref) {
                action.task = Some(format!("{}{}", prefix, task_ref));
            }
        }

        // Rewrite connection-type input field references in action inputs
        for input_field in action.input.values_mut() {
            if original_conn_type_names.contains(&input_field.field_type) {
                input_field.field_type = format!("{}{}", prefix, input_field.field_type);
            }
        }
    }

    // Ignore triggers, secrets, and connections — those are workspace-local
    result
}

/// Merge resolved library items into a workspace config.
/// Called after workspace loading to add library actions, tasks, and connection types.
pub fn merge_library_into_workspace(workspace: &mut WorkspaceConfig, lib: &ResolvedLibrary) {
    for (name, action) in &lib.config.actions {
        if workspace.actions.contains_key(name) {
            tracing::warn!(
                "Library action '{}' collides with existing action, library wins",
                name
            );
        }
        workspace.actions.insert(name.clone(), action.clone());
    }
    for (name, task) in &lib.config.tasks {
        if workspace.tasks.contains_key(name) {
            tracing::warn!(
                "Library task '{}' collides with existing task, library wins",
                name
            );
        }
        workspace.tasks.insert(name.clone(), task.clone());
    }
    for (name, conn_type) in &lib.config.connection_types {
        if workspace.connection_types.contains_key(name) {
            tracing::warn!(
                "Library connection type '{}' collides with existing type, library wins",
                name
            );
        }
        workspace
            .connection_types
            .insert(name.clone(), conn_type.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use stroem_common::models::workflow::{ActionDef, FlowStep, HookDef, InputFieldDef, TaskDef};
    use tempfile::TempDir;

    fn make_shell_action(cmd: &str) -> ActionDef {
        ActionDef {
            action_type: "script".to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some(cmd.to_string()),
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
        }
    }

    fn make_task_action(task_ref: &str) -> ActionDef {
        ActionDef {
            action_type: "task".to_string(),
            name: None,
            description: None,
            task: Some(task_ref.to_string()),
            cmd: None,
            script: None,
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
        }
    }

    fn make_flow_step(action: &str) -> FlowStep {
        FlowStep {
            action: action.to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input: HashMap::new(),
            continue_on_failure: false,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        }
    }

    #[test]
    fn test_prefix_library_actions() {
        let mut ws = WorkspaceConfig::new();
        ws.actions
            .insert("slack-notify".to_string(), make_shell_action("echo notify"));
        ws.actions.insert(
            "pagerduty-alert".to_string(),
            make_shell_action("echo alert"),
        );

        let prefixed = prefix_library("common", ws);

        assert_eq!(prefixed.actions.len(), 2);
        assert!(prefixed.actions.contains_key("common.slack-notify"));
        assert!(prefixed.actions.contains_key("common.pagerduty-alert"));
        assert!(!prefixed.actions.contains_key("slack-notify"));
    }

    #[test]
    fn test_prefix_library_tasks() {
        let mut ws = WorkspaceConfig::new();
        ws.actions
            .insert("deploy".to_string(), make_shell_action("echo deploy"));

        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_flow_step("deploy"));
        ws.tasks.insert(
            "full-deploy".to_string(),
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

        let prefixed = prefix_library("infra", ws);

        assert_eq!(prefixed.tasks.len(), 1);
        assert!(prefixed.tasks.contains_key("infra.full-deploy"));

        // Action reference within the task should be rewritten
        let task = &prefixed.tasks["infra.full-deploy"];
        assert_eq!(task.flow["step1"].action, "infra.deploy");
    }

    #[test]
    fn test_prefix_library_connection_types() {
        use stroem_common::models::workflow::{ConnectionPropertyDef, ConnectionTypeDef};

        let mut ws = WorkspaceConfig::new();
        let mut props = HashMap::new();
        props.insert(
            "host".to_string(),
            ConnectionPropertyDef {
                property_type: "string".to_string(),
                required: true,
                default: None,
                secret: false,
            },
        );
        ws.connection_types.insert(
            "postgres".to_string(),
            ConnectionTypeDef { properties: props },
        );

        let prefixed = prefix_library("common", ws);

        assert_eq!(prefixed.connection_types.len(), 1);
        assert!(prefixed.connection_types.contains_key("common.postgres"));
    }

    #[test]
    fn test_prefix_library_rewrites_hook_action_refs() {
        let mut ws = WorkspaceConfig::new();
        ws.actions
            .insert("slack-notify".to_string(), make_shell_action("echo notify"));
        ws.actions
            .insert("deploy".to_string(), make_shell_action("echo deploy"));

        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_flow_step("deploy"));

        ws.tasks.insert(
            "my-task".to_string(),
            TaskDef {
                name: None,
                description: None,
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow,
                timeout: None,
                retry: None,
                on_success: vec![HookDef {
                    action: "slack-notify".to_string(),
                    input: HashMap::new(),
                }],
                on_error: vec![HookDef {
                    action: "slack-notify".to_string(),
                    input: HashMap::new(),
                }],
                on_suspended: vec![HookDef {
                    action: "slack-notify".to_string(),
                    input: HashMap::new(),
                }],
                on_cancel: vec![HookDef {
                    action: "slack-notify".to_string(),
                    input: HashMap::new(),
                }],
            },
        );

        let prefixed = prefix_library("lib", ws);

        let task = &prefixed.tasks["lib.my-task"];
        assert_eq!(task.on_success[0].action, "lib.slack-notify");
        assert_eq!(task.on_error[0].action, "lib.slack-notify");
        assert_eq!(task.on_suspended[0].action, "lib.slack-notify");
        assert_eq!(task.on_cancel[0].action, "lib.slack-notify");
    }

    #[test]
    fn test_prefix_library_rewrites_task_action_refs() {
        let mut ws = WorkspaceConfig::new();
        ws.actions
            .insert("run-canary".to_string(), make_task_action("canary-deploy"));
        ws.actions
            .insert("deploy".to_string(), make_shell_action("echo deploy"));

        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_flow_step("deploy"));
        ws.tasks.insert(
            "canary-deploy".to_string(),
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

        let prefixed = prefix_library("common", ws);

        // The action's task reference should be rewritten
        let action = &prefixed.actions["common.run-canary"];
        assert_eq!(action.task.as_deref(), Some("common.canary-deploy"));
    }

    #[test]
    fn test_prefix_library_rewrites_connection_type_input_refs() {
        use stroem_common::models::workflow::{ConnectionPropertyDef, ConnectionTypeDef};

        let mut ws = WorkspaceConfig::new();

        // Connection type
        let mut props = HashMap::new();
        props.insert(
            "host".to_string(),
            ConnectionPropertyDef {
                property_type: "string".to_string(),
                required: true,
                default: None,
                secret: false,
            },
        );
        ws.connection_types.insert(
            "postgres".to_string(),
            ConnectionTypeDef { properties: props },
        );

        // Action with connection-type input
        let mut action = make_shell_action("echo deploy");
        action.input.insert(
            "db".to_string(),
            InputFieldDef {
                field_type: "postgres".to_string(),
                name: None,
                description: None,
                required: true,
                secret: false,
                default: None,
                options: None,
                allow_custom: false,
                order: None,
            },
        );
        ws.actions.insert("deploy".to_string(), action);

        // Task with connection-type input
        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_flow_step("deploy"));
        let mut task_input = HashMap::new();
        task_input.insert(
            "database".to_string(),
            InputFieldDef {
                field_type: "postgres".to_string(),
                name: None,
                description: None,
                required: true,
                secret: false,
                default: None,
                options: None,
                allow_custom: false,
                order: None,
            },
        );
        ws.tasks.insert(
            "my-task".to_string(),
            TaskDef {
                name: None,
                description: None,
                mode: "distributed".to_string(),
                folder: None,
                input: task_input,
                flow,
                timeout: None,
                retry: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
                on_cancel: vec![],
            },
        );

        let prefixed = prefix_library("common", ws);

        // Action input type should be rewritten
        let action = &prefixed.actions["common.deploy"];
        assert_eq!(action.input["db"].field_type, "common.postgres");

        // Task input type should be rewritten
        let task = &prefixed.tasks["common.my-task"];
        assert_eq!(task.input["database"].field_type, "common.postgres");
    }

    #[test]
    fn test_prefix_library_ignores_triggers_secrets_connections() {
        use stroem_common::models::workflow::{ConnectionDef, TriggerDef};

        let mut ws = WorkspaceConfig::new();
        ws.secrets
            .insert("api-key".to_string(), serde_json::json!("secret"));
        ws.connections.insert(
            "my-db".to_string(),
            ConnectionDef {
                connection_type: Some("postgres".to_string()),
                values: HashMap::new(),
            },
        );
        ws.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 0 * * *".to_string(),
                task: "deploy".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
                force_refresh: false,
            },
        );

        let prefixed = prefix_library("common", ws);

        // Triggers, secrets, and connections should be empty (not imported)
        assert!(prefixed.secrets.is_empty());
        assert!(prefixed.connections.is_empty());
        assert!(prefixed.triggers.is_empty());
    }

    #[test]
    fn test_prefix_library_unknown_action_ref_left_as_is() {
        let mut ws = WorkspaceConfig::new();
        ws.actions
            .insert("deploy".to_string(), make_shell_action("echo deploy"));

        let mut flow = HashMap::new();
        // Reference to an action not in this library
        flow.insert("step1".to_string(), make_flow_step("external-action"));
        ws.tasks.insert(
            "my-task".to_string(),
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

        let prefixed = prefix_library("lib", ws);

        // Unknown ref should be left as-is (will fail validation)
        let task = &prefixed.tasks["lib.my-task"];
        assert_eq!(task.flow["step1"].action, "external-action");
    }

    #[test]
    fn test_load_library_workspace_from_folder() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("actions.yaml"),
            r#"
actions:
  slack-notify:
    type: script
    script: "echo notify"
  pagerduty-alert:
    type: script
    script: "echo alert"
tasks:
  full-deploy:
    flow:
      notify:
        action: slack-notify
connection_types:
  postgres:
    host:
      type: string
      required: true
triggers:
  nightly:
    type: scheduler
    cron: "0 0 * * *"
    task: full-deploy
secrets:
  api-key: "should-be-ignored"
connections:
  my-db:
    type: postgres
    host: localhost
"#,
        )
        .unwrap();

        let (config, _) = load_library_workspace(temp.path()).unwrap();

        // Actions, tasks, and connection_types loaded
        assert_eq!(config.actions.len(), 2);
        assert!(config.actions.contains_key("slack-notify"));
        assert!(config.actions.contains_key("pagerduty-alert"));
        assert_eq!(config.tasks.len(), 1);
        assert!(config.tasks.contains_key("full-deploy"));
        assert_eq!(config.connection_types.len(), 1);
        assert!(config.connection_types.contains_key("postgres"));

        // Triggers, secrets, and connections are loaded (they'll be stripped during prefix_library)
        // load_library_workspace reads everything; prefix_library drops triggers/secrets/connections
    }

    #[test]
    fn test_load_library_workspace_empty_dir() {
        let temp = TempDir::new().unwrap();
        let (config, _) = load_library_workspace(temp.path()).unwrap();
        assert!(config.actions.is_empty());
        assert!(config.tasks.is_empty());
    }

    #[test]
    fn test_merge_library_into_workspace() {
        let mut workspace = WorkspaceConfig::new();
        workspace
            .actions
            .insert("local-action".to_string(), make_shell_action("echo local"));

        let mut lib_config = WorkspaceConfig::new();
        lib_config.actions.insert(
            "common.slack-notify".to_string(),
            make_shell_action("echo notify"),
        );

        let lib = ResolvedLibrary {
            config: lib_config,
            path: PathBuf::from("/tmp/lib"),
        };

        merge_library_into_workspace(&mut workspace, &lib);

        assert_eq!(workspace.actions.len(), 2);
        assert!(workspace.actions.contains_key("local-action"));
        assert!(workspace.actions.contains_key("common.slack-notify"));
    }

    #[test]
    fn test_folder_library_resolver() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("actions.yaml"),
            r#"
actions:
  slack-notify:
    type: script
    script: "echo notify"
tasks:
  deploy:
    flow:
      notify:
        action: slack-notify
"#,
        )
        .unwrap();

        let mut libs = HashMap::new();
        libs.insert(
            "common".to_string(),
            LibraryDef::Folder {
                path: temp.path().to_string_lossy().to_string(),
            },
        );

        let resolver = LibraryResolver::new(PathBuf::from("/tmp/cache"), HashMap::new());
        let resolved = resolver.resolve_all(&libs).unwrap();

        assert_eq!(resolved.len(), 1);
        let lib = &resolved["common"];
        assert!(lib.config.actions.contains_key("common.slack-notify"));
        assert!(lib.config.tasks.contains_key("common.deploy"));

        // Internal reference should be rewritten
        let task = &lib.config.tasks["common.deploy"];
        assert_eq!(task.flow["notify"].action, "common.slack-notify");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_git_library_resolver() {
        // Create a bare git repo with library content
        let bare_dir = TempDir::new().unwrap();
        let bare_repo = git2::Repository::init_bare(bare_dir.path()).unwrap();

        // Build a tree with .workflows directory
        let yaml_content = r#"actions:
  slack-notify:
    type: script
    script: echo notify
tasks:
  deploy:
    flow:
      notify:
        action: slack-notify
"#;
        // Create the file at root level (no .workflows subdir in flat tree)
        let blob_oid = bare_repo.blob(yaml_content.as_bytes()).unwrap();

        let mut tb = bare_repo.treebuilder(None).unwrap();
        tb.insert("actions.yaml", blob_oid, 0o100644).unwrap();
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

        let cache_dir = TempDir::new().unwrap();
        let mut libs = HashMap::new();
        libs.insert(
            "common".to_string(),
            LibraryDef::Git {
                url,
                git_ref: "main".to_string(),
                auth: None,
            },
        );

        let resolver = LibraryResolver::new(cache_dir.path().to_path_buf(), HashMap::new());
        let resolved = resolver.resolve_all(&libs).unwrap();

        assert_eq!(resolved.len(), 1);
        let lib = &resolved["common"];
        assert!(lib.config.actions.contains_key("common.slack-notify"));
        assert!(lib.config.tasks.contains_key("common.deploy"));
    }

    #[test]
    fn test_nonexistent_folder_library_fails() {
        let mut libs = HashMap::new();
        libs.insert(
            "missing".to_string(),
            LibraryDef::Folder {
                path: "/nonexistent/path/12345".to_string(),
            },
        );

        let resolver = LibraryResolver::new(PathBuf::from("/tmp/cache"), HashMap::new());
        let resolved = resolver.resolve_all(&libs).unwrap();

        // Should be empty (error logged, not propagated)
        assert!(resolved.is_empty());
    }

    #[test]
    fn test_auth_resolution_found() {
        let mut git_auth = HashMap::new();
        git_auth.insert(
            "my-token".to_string(),
            GitAuthConfig {
                auth_type: "token".to_string(),
                key_path: None,
                key: None,
                token: Some("ghp_xxx".to_string()),
                username: None,
            },
        );

        let resolver = LibraryResolver::new(PathBuf::from("/tmp"), git_auth);
        let auth = resolver.resolve_auth(Some("my-token")).unwrap();
        assert!(auth.is_some());
        assert_eq!(auth.unwrap().token.as_deref(), Some("ghp_xxx"));
    }

    #[test]
    fn test_auth_resolution_not_found() {
        let resolver = LibraryResolver::new(PathBuf::from("/tmp"), HashMap::new());
        let result = resolver.resolve_auth(Some("nonexistent"));
        assert!(result.is_err());
    }

    #[test]
    fn test_auth_resolution_none() {
        let resolver = LibraryResolver::new(PathBuf::from("/tmp"), HashMap::new());
        let auth = resolver.resolve_auth(None).unwrap();
        assert!(auth.is_none());
    }

    #[test]
    fn test_is_valid_library_name() {
        // Valid names
        assert!(is_valid_library_name("common"));
        assert!(is_valid_library_name("my-lib"));
        assert!(is_valid_library_name("lib_2"));
        assert!(is_valid_library_name("A"));
        assert!(is_valid_library_name("x123"));

        // Invalid names
        assert!(!is_valid_library_name(""));
        assert!(!is_valid_library_name("-starts-with-dash"));
        assert!(!is_valid_library_name("_starts-with-underscore"));
        assert!(!is_valid_library_name("has.dot"));
        assert!(!is_valid_library_name("has/slash"));
        assert!(!is_valid_library_name("has\\backslash"));
        assert!(!is_valid_library_name(".."));
        assert!(!is_valid_library_name("has space"));
        assert!(!is_valid_library_name("has@special"));
    }

    #[test]
    fn test_invalid_library_name_skipped_in_resolve() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".workflows");
        fs::create_dir(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("actions.yaml"),
            "actions:\n  notify:\n    type: script\n    script: echo hi\n",
        )
        .unwrap();

        let mut libs = HashMap::new();
        // Valid library
        libs.insert(
            "good".to_string(),
            LibraryDef::Folder {
                path: temp.path().to_string_lossy().to_string(),
            },
        );
        // Invalid library name (contains dot)
        libs.insert(
            "bad.name".to_string(),
            LibraryDef::Folder {
                path: temp.path().to_string_lossy().to_string(),
            },
        );
        // Invalid library name (path traversal)
        libs.insert(
            "..".to_string(),
            LibraryDef::Folder {
                path: temp.path().to_string_lossy().to_string(),
            },
        );

        let resolver = LibraryResolver::new(PathBuf::from("/tmp/cache"), HashMap::new());
        let resolved = resolver.resolve_all(&libs).unwrap();

        // Only the valid one should be resolved
        assert_eq!(resolved.len(), 1);
        assert!(resolved.contains_key("good"));
    }

    #[test]
    fn test_merge_collision_overwrites() {
        let mut workspace = WorkspaceConfig::new();
        workspace.actions.insert(
            "common.notify".to_string(),
            make_shell_action("echo local-version"),
        );

        let mut lib_config = WorkspaceConfig::new();
        lib_config.actions.insert(
            "common.notify".to_string(),
            make_shell_action("echo lib-version"),
        );

        let lib = ResolvedLibrary {
            config: lib_config,
            path: PathBuf::from("/tmp/lib"),
        };

        merge_library_into_workspace(&mut workspace, &lib);

        // Library version should win
        assert_eq!(workspace.actions.len(), 1);
        assert_eq!(
            workspace.actions["common.notify"].script.as_deref(),
            Some("echo lib-version")
        );
    }

    /// Full integration test: folder library on disk → WorkspaceManager::new()
    /// → merged config → validation passes.
    ///
    /// Verifies that library actions, tasks, and connection types are properly
    /// prefixed, internal references are rewritten, and the merged workspace
    /// passes `validate_workflow_config_with_libraries`.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_full_library_pipeline_through_workspace_manager() {
        use crate::config::{LibraryDef, WorkspaceSourceDef};
        use crate::workspace::WorkspaceManager;
        use stroem_common::validation::validate_workflow_config_with_libraries;

        // --- Library temp dir ---
        let library_dir = TempDir::new().unwrap();
        let lib_workflows = library_dir.path().join(".workflows");
        fs::create_dir(&lib_workflows).unwrap();
        fs::write(
            lib_workflows.join("common.yaml"),
            r#"
actions:
  slack-notify:
    type: script
    script: "echo notifying"
  run-deploy:
    type: script
    script: "echo deploying"
tasks:
  deploy-pipeline:
    flow:
      deploy:
        action: run-deploy
      notify:
        action: slack-notify
        depends_on: [deploy]
connection_types:
  postgres:
    host:
      type: string
      required: true
    port:
      type: integer
      default: 5432
"#,
        )
        .unwrap();

        // --- Workspace temp dir ---
        let workspace_dir = TempDir::new().unwrap();
        let ws_workflows = workspace_dir.path().join(".workflows");
        fs::create_dir(&ws_workflows).unwrap();
        fs::write(
            ws_workflows.join("main.yaml"),
            r#"
actions:
  build:
    type: script
    script: "echo building"
tasks:
  ci-pipeline:
    flow:
      build:
        action: build
      deploy:
        action: common.run-deploy
        depends_on: [build]
      notify:
        action: common.slack-notify
        depends_on: [deploy]
"#,
        )
        .unwrap();

        // --- Build config maps ---
        let mut workspace_defs = HashMap::new();
        workspace_defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: workspace_dir.path().to_string_lossy().to_string(),
            },
        );

        let mut library_defs = HashMap::new();
        library_defs.insert(
            "common".to_string(),
            LibraryDef::Folder {
                path: library_dir.path().to_string_lossy().to_string(),
            },
        );

        // --- Create WorkspaceManager ---
        let mgr = WorkspaceManager::new(workspace_defs, library_defs, HashMap::new()).await;

        // --- Fetch the merged config ---
        let config = mgr
            .get_config("default")
            .await
            .expect("workspace 'default' should be loaded");

        // Workspace-local action is present
        assert!(
            config.actions.contains_key("build"),
            "workspace-local action 'build' must be in merged config"
        );

        // Library actions are present with namespace prefix
        assert!(
            config.actions.contains_key("common.slack-notify"),
            "library action 'common.slack-notify' must be in merged config"
        );
        assert!(
            config.actions.contains_key("common.run-deploy"),
            "library action 'common.run-deploy' must be in merged config"
        );

        // Library task is present with namespace prefix
        assert!(
            config.tasks.contains_key("common.deploy-pipeline"),
            "library task 'common.deploy-pipeline' must be in merged config"
        );

        // Library task's internal flow step references are rewritten to use prefixed names
        let lib_task = &config.tasks["common.deploy-pipeline"];
        assert_eq!(
            lib_task.flow["deploy"].action, "common.run-deploy",
            "library task's 'deploy' step must reference 'common.run-deploy'"
        );
        assert_eq!(
            lib_task.flow["notify"].action, "common.slack-notify",
            "library task's 'notify' step must reference 'common.slack-notify'"
        );

        // Library connection type is present with namespace prefix
        assert!(
            config.connection_types.contains_key("common.postgres"),
            "library connection type 'common.postgres' must be in merged config"
        );

        // Workspace task still references library actions by their prefixed names
        let ws_task = &config.tasks["ci-pipeline"];
        assert_eq!(
            ws_task.flow["deploy"].action, "common.run-deploy",
            "workspace task's 'deploy' step must reference 'common.run-deploy'"
        );
        assert_eq!(
            ws_task.flow["notify"].action, "common.slack-notify",
            "workspace task's 'notify' step must reference 'common.slack-notify'"
        );

        // Full validation with library references enabled — must return Ok
        let result = validate_workflow_config_with_libraries(&(*config).clone());
        assert!(
            result.is_ok(),
            "validate_workflow_config_with_libraries must succeed; error: {:?}",
            result.err()
        );
    }

    /// Integration test: a workspace that references a library action that does
    /// not exist should produce a validation error when calling
    /// `validate_workflow_config_with_libraries`.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_library_pipeline_missing_library_ref_fails_validation() {
        use crate::config::{LibraryDef, WorkspaceSourceDef};
        use crate::workspace::WorkspaceManager;
        use stroem_common::validation::validate_workflow_config_with_libraries;

        // --- Library temp dir (same as above) ---
        let library_dir = TempDir::new().unwrap();
        let lib_workflows = library_dir.path().join(".workflows");
        fs::create_dir(&lib_workflows).unwrap();
        fs::write(
            lib_workflows.join("common.yaml"),
            r#"
actions:
  slack-notify:
    type: script
    script: "echo notifying"
  run-deploy:
    type: script
    script: "echo deploying"
tasks:
  deploy-pipeline:
    flow:
      deploy:
        action: run-deploy
      notify:
        action: slack-notify
        depends_on: [deploy]
connection_types:
  postgres:
    host:
      type: string
      required: true
    port:
      type: integer
      default: 5432
"#,
        )
        .unwrap();

        // --- Workspace that references a nonexistent library action ---
        let workspace_dir = TempDir::new().unwrap();
        let ws_workflows = workspace_dir.path().join(".workflows");
        fs::create_dir(&ws_workflows).unwrap();
        fs::write(
            ws_workflows.join("main.yaml"),
            r#"
actions:
  build:
    type: script
    script: "echo building"
tasks:
  broken-pipeline:
    flow:
      build:
        action: build
      missing:
        action: common.nonexistent-action
        depends_on: [build]
"#,
        )
        .unwrap();

        // --- Build config maps ---
        let mut workspace_defs = HashMap::new();
        workspace_defs.insert(
            "default".to_string(),
            WorkspaceSourceDef::Folder {
                path: workspace_dir.path().to_string_lossy().to_string(),
            },
        );

        let mut library_defs = HashMap::new();
        library_defs.insert(
            "common".to_string(),
            LibraryDef::Folder {
                path: library_dir.path().to_string_lossy().to_string(),
            },
        );

        // --- Create WorkspaceManager ---
        let mgr = WorkspaceManager::new(workspace_defs, library_defs, HashMap::new()).await;

        // The workspace loads successfully (library loading is best-effort)
        let config = mgr
            .get_config("default")
            .await
            .expect("workspace 'default' should be loaded even with bad refs");

        // Library actions are still merged in
        assert!(
            config.actions.contains_key("common.slack-notify"),
            "library action 'common.slack-notify' must be present"
        );

        // But `common.nonexistent-action` is not in the library — validation must fail
        assert!(
            !config.actions.contains_key("common.nonexistent-action"),
            "'common.nonexistent-action' must NOT be in the merged config"
        );

        let result = validate_workflow_config_with_libraries(&(*config).clone());
        assert!(
            result.is_err(),
            "validate_workflow_config_with_libraries must return Err for unknown library ref"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("common.nonexistent-action"),
            "error message must mention the unknown action; got: {err_msg}"
        );
    }
}
