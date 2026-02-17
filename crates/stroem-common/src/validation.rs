use crate::dag;
use crate::models::workflow::{ActionDef, WorkflowConfig};
use anyhow::{bail, Result};

/// Validates a workflow config and returns list of warnings.
/// Errors are returned as Err.
pub fn validate_workflow_config(config: &WorkflowConfig) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    // Validate each action
    for (action_name, action) in &config.actions {
        validate_action(action_name, action)?;

        // For type: task, verify the referenced task exists
        if action.action_type == "task" {
            let task_ref = action.task.as_ref().unwrap(); // validated above
            if !config.tasks.contains_key(task_ref) {
                bail!(
                    "Action '{}' references non-existent task '{}'",
                    action_name,
                    task_ref
                );
            }
        }
    }

    // Check for direct self-references: task T uses action A where A is type:task
    // pointing back to T
    for (task_name, task) in &config.tasks {
        for (step_name, step) in &task.flow {
            if let Some(action) = config.actions.get(&step.action) {
                if action.action_type == "task" {
                    if let Some(ref task_ref) = action.task {
                        if task_ref == task_name {
                            bail!(
                                "Task '{}' step '{}' uses action '{}' which references back to the same task (self-reference)",
                                task_name,
                                step_name,
                                step.action
                            );
                        }
                    }
                }
            }
        }

        // Check hooks for self-references too
        for (i, hook) in task.on_success.iter().enumerate() {
            if let Some(action) = config.actions.get(&hook.action) {
                if action.action_type == "task" {
                    if let Some(ref task_ref) = action.task {
                        if task_ref == task_name {
                            bail!(
                                "Task '{}' on_success[{}] uses action '{}' which references back to the same task (self-reference)",
                                task_name, i, hook.action
                            );
                        }
                    }
                }
            }
        }
        for (i, hook) in task.on_error.iter().enumerate() {
            if let Some(action) = config.actions.get(&hook.action) {
                if action.action_type == "task" {
                    if let Some(ref task_ref) = action.task {
                        if task_ref == task_name {
                            bail!(
                                "Task '{}' on_error[{}] uses action '{}' which references back to the same task (self-reference)",
                                task_name, i, hook.action
                            );
                        }
                    }
                }
            }
        }
    }

    // Validate each task
    for (task_name, task) in &config.tasks {
        // Validate that flow steps reference existing actions
        for (step_name, step) in &task.flow {
            let action_ref = &step.action;

            // Check if it's a library action (contains /)
            if action_ref.contains('/') {
                // Library actions can't be validated here (would need library context)
                warnings.push(format!(
                    "Task '{}' step '{}' references library action '{}' - validation skipped",
                    task_name, step_name, action_ref
                ));
            } else {
                // Local action - must exist in this config
                if !config.actions.contains_key(action_ref) {
                    bail!(
                        "Task '{}' step '{}' references non-existent action '{}'",
                        task_name,
                        step_name,
                        action_ref
                    );
                }
            }

            // Validate depends_on references
            for dep in &step.depends_on {
                if !task.flow.contains_key(dep) {
                    bail!(
                        "Task '{}' step '{}' depends on non-existent step '{}'",
                        task_name,
                        step_name,
                        dep
                    );
                }
            }
        }

        // Validate DAG (no cycles)
        dag::validate_dag(&task.flow)
            .map_err(|e| anyhow::anyhow!("Task '{}' has invalid DAG: {}", task_name, e))?;

        // Validate hooks reference existing actions
        for (i, hook) in task.on_success.iter().enumerate() {
            validate_hook_action(task_name, "on_success", i, &hook.action, config)?;
        }
        for (i, hook) in task.on_error.iter().enumerate() {
            validate_hook_action(task_name, "on_error", i, &hook.action, config)?;
        }
    }

    // Validate triggers reference existing tasks
    for (trigger_name, trigger) in &config.triggers {
        if !config.tasks.contains_key(&trigger.task) {
            bail!(
                "Trigger '{}' references non-existent task '{}'",
                trigger_name,
                trigger.task
            );
        }

        // Validate scheduler triggers have cron
        if trigger.trigger_type == "scheduler" && trigger.cron.is_none() {
            bail!(
                "Trigger '{}' is type 'scheduler' but missing 'cron' field",
                trigger_name
            );
        }

        // Validate cron expression syntax
        if let Some(cron_expr) = &trigger.cron {
            if croner::parser::CronParser::builder()
                .seconds(croner::parser::Seconds::Optional)
                .build()
                .parse(cron_expr)
                .is_err()
            {
                bail!(
                    "Trigger '{}' has invalid cron expression '{}'",
                    trigger_name,
                    cron_expr
                );
            }
        }
    }

    // Validate secrets
    validate_secrets(
        "",
        &serde_json::Value::Object(
            config
                .secrets
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
    )?;

    // Check for secret references in action env that don't exist in secrets map
    for (action_name, action) in &config.actions {
        if let Some(env) = &action.env {
            for (env_key, env_val) in env {
                // Look for {{ secret.xxx.yyy }} patterns
                for ref_path in extract_secret_refs(env_val) {
                    if !resolve_secret_path(&config.secrets, &ref_path) {
                        warnings.push(format!(
                            "Action '{}' env '{}' references secret '{}' which is not defined in secrets",
                            action_name, env_key, ref_path
                        ));
                    }
                }
            }
        }
    }

    Ok(warnings)
}

