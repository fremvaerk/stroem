use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Connection type property definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionPropertyDef {
    #[serde(rename = "type")]
    pub property_type: String, // "string", "integer", "number", "boolean"
    #[serde(default)]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub secret: bool,
}

/// Connection type definition — a schema for connections
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionTypeDef {
    pub properties: HashMap<String, ConnectionPropertyDef>,
}

/// Connection definition — a named, typed object storing external system config.
///
/// Flat syntax: `type` is the connection type reference, all other fields are values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionDef {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    #[serde(flatten)]
    pub values: HashMap<String, serde_json::Value>,
}

/// Input field definition for actions and tasks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputFieldDef {
    #[serde(rename = "type")]
    pub field_type: String, // "string", "number", "boolean"
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// Output field definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFieldDef {
    #[serde(rename = "type")]
    pub field_type: String,
}

/// Output schema for actions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputDef {
    pub properties: HashMap<String, OutputFieldDef>,
}

/// Resource limits for actions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceDef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
}

/// Action definition - represents a reusable execution unit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDef {
    #[serde(rename = "type")]
    pub action_type: String, // "shell", "docker", "pod", "task"

    /// For type: task — the name of the task to execute as a sub-job
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,

    // Shell action fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>, // for shell
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>, // for shell (alternative to cmd)

    // Runner field (for shell actions: "local", "docker", "pod")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,

    // Tags for worker routing
    #[serde(default)]
    pub tags: Vec<String>,

    // Container fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>, // for docker, pod
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>, // for docker/pod
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>, // for docker/pod Type 1

    // Common fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceDef>,

    // Input/output schemas
    #[serde(default)]
    pub input: HashMap<String, InputFieldDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputDef>,

    /// Raw Kubernetes pod manifest overrides — deep-merged into the generated pod spec.
    /// Only valid on `type: pod` and `type: shell` + `runner: pod`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<serde_json::Value>,
}

/// Hook definition — an action to run when a job completes or fails
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    pub action: String,
    #[serde(default)]
    pub input: HashMap<String, serde_json::Value>,
}

/// A single step in a task flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStep {
    pub action: String, // action name or library/action
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub input: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub continue_on_failure: bool,
}

/// Task definition - represents a workflow with multiple steps
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDef {
    #[serde(default = "default_mode")]
    pub mode: String, // "distributed" or "local"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default)]
    pub input: HashMap<String, InputFieldDef>,
    pub flow: HashMap<String, FlowStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_success: Vec<HookDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_error: Vec<HookDef>,
}

fn default_mode() -> String {
    "distributed".to_string()
}

