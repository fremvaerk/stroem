use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Input field definition for actions and tasks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputFieldDef {
    #[serde(rename = "type")]
    pub field_type: String, // "string", "number", "boolean"
    #[serde(default)]
    pub required: bool,
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
pub struct TriggerDef {
    #[serde(rename = "type")]
    pub trigger_type: String, // "scheduler", "webhook"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    pub task: String,
    #[serde(default)]
    pub input: HashMap<String, serde_json::Value>,
    #[serde(default = "default_true")]
    pub enabled: bool,
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
    pub actions: HashMap<String, ActionDef>,
    #[serde(default)]
    pub tasks: HashMap<String, TaskDef>,
    #[serde(default)]
    pub triggers: HashMap<String, TriggerDef>,
}

/// Merged workspace - all YAML files combined
#[derive(Debug, Clone, Default)]
pub struct WorkspaceConfig {
    pub secrets: HashMap<String, serde_json::Value>,
    pub actions: HashMap<String, ActionDef>,
    pub tasks: HashMap<String, TaskDef>,
    pub triggers: HashMap<String, TriggerDef>,
}

impl WorkspaceConfig {
    /// Create a new empty workspace config
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge another workflow config into this workspace
    pub fn merge(&mut self, config: WorkflowConfig) {
        self.secrets.extend(config.secrets);
        self.actions.extend(config.actions);
        self.tasks.extend(config.tasks);
        self.triggers.extend(config.triggers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("nightly").unwrap();
        assert_eq!(trigger.trigger_type, "scheduler");
        assert_eq!(trigger.cron.as_ref().unwrap(), "0 0 2 * * *");
        assert_eq!(trigger.task, "nightly-backup");
        assert!(trigger.enabled);
    }

    #[test]
    fn test_workspace_merge() {
        let config1: WorkflowConfig = serde_yml::from_str(
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

        let config2: WorkflowConfig = serde_yml::from_str(
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("test").unwrap();
        assert!(trigger.enabled);
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let task = config.tasks.get("test").unwrap();
        assert!(task.folder.is_none());
    }

    #[test]
    fn test_workspace_merge_secrets() {
        let config1: WorkflowConfig = serde_yml::from_str(
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

        let config2: WorkflowConfig = serde_yml::from_str(
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let task = config.tasks.get("test").unwrap();
        assert!(task.on_success.is_empty());
        assert!(task.on_error.is_empty());
    }
}