/// Validates secret definitions recursively.
/// Keys must be alphanumeric + underscore. Leaf string values must be non-empty.
fn validate_secrets(path: &str, value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Object(map) => {
            for (name, val) in map {
                if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    bail!(
                        "Secret name '{}' contains invalid characters (only alphanumeric and underscore allowed)",
                        if path.is_empty() { name.clone() } else { format!("{}.{}", path, name) }
                    );
                }
                let child_path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{}.{}", path, name)
                };
                validate_secrets(&child_path, val)?;
            }
        }
        serde_json::Value::String(s) => {
            if s.is_empty() {
                bail!("Secret '{}' has an empty value", path);
            }
        }
        _ => {
            // Allow other JSON value types (numbers, booleans) as leaf values
        }
    }
    Ok(())
}

/// Extract secret reference paths from {{ secret.xxx.yyy }} patterns in a string
fn extract_secret_refs(s: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut remaining = s;
    while let Some(start) = remaining.find("{{ secret.") {
        let after = &remaining[start + 10..]; // skip "{{ secret."
        if let Some(end) = after.find(" }}") {
            let name = after[..end].trim().to_string();
            if !name.is_empty() {
                refs.push(name);
            }
            remaining = &after[end + 3..];
        } else {
            break;
        }
    }
    refs
}

/// Check if a dot-separated path resolves to a value in the secrets map
fn resolve_secret_path(
    secrets: &std::collections::HashMap<String, serde_json::Value>,
    path: &str,
) -> bool {
    let parts: Vec<&str> = path.splitn(2, '.').collect();
    match secrets.get(parts[0]) {
        Some(value) => {
            if parts.len() == 1 {
                // Leaf lookup — the key exists
                true
            } else {
                // Nested lookup — walk into the value
                resolve_json_path(value, parts[1])
            }
        }
        None => false,
    }
}

/// Walk a JSON value by dot-separated path
fn resolve_json_path(value: &serde_json::Value, path: &str) -> bool {
    let parts: Vec<&str> = path.splitn(2, '.').collect();
    match value.get(parts[0]) {
        Some(child) => {
            if parts.len() == 1 {
                true
            } else {
                resolve_json_path(child, parts[1])
            }
        }
        None => false,
    }
}

