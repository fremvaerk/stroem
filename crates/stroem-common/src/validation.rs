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
        }
        "docker" => {
            // Docker actions must have image
            if action.image.is_none() {
                bail!(
                    "Action '{}' is type 'docker' but missing 'image' field",
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
        }
        other => {
            bail!(
                "Action '{}' has invalid type '{}' (expected: shell, docker, pod)",
                action_name,
                other
            );
        }
    }

    Ok(())
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("non-existent task"));
    }

    #[test]
    fn test_validate_trigger_scheduler_missing_cron() {
        let mut config = WorkflowConfig::default();
        config.tasks.insert(
            "test".to_string(),
            TaskDef {
                mode: "distributed".to_string(),
                input: HashMap::new(),
                flow: HashMap::new(),
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_shell_with_image() {
        let yaml = r#"
actions:
  build:
    type: shell
    image: node:20
    cmd: "npm run build"
"#;
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
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
    image: node:20
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_secrets_invalid_name() {
        let yaml = r#"
secrets:
  "db-password": "ref+awsssm:///prod/db/password"
"#;
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
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
        let config: WorkflowConfig = serde_yml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        // Should warn about db.host but not db.password
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("db.host"));
        assert!(warnings[0].contains("not defined in secrets"));
    }
}