/// Trigger definition - represents automated task execution
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TriggerDef {
    #[serde(rename = "scheduler")]
    Scheduler {
        cron: String,
        task: String,
        #[serde(default)]
        input: HashMap<String, serde_json::Value>,
        #[serde(default = "default_true")]
        enabled: bool,
    },
    #[serde(rename = "webhook")]
    Webhook {
        name: String,
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
        #[serde(default)]
        input: HashMap<String, serde_json::Value>,
        #[serde(default = "default_true")]
        enabled: bool,
        /// Webhook response mode: "async" (default, fire-and-forget) or "sync" (wait for completion).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        /// Max seconds to wait in sync mode before returning 202 (default 30, max 300).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
}

impl TriggerDef {
    /// Get the task name this trigger targets.
    pub fn task(&self) -> &str {
        match self {
            TriggerDef::Scheduler { task, .. } => task,
            TriggerDef::Webhook { task, .. } => task,
        }
    }

    /// Get the input map for this trigger.
    pub fn input(&self) -> &HashMap<String, serde_json::Value> {
        match self {
            TriggerDef::Scheduler { input, .. } => input,
            TriggerDef::Webhook { input, .. } => input,
        }
    }

    /// Get whether this trigger is enabled.
    pub fn enabled(&self) -> bool {
        match self {
            TriggerDef::Scheduler { enabled, .. } => *enabled,
            TriggerDef::Webhook { enabled, .. } => *enabled,
        }
    }

    /// Get the trigger type as a string (for API responses).
    pub fn trigger_type_str(&self) -> &str {
        match self {
            TriggerDef::Scheduler { .. } => "scheduler",
            TriggerDef::Webhook { .. } => "webhook",
        }
    }
}

fn default_true() -> bool {
    true
}

/// Top-level workflow config - represents one YAML file
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowConfig {
    #[serde(default)]
    pub secrets: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub connection_types: HashMap<String, ConnectionTypeDef>,
    #[serde(default)]
    pub connections: HashMap<String, ConnectionDef>,
    #[serde(default)]
    pub actions: HashMap<String, ActionDef>,
    #[serde(default)]
    pub tasks: HashMap<String, TaskDef>,
    #[serde(default)]
    pub triggers: HashMap<String, TriggerDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_success: Vec<HookDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_error: Vec<HookDef>,
}

/// Merged workspace - all YAML files combined
#[derive(Debug, Clone, Default)]
pub struct WorkspaceConfig {
    pub secrets: HashMap<String, serde_json::Value>,
    pub connection_types: HashMap<String, ConnectionTypeDef>,
    pub connections: HashMap<String, ConnectionDef>,
    pub actions: HashMap<String, ActionDef>,
    pub tasks: HashMap<String, TaskDef>,
    pub triggers: HashMap<String, TriggerDef>,
    pub on_success: Vec<HookDef>,
    pub on_error: Vec<HookDef>,
}

impl WorkspaceConfig {
    /// Create a new empty workspace config
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge another workflow config into this workspace
    pub fn merge(&mut self, config: WorkflowConfig) {
        self.secrets.extend(config.secrets);
        self.connection_types.extend(config.connection_types);
        self.connections.extend(config.connections);
        self.actions.extend(config.actions);
        self.tasks.extend(config.tasks);
        self.triggers.extend(config.triggers);
        self.on_success.extend(config.on_success);
        self.on_error.extend(config.on_error);
    }

    /// Render secret values through Tera templates.
    ///
    /// This allows secrets to use template expressions at load time, e.g.:
    /// ```yaml
    /// secrets:
    ///   DB_URL: "{{ 'ref+awsssm:///prod/db/password' | vals }}"
    /// ```
    ///
    /// String values are rendered with an empty context. Non-string values
    /// and strings without template syntax pass through unchanged.
    pub fn render_secrets(&mut self) -> anyhow::Result<()> {
        let empty_context = serde_json::json!({});
        for (key, value) in &mut self.secrets {
            render_secret_value(value, &empty_context)
                .with_context(|| format!("Failed to render secret '{key}'"))?;
        }
        Ok(())
    }

    /// Render connection values through Tera templates and apply type defaults.
    ///
    /// Phase 1: Render template strings in connection values using secrets as context
    ///          (e.g. `{{ secret.db_host }}`, `{{ 'ref+...' | vals }}`).
    /// Phase 2: Apply default values from connection type properties for missing fields.
    pub fn render_connections(&mut self) -> anyhow::Result<()> {
        let context = serde_json::json!({ "secret": &self.secrets });

        // Phase 1: Render template values in connections (with secrets available)
        for (conn_name, conn) in &mut self.connections {
            for (key, value) in &mut conn.values {
                render_secret_value(value, &context).with_context(|| {
                    format!("Failed to render connection '{conn_name}' field '{key}'")
                })?;
            }
        }

        // Phase 2: Apply defaults from type properties (clone types to avoid borrow issues)
        let types = self.connection_types.clone();
        for (conn_name, conn) in &mut self.connections {
            if let Some(ref type_name) = conn.connection_type {
                if let Some(type_def) = types.get(type_name) {
                    for (prop_name, prop_def) in &type_def.properties {
                        if !conn.values.contains_key(prop_name) {
                            if let Some(ref default_value) = prop_def.default {
                                conn.values.insert(prop_name.clone(), default_value.clone());
                            }
                        }
                    }
                } else {
                    tracing::warn!(
                        "Connection '{}' references unknown type '{}'",
                        conn_name,
                        type_name
                    );
                }
            }
        }

        Ok(())
    }
}

/// Recursively render string values in a serde_json::Value through Tera.
fn render_secret_value(
    value: &mut serde_json::Value,
    context: &serde_json::Value,
) -> anyhow::Result<()> {
    use crate::template::render_template;

    match value {
        serde_json::Value::String(s) => {
            if s.contains("{{") {
                let rendered = render_template(s, context)?;
                *s = rendered;
            }
        }
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                render_secret_value(v, context)?;
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                render_secret_value(v, context)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_simple_workflow() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }}"
    input:
      name:
        type: string
        required: true

tasks:
  hello-world:
    mode: distributed
    input:
      name:
        type: string
        default: "World"
    flow:
      say-hello:
        action: greet
        input:
          name: "{{ input.name }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.actions.len(), 1);
        assert_eq!(config.tasks.len(), 1);

        let greet = config.actions.get("greet").unwrap();
        assert_eq!(greet.action_type, "shell");
        assert_eq!(greet.cmd.as_ref().unwrap(), "echo Hello {{ input.name }}");

        let task = config.tasks.get("hello-world").unwrap();
        assert_eq!(task.mode, "distributed");
        assert_eq!(task.flow.len(), 1);
    }

    #[test]
    fn test_parse_docker_action() {
        let yaml = r#"
actions:
  backup:
    type: docker
    image: company/db-backup:v2.1
    command: ["--compress", "--output", "/data/backup.sql"]
    env:
      DB_HOST: "localhost"
    resources:
      cpu: "500m"
      memory: "512Mi"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let backup = config.actions.get("backup").unwrap();
        assert_eq!(backup.action_type, "docker");
        assert_eq!(backup.image.as_ref().unwrap(), "company/db-backup:v2.1");
        assert_eq!(backup.command.as_ref().unwrap().len(), 3);

        let resources = backup.resources.as_ref().unwrap();
        assert_eq!(resources.cpu.as_ref().unwrap(), "500m");
        assert_eq!(resources.memory.as_ref().unwrap(), "512Mi");
    }

    #[test]
    fn test_parse_trigger() {
        let yaml = r#"
triggers:
  nightly:
    type: scheduler
    cron: "0 0 2 * * *"
    task: nightly-backup
    enabled: true
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("nightly").unwrap();
        assert_eq!(trigger.trigger_type_str(), "scheduler");
        match trigger {
            TriggerDef::Scheduler { cron, .. } => assert_eq!(cron, "0 0 2 * * *"),
            _ => panic!("Expected Scheduler variant"),
        }
        assert_eq!(trigger.task(), "nightly-backup");
        assert!(trigger.enabled());
    }

    #[test]
    fn test_parse_webhook_trigger_with_mode() {
        let yaml = r#"
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    secret: "whsec_abc"
    mode: sync
    timeout_secs: 60
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("on-push").unwrap();
        assert_eq!(trigger.trigger_type_str(), "webhook");
        assert_eq!(trigger.task(), "ci-pipeline");
        match trigger {
            TriggerDef::Webhook {
                name,
                secret,
                mode,
                timeout_secs,
                ..
            } => {
                assert_eq!(name, "github-push");
                assert_eq!(secret.as_deref(), Some("whsec_abc"));
                assert_eq!(mode.as_deref(), Some("sync"));
                assert_eq!(*timeout_secs, Some(60));
            }
            _ => panic!("Expected Webhook variant"),
        }
    }

    #[test]
    fn test_parse_webhook_trigger_mode_defaults_none() {
        let yaml = r#"
triggers:
  on-push:
    type: webhook
    name: deploy
    task: do-deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("on-push").unwrap();
        match trigger {
            TriggerDef::Webhook {
                mode, timeout_secs, ..
            } => {
                assert!(mode.is_none());
                assert!(timeout_secs.is_none());
            }
            _ => panic!("Expected Webhook variant"),
        }
    }

    #[test]
    fn test_workspace_merge() {
        let config1: WorkflowConfig = serde_yaml::from_str(
            r#"
actions:
  action1:
    type: shell
    cmd: "echo test"
tasks:
  task1:
    flow:
      step1:
        action: action1
"#,
        )
        .unwrap();

        let config2: WorkflowConfig = serde_yaml::from_str(
            r#"
actions:
  action2:
    type: shell
    cmd: "echo test2"
tasks:
  task2:
    flow:
      step1:
        action: action2
"#,
        )
        .unwrap();

        let mut workspace = WorkspaceConfig::new();
        workspace.merge(config1);
        workspace.merge(config2);

        assert_eq!(workspace.actions.len(), 2);
        assert_eq!(workspace.tasks.len(), 2);
        assert!(workspace.actions.contains_key("action1"));
        assert!(workspace.actions.contains_key("action2"));
    }

    #[test]
    fn test_default_mode() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: test
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let task = config.tasks.get("test").unwrap();
        assert_eq!(task.mode, "distributed");
    }

    #[test]
    fn test_default_enabled() {
        let yaml = r#"
triggers:
  test:
    type: scheduler
    cron: "* * * * * *"
    task: test-task
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("test").unwrap();
        assert!(trigger.enabled());
    }

    #[test]
    fn test_continue_on_failure_defaults_false() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: action1
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let task = config.tasks.get("test").unwrap();
        let step = task.flow.get("step1").unwrap();
        assert!(!step.continue_on_failure);
    }

    #[test]
    fn test_continue_on_failure_true() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: action1
        depends_on: [step0]
        continue_on_failure: true
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let task = config.tasks.get("test").unwrap();
        let step = task.flow.get("step1").unwrap();
        assert!(step.continue_on_failure);
    }

    #[test]
    fn test_depends_on() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: action1
      step2:
        action: action2
        depends_on: [step1]
      step3:
        action: action3
        depends_on: [step1, step2]
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let task = config.tasks.get("test").unwrap();

        let step1 = task.flow.get("step1").unwrap();
        assert_eq!(step1.depends_on.len(), 0);

        let step2 = task.flow.get("step2").unwrap();
        assert_eq!(step2.depends_on, vec!["step1"]);

        let step3 = task.flow.get("step3").unwrap();
        assert_eq!(step3.depends_on, vec!["step1", "step2"]);
    }

    #[test]
    fn test_parse_workflow_with_secrets() {
        let yaml = r#"
secrets:
  db_password: "ref+awsssm:///prod/db/password"
  api_key: "ref+vault://secret/data/api#key"

actions:
  backup:
    type: shell
    cmd: "pg_dump"
    env:
      DB_PASSWORD: "{{ secret.db_password }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.secrets.len(), 2);
        assert_eq!(
            config.secrets.get("db_password").unwrap(),
            "ref+awsssm:///prod/db/password"
        );
        assert_eq!(
            config.secrets.get("api_key").unwrap(),
            "ref+vault://secret/data/api#key"
        );
        assert_eq!(config.actions.len(), 1);
    }

    #[test]
    fn test_parse_workflow_with_nested_secrets() {
        let yaml = r#"
secrets:
  db:
    password: "ref+sops://secrets.enc.yaml#/db/password"
    host: "ref+sops://secrets.enc.yaml#/db/host"
  api_key: "ref+vault://secret/data/api#key"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.secrets.len(), 2);

        // db is a nested object
        let db = config.secrets.get("db").unwrap();
        assert!(db.is_object());
        assert_eq!(
            db.get("password").unwrap(),
            "ref+sops://secrets.enc.yaml#/db/password"
        );
        assert_eq!(
            db.get("host").unwrap(),
            "ref+sops://secrets.enc.yaml#/db/host"
        );

        // api_key is a flat string
        assert_eq!(
            config.secrets.get("api_key").unwrap(),
            "ref+vault://secret/data/api#key"
        );
    }

    #[test]
    fn test_parse_action_with_runner() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: docker
    cmd: "npm test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("test").unwrap();
        assert_eq!(action.runner.as_deref(), Some("docker"));
    }

    #[test]
    fn test_parse_action_with_tags() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: docker
    tags: ["node-20", "gpu"]
    cmd: "npm test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("test").unwrap();
        assert_eq!(action.tags, vec!["node-20", "gpu"]);
    }

    #[test]
    fn test_parse_action_with_entrypoint() {
        let yaml = r#"
actions:
  deploy:
    type: docker
    image: company/deploy:v3
    entrypoint: ["/app/run"]
    command: ["--env", "prod"]
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("deploy").unwrap();
        assert_eq!(
            action.entrypoint.as_ref().unwrap(),
            &vec!["/app/run".to_string()]
        );
        assert_eq!(
            action.command.as_ref().unwrap(),
            &vec!["--env".to_string(), "prod".to_string()]
        );
    }

    #[test]
    fn test_action_defaults_for_new_fields() {
        let yaml = r#"
actions:
  simple:
    type: shell
    cmd: "echo test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("simple").unwrap();
        assert!(action.runner.is_none());
        assert!(action.tags.is_empty());
        assert!(action.entrypoint.is_none());
    }

    #[test]
    fn test_parse_task_action() {
        let yaml = r#"
actions:
  run-cleanup:
    type: task
    task: cleanup-resources

  greet:
    type: shell
    cmd: "echo hello"

tasks:
  cleanup-resources:
    flow:
      step1:
        action: greet
  deploy:
    flow:
      build:
        action: greet
      cleanup:
        action: run-cleanup
        depends_on: [build]
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("run-cleanup").unwrap();
        assert_eq!(action.action_type, "task");
        assert_eq!(action.task.as_deref(), Some("cleanup-resources"));
        assert!(action.cmd.is_none());
        assert!(action.image.is_none());
    }

    #[test]
    fn test_parse_task_with_folder() {
        let yaml = r#"
tasks:
  deploy-staging:
    folder: deploy/staging
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let task = config.tasks.get("deploy-staging").unwrap();
        assert_eq!(task.folder.as_deref(), Some("deploy/staging"));
    }

    #[test]
    fn test_parse_task_without_folder() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: test
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let task = config.tasks.get("test").unwrap();
        assert!(task.folder.is_none());
    }

    #[test]
    fn test_workspace_merge_secrets() {
        let config1: WorkflowConfig = serde_yaml::from_str(
            r#"
secrets:
  db_password: "ref+awsssm:///prod/db/password"
actions:
  action1:
    type: shell
    cmd: "echo test"
"#,
        )
        .unwrap();

        let config2: WorkflowConfig = serde_yaml::from_str(
            r#"
secrets:
  api_key: "ref+vault://secret/data/api#key"
actions:
  action2:
    type: shell
    cmd: "echo test2"
"#,
        )
        .unwrap();

        let mut workspace = WorkspaceConfig::new();
        workspace.merge(config1);
        workspace.merge(config2);

        assert_eq!(workspace.secrets.len(), 2);
        assert!(workspace.secrets.contains_key("db_password"));
        assert!(workspace.secrets.contains_key("api_key"));
        assert_eq!(workspace.actions.len(), 2);
    }

    #[test]
    fn test_parse_task_with_hooks() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "make deploy"
  notify:
    type: shell
    cmd: "curl $WEBHOOK"
    input:
      message:
        type: string