/// Validates a single action definition
fn validate_action(action_name: &str, action: &ActionDef) -> Result<()> {
    match action.action_type.as_str() {
        "shell" => {
            // Shell actions must have either cmd or script
            if action.cmd.is_none() && action.script.is_none() {
                bail!(
                    "Action '{}' is type 'shell' but has neither 'cmd' nor 'script'",
                    action_name
                );
            }

            // Shell actions should not have both cmd and script
            if action.cmd.is_some() && action.script.is_some() {
                bail!(
                    "Action '{}' is type 'shell' but has both 'cmd' and 'script' - use only one",
                    action_name
                );
            }

            // Shell actions must not have 'image' — use type: docker (Type 1) or runner: docker (Type 2) instead
            if action.image.is_some() {
                bail!(
                    "Action '{}' is type 'shell' but has 'image' field. Use type: docker for container images, or runner: docker for shell-in-container",
                    action_name
                );
            }

            // Validate runner if present
            if let Some(ref runner) = action.runner {
                match runner.as_str() {
                    "local" | "docker" | "pod" => {}
                    other => bail!(
                        "Action '{}' has invalid runner '{}' (expected: local, docker, pod)",
                        action_name,
                        other
                    ),
                }
            }

            // Shell actions must not have entrypoint
            if action.entrypoint.is_some() {
                bail!(
                    "Action '{}' is type 'shell' but has 'entrypoint' field (only valid for docker/pod)",
                    action_name
                );
            }
        }
        "docker" => {
            // Docker actions must have image
            if action.image.is_none() {
                bail!(
                    "Action '{}' is type 'docker' but missing 'image' field",
                    action_name
                );
            }

            // Docker actions must not have runner
            if action.runner.is_some() {
                bail!(
                    "Action '{}' is type 'docker' but has 'runner' field (runner is only for shell actions)",
                    action_name
                );
            }

            // Docker actions must not have script
            if action.script.is_some() {
                bail!(
                    "Action '{}' is type 'docker' but has 'script' field (use cmd or entrypoint instead)",
                    action_name
                );
            }
        }
        "pod" => {
            // Pod actions must have image
            if action.image.is_none() {
                bail!(
                    "Action '{}' is type 'pod' but missing 'image' field",
                    action_name
                );
            }

            // Pod actions must not have runner
            if action.runner.is_some() {
                bail!(
                    "Action '{}' is type 'pod' but has 'runner' field (runner is only for shell actions)",
                    action_name
                );
            }

            // Pod actions must not have script
            if action.script.is_some() {
                bail!(
                    "Action '{}' is type 'pod' but has 'script' field (use cmd or entrypoint instead)",
                    action_name
                );
            }
        }
        "task" => {
            // Task actions must have task field
            if action.task.is_none() {
                bail!(
                    "Action '{}' is type 'task' but missing 'task' field",
                    action_name
                );
            }

            // Task actions must not have cmd, script, or image
            if action.cmd.is_some() {
                bail!(
                    "Action '{}' is type 'task' but has 'cmd' field (task actions only reference another task)",
                    action_name
                );
            }
            if action.script.is_some() {
                bail!(
                    "Action '{}' is type 'task' but has 'script' field (task actions only reference another task)",
                    action_name
                );
            }
            if action.image.is_some() {
                bail!(
                    "Action '{}' is type 'task' but has 'image' field (task actions only reference another task)",
                    action_name
                );
            }
            if action.runner.is_some() {
                bail!(
                    "Action '{}' is type 'task' but has 'runner' field (task actions only reference another task)",
                    action_name
                );
            }
        }
        other => {
            bail!(
                "Action '{}' has invalid type '{}' (expected: shell, docker, pod, task)",
                action_name,
                other
            );
        }
    }

    Ok(())
}

/// Validates that a hook references an existing action (or a library action).
fn validate_hook_action(
    task_name: &str,
    hook_type: &str,
    idx: usize,
    action: &str,
    config: &WorkflowConfig,
) -> Result<()> {
    if action.contains('/') {
        return Ok(()); // library action — skip
    }
    if !config.actions.contains_key(action) {
        bail!(
            "Task '{}' {}[{}] references non-existent action '{}'",
            task_name,
            hook_type,
            idx,
            action
        );
    }
    Ok(())
}

/// Compute the required worker tags for an action.
/// These tags must all be present on a worker for it to claim a step using this action.
pub fn compute_required_tags(action: &ActionDef) -> Vec<String> {
    // Task actions are handled server-side, not by workers
    if action.action_type == "task" {
        return vec![];
    }

    let base_tag = match action.action_type.as_str() {
        "shell" => match action.runner.as_deref() {
            Some("docker") => "docker",
            Some("pod") => "kubernetes",
            _ => "shell",
        },
        "docker" => "docker",
        "pod" => "kubernetes",
        _ => "shell",
    };

    let mut tags = vec![base_tag.to_string()];
    for tag in &action.tags {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    tags
}

/// Derive the runner mode string for an action.
/// Returns "local", "docker", "pod", or "none".
pub fn derive_runner(action: &ActionDef) -> String {
    match action.action_type.as_str() {
        "shell" => action.runner.as_deref().unwrap_or("local").to_string(),
        "docker" | "pod" => "none".to_string(),
        "task" => "none".to_string(),
        _ => "local".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::workflow::{TaskDef, TriggerDef};
    use std::collections::HashMap;

    #[test]
    fn test_validate_valid_workflow() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo Hello"
  build:
    type: docker
    image: node:20

tasks:
  test:
    flow:
      step1:
        action: greet
      step2:
        action: build
        depends_on: [step1]

triggers:
  nightly:
    type: scheduler
    cron: "0 0 * * * *"
    task: test
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_action_shell_missing_cmd_and_script() {
        let yaml = r#"
actions:
  bad:
    type: shell
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("neither 'cmd' nor 'script'"));
    }

    #[test]
    fn test_validate_action_shell_both_cmd_and_script() {
        let yaml = r#"
actions:
  bad:
    type: shell
    cmd: "echo test"
    script: "test.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("both 'cmd' and 'script'"));
    }

    #[test]
    fn test_validate_action_docker_missing_image() {
        let yaml = r#"
actions:
  bad:
    type: docker
    command: ["echo", "test"]
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'image'"));
    }

    #[test]
    fn test_validate_action_pod_missing_image() {
        let yaml = r#"
actions:
  bad:
    type: pod
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'image'"));
    }

    #[test]
    fn test_validate_action_invalid_type() {
        let yaml = r#"
actions:
  bad:
    type: invalid
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid type"));
    }

    #[test]
    fn test_validate_task_missing_action() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo test"

tasks:
  test:
    flow:
      step1:
        action: nonexistent
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("non-existent action"));
    }

    #[test]
    fn test_validate_task_missing_dependency() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo test"

tasks:
  test:
    flow:
      step1:
        action: greet
        depends_on: [nonexistent]
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("non-existent step"));
    }

    #[test]
    fn test_validate_task_cycle() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo test"

tasks:
  test:
    flow:
      step1:
        action: greet
        depends_on: [step2]
      step2:
        action: greet
        depends_on: [step1]
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cycle detected"));
    }

    #[test]
    fn test_validate_trigger_missing_task() {
        let yaml = r#"
triggers:
  nightly:
    type: scheduler
    cron: "0 0 * * * *"
    task: nonexistent
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("non-existent task"));
    }

    #[test]
    fn test_validate_trigger_invalid_cron() {
        let mut config = WorkflowConfig::default();
        config.tasks.insert(
            "test".to_string(),
            TaskDef {
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow: HashMap::new(),
                on_success: vec![],
                on_error: vec![],
            },
        );
        config.triggers.insert(
            "bad-cron".to_string(),
            TriggerDef {
                trigger_type: "scheduler".to_string(),
                cron: Some("not a cron".to_string()),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
            },
        );

        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid cron expression"));
    }

    #[test]
    fn test_validate_trigger_valid_cron_6field() {
        let mut config = WorkflowConfig::default();
        config.tasks.insert(
            "test".to_string(),
            TaskDef {
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow: HashMap::new(),
                on_success: vec![],
                on_error: vec![],
            },
        );
        config.triggers.insert(
            "every-10s".to_string(),
            TriggerDef {
                trigger_type: "scheduler".to_string(),
                cron: Some("*/10 * * * * *".to_string()),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
            },
        );

        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_trigger_scheduler_missing_cron() {
        let mut config = WorkflowConfig::default();
        config.tasks.insert(
            "test".to_string(),
            TaskDef {
                mode: "distributed".to_string(),
                folder: None,
                input: HashMap::new(),
                flow: HashMap::new(),
                on_success: vec![],
                on_error: vec![],
            },
        );
        config.triggers.insert(
            "bad".to_string(),
            TriggerDef {
                trigger_type: "scheduler".to_string(),
                cron: None,
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
            },
        );

        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'cron'"));
    }

    #[test]
    fn test_validate_library_action_warning() {
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: common/slack-notify
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("library action"));
        assert!(warnings[0].contains("common/slack-notify"));
    }

    #[test]
    fn test_validate_shell_with_script() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    script: actions/deploy.sh
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_shell_with_image_rejected() {
        let yaml = r#"
actions:
  build:
    type: shell
    image: node:20
    cmd: "npm run build"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("has 'image' field"));
    }

    #[test]
    fn test_validate_complex_workflow() {
        let yaml = r#"
actions:
  checkout:
    type: shell
    cmd: "git clone repo"
  build:
    type: shell
    runner: docker
    cmd: "npm run build"
  deploy:
    type: docker
    image: kubectl:latest
    command: ["apply", "-f", "deployment.yaml"]

tasks:
  ci-pipeline:
    flow:
      checkout:
        action: checkout
      build:
        action: build
        depends_on: [checkout]
      deploy:
        action: deploy
        depends_on: [build]

triggers:
  on-push:
    type: webhook
    task: ci-pipeline
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_secrets_valid() {
        let yaml = r#"
secrets:
  db_password: "ref+awsssm:///prod/db/password"
  api_key_2: "ref+vault://secret/data/api#key"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_secrets_nested_valid() {
        let yaml = r#"
secrets:
  db:
    password: "ref+sops://secrets.enc.yaml#/db/password"
    host: "ref+sops://secrets.enc.yaml#/db/host"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_secrets_invalid_name() {
        let yaml = r#"
secrets:
  "db-password": "ref+awsssm:///prod/db/password"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid characters"));
    }

    #[test]
    fn test_validate_secrets_nested_invalid_name() {
        let yaml = r#"
secrets:
  db:
    "my-password": "ref+sops://x"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid characters"));
    }

    #[test]
    fn test_validate_secrets_empty_value() {
        let yaml = r#"
secrets:
  db_password: ""
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty value"));
    }

    #[test]
    fn test_validate_secrets_nested_empty_value() {
        let yaml = r#"
secrets:
  db:
    password: ""
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty value"));
    }

    #[test]
    fn test_validate_secret_reference_warning() {
        let yaml = r#"
secrets:
  db_password: "ref+awsssm:///prod/db/password"

actions:
  backup:
    type: shell
    cmd: "pg_dump"
    env:
      DB_PASSWORD: "{{ secret.db_password }}"
      API_KEY: "{{ secret.missing_key }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        // Should warn about missing_key but not db_password
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("missing_key"));
        assert!(warnings[0].contains("not defined in secrets"));
    }

    #[test]
    fn test_validate_secret_nested_reference_warning() {
        let yaml = r#"
secrets:
  db:
    password: "ref+sops://secrets.enc.yaml#/db/password"

actions:
  backup:
    type: shell
    cmd: "pg_dump"
    env:
      DB_PASSWORD: "{{ secret.db.password }}"
      DB_HOST: "{{ secret.db.host }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        // Should warn about db.host but not db.password
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("db.host"));
        assert!(warnings[0].contains("not defined in secrets"));
    }

    #[test]
    fn test_validate_shell_with_runner() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: docker
    cmd: "npm test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_shell_with_invalid_runner() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: invalid
    cmd: "npm test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid runner"));
    }

    #[test]
    fn test_validate_shell_with_entrypoint_rejected() {
        let yaml = r#"
actions:
  test:
    type: shell
    cmd: "echo hi"
    entrypoint: ["/bin/sh"]
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("entrypoint"));
    }

    #[test]
    fn test_validate_docker_with_runner_rejected() {
        let yaml = r#"
actions:
  test:
    type: docker
    image: alpine:latest
    runner: local
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("runner"));
    }

    #[test]
    fn test_validate_docker_with_script_rejected() {
        let yaml = r#"
actions:
  test:
    type: docker
    image: alpine:latest
    script: "deploy.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("script"));
    }

    #[test]
    fn test_validate_docker_no_cmd_ok() {
        // Type 1: docker with image only (uses image default entrypoint)
        let yaml = r#"
actions:
  migrate:
    type: docker
    image: company/db-migrations:v3
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_docker_with_entrypoint() {
        let yaml = r#"
actions:
  test:
    type: docker
    image: alpine:latest
    entrypoint: ["/bin/sh", "-c"]
    cmd: "echo hello"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_shell_with_tags() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: docker
    tags: ["node-20"]
    cmd: "npm test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_compute_required_tags_shell_local() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: shell
cmd: "echo test"
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["shell"]);
    }

    #[test]
    fn test_compute_required_tags_shell_docker() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: shell