tasks:
  release:
    flow:
      step1:
        action: deploy
    on_success:
      - action: notify
        input:
          message: "Deploy succeeded"
      - action: notify
        input:
          message: "All good"
    on_error:
      - action: notify
        input:
          message: "Deploy FAILED"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let task = config.tasks.get("release").unwrap();
        assert_eq!(task.on_success.len(), 2);
        assert_eq!(task.on_error.len(), 1);
        assert_eq!(task.on_success[0].action, "notify");
        assert_eq!(
            task.on_success[0].input.get("message").unwrap(),
            "Deploy succeeded"
        );
        assert_eq!(task.on_error[0].action, "notify");
        assert_eq!(
            task.on_error[0].input.get("message").unwrap(),
            "Deploy FAILED"
        );
    }

    #[test]
    fn test_parse_task_without_hooks() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: test
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let task = config.tasks.get("test").unwrap();
        assert!(task.on_success.is_empty());
        assert!(task.on_error.is_empty());
    }

    #[test]
    fn test_parse_pod_action_with_manifest() {
        let yaml = r#"
actions:
  curl-foo:
    type: pod
    image: curlimages/curl:latest
    cmd: "curl -fsSL https://example.com"
    manifest:
      metadata:
        annotations:
          iam.amazonaws.com/role: my-role
      spec:
        serviceAccountName: my-sa
        nodeSelector:
          gpu: "true"
        tolerations:
          - key: gpu
            operator: Exists
            effect: NoSchedule
        containers:
          - name: step
            resources:
              requests:
                memory: "256Mi"
                cpu: "500m"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("curl-foo").unwrap();
        assert_eq!(action.action_type, "pod");
        let manifest = action.manifest.as_ref().unwrap();
        assert!(manifest.is_object());
        assert_eq!(
            manifest["metadata"]["annotations"]["iam.amazonaws.com/role"],
            "my-role"
        );
        assert_eq!(manifest["spec"]["serviceAccountName"], "my-sa");
        assert_eq!(manifest["spec"]["nodeSelector"]["gpu"], "true");
        let tolerations = manifest["spec"]["tolerations"].as_array().unwrap();
        assert_eq!(tolerations.len(), 1);
        assert_eq!(tolerations[0]["key"], "gpu");
        let containers = manifest["spec"]["containers"].as_array().unwrap();
        assert_eq!(containers[0]["name"], "step");
        assert_eq!(containers[0]["resources"]["requests"]["memory"], "256Mi");
    }

    #[test]
    fn test_parse_action_without_manifest() {
        let yaml = r#"
actions:
  simple:
    type: pod
    image: alpine:latest
    cmd: "echo hello"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("simple").unwrap();
        assert!(action.manifest.is_none());
    }

    #[test]
    fn test_parse_workspace_level_hooks() {
        let yaml = r#"
actions:
  notify:
    type: shell
    cmd: "curl $WEBHOOK"

on_success:
  - action: notify
    input:
      message: "Job succeeded"
on_error:
  - action: notify
    input:
      message: "Job FAILED"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.on_success.len(), 1);
        assert_eq!(config.on_error.len(), 1);
        assert_eq!(config.on_success[0].action, "notify");
        assert_eq!(
            config.on_success[0].input.get("message").unwrap(),
            "Job succeeded"
        );
        assert_eq!(config.on_error[0].action, "notify");
        assert_eq!(
            config.on_error[0].input.get("message").unwrap(),
            "Job FAILED"
        );
    }

    #[test]
    fn test_parse_workflow_without_workspace_hooks() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo Hello"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.on_success.is_empty());
        assert!(config.on_error.is_empty());
    }

    #[test]
    fn test_workspace_merge_hooks() {
        let config1: WorkflowConfig = serde_yaml::from_str(
            r#"
actions:
  notify:
    type: shell
    cmd: "curl $WEBHOOK"
on_success:
  - action: notify
    input:
      message: "from config1"
"#,
        )
        .unwrap();

        let config2: WorkflowConfig = serde_yaml::from_str(
            r#"
on_success:
  - action: notify
    input:
      message: "from config2"
on_error:
  - action: notify
    input:
      message: "error from config2"
"#,
        )
        .unwrap();

        let mut workspace = WorkspaceConfig::new();
        workspace.merge(config1);
        workspace.merge(config2);

        // on_success should have 2 entries (extended, not replaced)
        assert_eq!(workspace.on_success.len(), 2);
        assert_eq!(
            workspace.on_success[0].input.get("message").unwrap(),
            "from config1"
        );
        assert_eq!(
            workspace.on_success[1].input.get("message").unwrap(),
            "from config2"
        );
        // on_error should have 1 entry
        assert_eq!(workspace.on_error.len(), 1);
    }

    #[test]
    fn test_render_secrets_plain_values_unchanged() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets
            .insert("plain".to_string(), json!("just-a-string"));
        ws.secrets.insert("number".to_string(), json!(42));
        ws.secrets.insert("flag".to_string(), json!(true));
        ws.render_secrets().unwrap();
        assert_eq!(ws.secrets["plain"], "just-a-string");
        assert_eq!(ws.secrets["number"], 42);
        assert_eq!(ws.secrets["flag"], true);
    }

    #[test]
    fn test_render_secrets_template_without_filter() {
        // A template expression with no filter just renders to the literal
        let mut ws = WorkspaceConfig::new();
        ws.secrets.insert("key".to_string(), json!("{{ 'hello' }}"));
        ws.render_secrets().unwrap();
        assert_eq!(ws.secrets["key"], "hello");
    }

    #[test]
    fn test_render_secrets_nested_values() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets
            .insert("db".to_string(), json!({"host": "localhost", "port": 5432}));
        ws.render_secrets().unwrap();
        assert_eq!(ws.secrets["db"]["host"], "localhost");
        assert_eq!(ws.secrets["db"]["port"], 5432);
    }

    #[test]
    fn test_render_secrets_skips_no_template_syntax() {
        // Strings without {{ }} are not passed through Tera at all
        let mut ws = WorkspaceConfig::new();
        ws.secrets.insert(
            "raw_ref".to_string(),
            json!("ref+awsssm:///prod/db/password"),
        );
        ws.render_secrets().unwrap();
        // No {{ }} so it passes through unchanged
        assert_eq!(ws.secrets["raw_ref"], "ref+awsssm:///prod/db/password");
    }

    #[test]
    fn test_render_secrets_error_on_bad_template() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets
            .insert("bad".to_string(), json!("{{ missing_var }}"));
        let result = ws.render_secrets();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("bad"), "Error should mention key name: {err}");
    }

    #[test]
    fn test_render_secrets_empty() {
        let mut ws = WorkspaceConfig::new();
        ws.render_secrets().unwrap();
        assert!(ws.secrets.is_empty());
    }

    #[test]
    fn test_render_secrets_nested_object_with_template() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets.insert(
            "db".to_string(),
            json!({"password": "{{ 'resolved-pw' }}", "host": "localhost"}),
        );
        ws.render_secrets().unwrap();
        assert_eq!(ws.secrets["db"]["password"], "resolved-pw");
        assert_eq!(ws.secrets["db"]["host"], "localhost");
    }

    #[test]
    fn test_render_secrets_array_values() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets.insert(
            "hosts".to_string(),
            json!(["{{ 'host-a' }}", "plain", "{{ 'host-c' }}"]),
        );
        ws.render_secrets().unwrap();
        assert_eq!(ws.secrets["hosts"][0], "host-a");
        assert_eq!(ws.secrets["hosts"][1], "plain");
        assert_eq!(ws.secrets["hosts"][2], "host-c");
    }

    #[test]
    fn test_render_secrets_deeply_nested() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets.insert(
            "cloud".to_string(),
            json!({"providers": [{"name": "aws", "key": "{{ 'resolved' }}"}]}),
        );
        ws.render_secrets().unwrap();
        assert_eq!(ws.secrets["cloud"]["providers"][0]["name"], "aws");
        assert_eq!(ws.secrets["cloud"]["providers"][0]["key"], "resolved");
    }

    #[test]
    fn test_input_field_secret_defaults_false() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
    input:
      env:
        type: string
        required: true
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("deploy").unwrap();
        let env_field = action.input.get("env").unwrap();
        assert!(!env_field.secret);
    }

    #[test]
    fn test_input_field_secret_roundtrip() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
    input:
      api_key:
        type: string
        secret: true
        default: "{{ secret.DEPLOY_KEY }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let action = config.actions.get("deploy").unwrap();
        let key_field = action.input.get("api_key").unwrap();
        assert!(key_field.secret);
        assert_eq!(
            key_field.default.as_ref().unwrap(),
            &json!("{{ secret.DEPLOY_KEY }}")
        );

        // Round-trip through JSON
        let json_str = serde_json::to_string(&key_field).unwrap();
        let deserialized: super::InputFieldDef = serde_json::from_str(&json_str).unwrap();
        assert!(deserialized.secret);
        assert_eq!(deserialized.field_type, "string");
        assert_eq!(
            deserialized.default.as_ref().unwrap(),
            &json!("{{ secret.DEPLOY_KEY }}")
        );
    }

    // --- Connection tests ---

    #[test]
    fn test_parse_connection_types() {
        let yaml = r#"
connection_types:
  postgres:
    properties:
      host:
        type: string
        required: true
      port:
        type: integer
        default: 5432
      database:
        type: string
        required: true
      user:
        type: string
        required: true
      password:
        type: string
        required: true
        secret: true
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.connection_types.len(), 1);

        let pg = config.connection_types.get("postgres").unwrap();
        assert_eq!(pg.properties.len(), 5);

        let host = pg.properties.get("host").unwrap();
        assert_eq!(host.property_type, "string");
        assert!(host.required);
        assert!(host.default.is_none());
        assert!(!host.secret);

        let port = pg.properties.get("port").unwrap();
        assert_eq!(port.property_type, "integer");
        assert!(!port.required);
        assert_eq!(port.default.as_ref().unwrap(), &json!(5432));

        let password = pg.properties.get("password").unwrap();
        assert!(password.required);
        assert!(password.secret);
    }

    #[test]
    fn test_parse_connections() {
        let yaml = r#"
connection_types:
  postgres:
    properties:
      host:
        type: string
        required: true
      port:
        type: integer
        default: 5432

connections:
  prod_db:
    type: postgres
    host: "db.example.com"
    port: 5432
    database: "myapp"
    user: "admin"
    password: "secret123"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.connections.len(), 1);

        let conn = config.connections.get("prod_db").unwrap();
        assert_eq!(conn.connection_type.as_deref(), Some("postgres"));
        assert_eq!(conn.values.get("host").unwrap(), "db.example.com");
        assert_eq!(conn.values.get("port").unwrap(), &json!(5432));
        assert_eq!(conn.values.get("database").unwrap(), "myapp");
        assert_eq!(conn.values.get("user").unwrap(), "admin");
        assert_eq!(conn.values.get("password").unwrap(), "secret123");
    }

    #[test]
    fn test_parse_untyped_connection() {
        let yaml = r#"
connections:
  custom_api:
    url: "https://api.example.com"
    token: "abc123"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let conn = config.connections.get("custom_api").unwrap();
        assert!(conn.connection_type.is_none());
        assert_eq!(conn.values.get("url").unwrap(), "https://api.example.com");
        assert_eq!(conn.values.get("token").unwrap(), "abc123");
    }

    #[test]
    fn test_workspace_merge_connections() {
        let config1: WorkflowConfig = serde_yaml::from_str(
            r#"
connection_types:
  postgres:
    properties:
      host:
        type: string
connections:
  db1:
    type: postgres
    host: "host1"
"#,
        )
        .unwrap();

        let config2: WorkflowConfig = serde_yaml::from_str(
            r#"
connection_types:
  redis:
    properties:
      url:
        type: string
connections:
  cache1:
    type: redis
    url: "redis://localhost"
"#,
        )
        .unwrap();

        let mut workspace = WorkspaceConfig::new();
        workspace.merge(config1);
        workspace.merge(config2);

        assert_eq!(workspace.connection_types.len(), 2);
        assert!(workspace.connection_types.contains_key("postgres"));
        assert!(workspace.connection_types.contains_key("redis"));
        assert_eq!(workspace.connections.len(), 2);
        assert!(workspace.connections.contains_key("db1"));
        assert!(workspace.connections.contains_key("cache1"));
    }

    #[test]
    fn test_render_connections_applies_defaults() {
        let mut ws = WorkspaceConfig::new();
        ws.connection_types.insert(
            "postgres".to_string(),
            ConnectionTypeDef {
                properties: {
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
                    props.insert(
                        "port".to_string(),
                        ConnectionPropertyDef {
                            property_type: "integer".to_string(),
                            required: false,
                            default: Some(json!(5432)),
                            secret: false,
                        },
                    );
                    props
                },
            },
        );
        ws.connections.insert(
            "prod_db".to_string(),
            ConnectionDef {
                connection_type: Some("postgres".to_string()),
                values: {
                    let mut v = HashMap::new();
                    v.insert("host".to_string(), json!("db.example.com"));
                    v
                },
            },
        );

        ws.render_connections().unwrap();

        let conn = ws.connections.get("prod_db").unwrap();
        assert_eq!(conn.values.get("host").unwrap(), "db.example.com");
        // Port should have default applied
        assert_eq!(conn.values.get("port").unwrap(), &json!(5432));
    }

    #[test]
    fn test_render_connections_user_value_takes_precedence() {
        let mut ws = WorkspaceConfig::new();
        ws.connection_types.insert(
            "postgres".to_string(),
            ConnectionTypeDef {
                properties: {
                    let mut props = HashMap::new();
                    props.insert(
                        "port".to_string(),
                        ConnectionPropertyDef {
                            property_type: "integer".to_string(),
                            required: false,
                            default: Some(json!(5432)),
                            secret: false,
                        },
                    );
                    props
                },
            },
        );
        ws.connections.insert(
            "prod_db".to_string(),
            ConnectionDef {
                connection_type: Some("postgres".to_string()),
                values: {
                    let mut v = HashMap::new();
                    v.insert("port".to_string(), json!(5433));
                    v
                },
            },
        );

        ws.render_connections().unwrap();

        let conn = ws.connections.get("prod_db").unwrap();
        assert_eq!(conn.values.get("port").unwrap(), &json!(5433));
    }

    #[test]
    fn test_render_connections_template_values() {
        let mut ws = WorkspaceConfig::new();
        ws.connections.insert(
            "api".to_string(),
            ConnectionDef {
                connection_type: None,
                values: {
                    let mut v = HashMap::new();
                    v.insert("token".to_string(), json!("{{ 'resolved-token' }}"));
                    v
                },
            },
        );

        ws.render_connections().unwrap();

        let conn = ws.connections.get("api").unwrap();
        assert_eq!(conn.values.get("token").unwrap(), "resolved-token");
    }

    #[test]
    fn test_render_connections_secret_references() {
        let mut ws = WorkspaceConfig::new();
        ws.secrets.insert(
            "clickhouse".to_string(),
            json!({"host": "ch.example.com", "password": "s3cret"}),
        );
        ws.connections.insert(
            "ch-prod".to_string(),
            ConnectionDef {
                connection_type: None,
                values: {
                    let mut v = HashMap::new();
                    v.insert("host".to_string(), json!("{{ secret.clickhouse.host }}"));
                    v.insert(
                        "password".to_string(),
                        json!("{{ secret.clickhouse.password }}"),
                    );
                    v.insert("port".to_string(), json!(8123));
                    v
                },
            },
        );

        ws.render_connections().unwrap();

        let conn = ws.connections.get("ch-prod").unwrap();
        assert_eq!(conn.values.get("host").unwrap(), "ch.example.com");
        assert_eq!(conn.values.get("password").unwrap(), "s3cret");
        assert_eq!(conn.values.get("port").unwrap(), &json!(8123));
    }

    #[test]
    fn test_render_connections_untyped_passthrough() {
        let mut ws = WorkspaceConfig::new();
        ws.connections.insert(
            "custom".to_string(),
            ConnectionDef {
                connection_type: None,
                values: {
                    let mut v = HashMap::new();
                    v.insert("url".to_string(), json!("https://example.com"));
                    v.insert("count".to_string(), json!(42));
                    v
                },
            },
        );

        ws.render_connections().unwrap();

        let conn = ws.connections.get("custom").unwrap();
        assert_eq!(conn.values.get("url").unwrap(), "https://example.com");
        assert_eq!(conn.values.get("count").unwrap(), &json!(42));
    }

    #[test]
    fn test_render_connections_empty() {
        let mut ws = WorkspaceConfig::new();
        ws.render_connections().unwrap();
        assert!(ws.connections.is_empty());
    }
}