runner: docker
cmd: "npm test"
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["docker"]);
    }

    #[test]
    fn test_compute_required_tags_shell_docker_with_tags() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: shell
runner: docker
tags: ["node-20"]
cmd: "npm test"
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["docker", "node-20"]);
    }

    #[test]
    fn test_compute_required_tags_docker() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: docker
image: alpine:latest
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["docker"]);
    }

    #[test]
    fn test_compute_required_tags_pod() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: pod
image: alpine:latest
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["kubernetes"]);
    }

    #[test]
    fn test_compute_required_tags_pod_with_tags() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: pod
image: alpine:latest
tags: ["gpu"]
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["kubernetes", "gpu"]);
    }

    #[test]
    fn test_compute_required_tags_shell_pod() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: shell
runner: pod
cmd: "npm test"
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["kubernetes"]);
    }

    #[test]
    fn test_compute_required_tags_dedup_base_tag() {
        use crate::models::workflow::ActionDef;
        // If tags already include the base tag, it should not be duplicated
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: docker
image: alpine:latest
tags: ["docker", "gpu"]
"#,
        )
        .unwrap();
        let tags = compute_required_tags(&action);
        assert_eq!(tags, vec!["docker", "gpu"]);
        // Ensure "docker" appears only once
        assert_eq!(tags.iter().filter(|t| *t == "docker").count(), 1);
    }

    #[test]
    fn test_validate_pod_with_runner_rejected() {
        let yaml = r#"
actions:
  test:
    type: pod
    image: alpine:latest
    runner: local
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("runner"));
    }

    #[test]
    fn test_validate_pod_with_script_rejected() {
        let yaml = r#"
actions:
  test:
    type: pod
    image: alpine:latest
    script: "deploy.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("script"));
    }

    #[test]
    fn test_validate_shell_runner_pod() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: pod
    cmd: "npm test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_shell_runner_local() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: local
    cmd: "echo test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_derive_runner_shell_default() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: shell
cmd: "echo test"
"#,
        )
        .unwrap();
        assert_eq!(derive_runner(&action), "local");
    }

    #[test]
    fn test_derive_runner_shell_docker() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: shell
runner: docker
cmd: "npm test"
"#,
        )
        .unwrap();
        assert_eq!(derive_runner(&action), "docker");
    }

    #[test]
    fn test_derive_runner_shell_pod() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: shell
runner: pod
cmd: "npm test"
"#,
        )
        .unwrap();
        assert_eq!(derive_runner(&action), "pod");
    }

    #[test]
    fn test_derive_runner_docker() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: docker
image: alpine:latest
"#,
        )
        .unwrap();
        assert_eq!(derive_runner(&action), "none");
    }

    #[test]
    fn test_derive_runner_pod() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: pod
image: alpine:latest
"#,
        )
        .unwrap();
        assert_eq!(derive_runner(&action), "none");
    }

    #[test]
    fn test_validate_hook_valid_action() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "make deploy"
  notify:
    type: shell
    cmd: "curl $WEBHOOK"

tasks:
  release:
    flow:
      step1:
        action: deploy
    on_success:
      - action: notify
        input:
          message: "Deploy succeeded"
    on_error:
      - action: notify
        input:
          message: "Deploy FAILED"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_hook_missing_action() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "make deploy"

tasks:
  release:
    flow:
      step1:
        action: deploy
    on_success:
      - action: nonexistent
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("on_success[0]"));
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn test_validate_hook_library_action() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "make deploy"

tasks:
  release:
    flow:
      step1:
        action: deploy
    on_success:
      - action: common/slack-notify
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_task_action_valid() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
  run-cleanup:
    type: task
    task: cleanup

tasks:
  cleanup:
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
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_task_action_missing_task_field() {
        let yaml = r#"
actions:
  bad:
    type: task
tasks:
  t:
    flow:
      s:
        action: bad
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'task'"));
    }

    #[test]
    fn test_validate_task_action_with_cmd_rejected() {
        let yaml = r#"
actions:
  bad:
    type: task
    task: some-task
    cmd: "echo nope"
tasks:
  some-task:
    flow:
      s:
        action: bad
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'cmd'"));
    }

    #[test]
    fn test_validate_task_action_with_image_rejected() {
        let yaml = r#"
actions:
  bad:
    type: task
    task: some-task
    image: alpine:latest
tasks:
  some-task:
    flow:
      s:
        action: bad
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'image'"));
    }

    #[test]
    fn test_validate_task_action_nonexistent_task() {
        let yaml = r#"
actions:
  run-missing:
    type: task
    task: nonexistent
tasks:
  deploy:
    flow:
      s:
        action: run-missing
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("non-existent task"));
    }

    #[test]
    fn test_validate_task_action_self_reference() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
  run-self:
    type: task
    task: loopy
tasks:
  loopy:
    flow:
      step1:
        action: greet
      step2:
        action: run-self
        depends_on: [step1]
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("self-reference"));
    }

    #[test]
    fn test_validate_task_action_self_reference_in_hook() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
  run-self:
    type: task
    task: loopy
tasks:
  loopy:
    flow:
      step1:
        action: greet
    on_error:
      - action: run-self
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("self-reference"));
    }

    #[test]
    fn test_compute_required_tags_task() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: task
task: other-task
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), Vec::<String>::new());
    }

    #[test]
    fn test_derive_runner_task() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: task
task: other-task
"#,
        )
        .unwrap();
        assert_eq!(derive_runner(&action), "none");
    }
}
