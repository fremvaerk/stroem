use crate::dag;
use crate::models::workflow::{ActionDef, WorkflowConfig};
use anyhow::{bail, Result};

/// Validates a workflow config and returns list of warnings.
/// Errors are returned as Err.
pub fn validate_workflow_config(config: &WorkflowConfig) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    // Validate each action
    for (action_name, action) in &config.actions {
        warnings.extend(validate_action(action_name, action)?);

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
            check_task_self_reference(
                config,
                task_name,
                &step.action,
                &format!("step '{step_name}'"),
            )?;
        }

        // Check hooks for self-references too
        for (i, hook) in task.on_success.iter().enumerate() {
            check_task_self_reference(
                config,
                task_name,
                &hook.action,
                &format!("on_success[{i}]"),
            )?;
        }
        for (i, hook) in task.on_error.iter().enumerate() {
            check_task_self_reference(config, task_name, &hook.action, &format!("on_error[{i}]"))?;
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
            validate_hook_action_exists(
                &format!("Task '{task_name}' on_success[{i}]"),
                &hook.action,
                config,
            )?;
        }
        for (i, hook) in task.on_error.iter().enumerate() {
            validate_hook_action_exists(
                &format!("Task '{task_name}' on_error[{i}]"),
                &hook.action,
                config,
            )?;
        }
    }

    // Validate triggers reference existing tasks
    for (trigger_name, trigger) in &config.triggers {
        if !config.tasks.contains_key(trigger.task()) {
            bail!(
                "Trigger '{}' references non-existent task '{}'",
                trigger_name,
                trigger.task()
            );
        }

        match trigger {
            crate::models::workflow::TriggerDef::Scheduler { cron, .. } => {
                // Validate cron expression syntax
                if croner::parser::CronParser::builder()
                    .seconds(croner::parser::Seconds::Optional)
                    .build()
                    .parse(cron)
                    .is_err()
                {
                    bail!(
                        "Trigger '{}' has invalid cron expression '{}'",
                        trigger_name,
                        cron
                    );
                }
            }
            crate::models::workflow::TriggerDef::Webhook {
                name,
                mode,
                timeout_secs,
                ..
            } => {
                // Validate webhook name is URL-safe
                if !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    bail!(
                        "Trigger '{}' has invalid webhook name '{}' (only alphanumeric, hyphens, and underscores allowed)",
                        trigger_name,
                        name
                    );
                }
                if name.is_empty() {
                    bail!("Trigger '{}' has empty webhook name", trigger_name);
                }
                // Validate mode
                if let Some(m) = mode {
                    if m != "sync" && m != "async" {
                        bail!(
                            "Trigger '{}' has invalid mode '{}' (must be 'sync' or 'async')",
                            trigger_name,
                            m
                        );
                    }
                }
                // Validate timeout_secs
                if let Some(t) = timeout_secs {
                    if *t == 0 || *t > 300 {
                        bail!(
                            "Trigger '{}' has invalid timeout_secs {} (must be 1..=300)",
                            trigger_name,
                            t
                        );
                    }
                }
            }
        }
    }

    // Validate workspace-level hooks reference existing actions
    for (i, hook) in config.on_success.iter().enumerate() {
        validate_hook_action_exists(&format!("Workspace on_success[{i}]"), &hook.action, config)?;
    }
    for (i, hook) in config.on_error.iter().enumerate() {
        validate_hook_action_exists(&format!("Workspace on_error[{i}]"), &hook.action, config)?;
    }

    // Validate connections
    warnings.extend(validate_connections(config)?);

    // Validate connection input references in tasks
    warnings.extend(validate_connection_inputs(config)?);

    // Validate input field options
    warnings.extend(validate_input_options(config));

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

/// Validates connection types and connections.
///
/// - Property types must be `string`, `integer`, `number`, or `boolean`.
/// - Typed connections must reference an existing connection_type.
/// - Required fields without defaults must be present.
/// - Unknown fields produce warnings.
/// - Empty string values produce errors.
fn validate_connections(config: &WorkflowConfig) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let valid_prop_types = [
        "string", "text", "integer", "number", "boolean", "date", "datetime",
    ];

    // Validate connection type definitions
    for (type_name, type_def) in &config.connection_types {
        for (prop_name, prop_def) in &type_def.properties {
            if !valid_prop_types.contains(&prop_def.property_type.as_str()) {
                bail!(
                    "Connection type '{}' property '{}' has invalid type '{}' (expected: string, text, integer, number, boolean, date, datetime)",
                    type_name,
                    prop_name,
                    prop_def.property_type
                );
            }
        }
    }

    // Validate connection instances
    for (conn_name, conn) in &config.connections {
        if let Some(ref type_name) = conn.connection_type {
            if let Some(type_def) = config.connection_types.get(type_name) {
                // Check required fields without defaults are present
                for (prop_name, prop_def) in &type_def.properties {
                    if prop_def.required
                        && prop_def.default.is_none()
                        && !conn.values.contains_key(prop_name)
                    {
                        bail!(
                            "Connection '{}' is missing required field '{}' (type '{}')",
                            conn_name,
                            prop_name,
                            type_name
                        );
                    }
                }

                // Warn about unknown fields
                for key in conn.values.keys() {
                    if !type_def.properties.contains_key(key) {
                        warnings.push(format!(
                            "Connection '{}' has field '{}' not defined in type '{}'",
                            conn_name, key, type_name
                        ));
                    }
                }
            } else {
                bail!(
                    "Connection '{}' references non-existent connection type '{}'",
                    conn_name,
                    type_name
                );
            }
        }

        // Check for empty string values
        for (key, value) in &conn.values {
            if let Some(s) = value.as_str() {
                if s.is_empty() {
                    bail!(
                        "Connection '{}' field '{}' has an empty value",
                        conn_name,
                        key
                    );
                }
            }
        }
    }

    Ok(warnings)
}

/// Validates input field options across all actions and tasks.
fn validate_input_options(config: &WorkflowConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    // Check action-level inputs
    for (action_name, action) in &config.actions {
        for (field_name, field) in &action.input {
            check_input_field_options(
                &format!("Action '{action_name}' input '{field_name}'"),
                field,
                &mut warnings,
            );
        }
    }

    // Check task-level inputs
    for (task_name, task) in &config.tasks {
        for (field_name, field) in &task.input {
            check_input_field_options(
                &format!("Task '{task_name}' input '{field_name}'"),
                field,
                &mut warnings,
            );
        }
    }

    warnings
}

fn check_input_field_options(
    context: &str,
    field: &crate::models::workflow::InputFieldDef,
    warnings: &mut Vec<String>,
) {
    if let Some(ref options) = field.options {
        if options.is_empty() {
            warnings.push(format!("{context}: options list is empty"));
        }
        if options.iter().any(|o| o.is_empty()) {
            warnings.push(format!("{context}: options list contains empty string"));
        }
        let mut seen = std::collections::HashSet::new();
        for opt in options {
            if !seen.insert(opt) {
                warnings.push(format!("{context}: duplicate option '{opt}'"));
            }
        }
        // Warn if default value is not in options (strict mode only)
        if !field.allow_custom {
            if let Some(ref default) = field.default {
                if let Some(default_str) = default.as_str() {
                    if !options.iter().any(|o| o == default_str) {
                        warnings.push(format!(
                            "{context}: default value '{}' is not in options list",
                            default_str
                        ));
                    }
                }
            }
        }
    } else if field.allow_custom {
        warnings.push(format!(
            "{context}: allow_custom has no effect without options"
        ));
    }
}

/// Validates that task input fields referencing connection types have valid references.
///
/// For each task input where `field_type` is not a primitive type (string/integer/number/boolean),
/// it's treated as a connection type reference and must exist in `connection_types`.
fn validate_connection_inputs(config: &WorkflowConfig) -> Result<Vec<String>> {
    let warnings = Vec::new();
    let primitives = [
        "string", "text", "integer", "number", "boolean", "date", "datetime",
    ];

    for (task_name, task) in &config.tasks {
        for (field_name, field_def) in &task.input {
            if !primitives.contains(&field_def.field_type.as_str()) {
                // Non-primitive type — must be a connection type reference
                if !config.connection_types.contains_key(&field_def.field_type) {
                    bail!(
                        "Task '{}' input '{}' references unknown type '{}' (not a primitive or connection type)",
                        task_name,
                        field_name,
                        field_def.field_type
                    );
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

/// Checks whether `action_name` is a task action that references back to `task_name` (self-reference).
/// `context` describes the location being checked, e.g. "step 'build'" or "on_success[0]".
fn check_task_self_reference(
    config: &WorkflowConfig,
    task_name: &str,
    action_name: &str,
    context: &str,
) -> Result<()> {
    if let Some(action) = config.actions.get(action_name) {
        if action.action_type == "task" {
            if let Some(ref task_ref) = action.task {
                if task_ref == task_name {
                    bail!(
                        "Task '{}' {} uses action '{}' which references back to the same task (self-reference)",
                        task_name,
                        context,
                        action_name
                    );
                }
            }
        }
    }
    Ok(())
}

/// Validates a single action definition. Dispatches to a per-type helper.
fn validate_action(action_name: &str, action: &ActionDef) -> Result<Vec<String>> {
    match action.action_type.as_str() {
        "shell" => validate_shell_action(action, action_name),
        "docker" => validate_docker_action(action, action_name),
        "pod" => validate_pod_action(action, action_name),
        "task" => validate_task_action(action, action_name),
        other => bail!(
            "Action '{}' has invalid type '{}' (expected: shell, docker, pod, task)",
            action_name,
            other
        ),
    }
}

fn validate_shell_action(action: &ActionDef, action_name: &str) -> Result<Vec<String>> {
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

    // Manifest is only valid on shell + runner: pod
    if let Some(ref manifest) = action.manifest {
        let runner = action.runner.as_deref().unwrap_or("local");
        if runner != "pod" {
            bail!(
                "Action '{}' has 'manifest' field but runner is '{}' (manifest is only valid on pod actions)",
                action_name,
                runner
            );
        }
        if !manifest.is_object() {
            bail!(
                "Action '{}' has 'manifest' field but it is not an object",
                action_name
            );
        }
    }

    Ok(vec![])
}

fn validate_docker_action(action: &ActionDef, action_name: &str) -> Result<Vec<String>> {
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

    // Docker actions must not have manifest (use type: pod for Kubernetes manifest overrides)
    if action.manifest.is_some() {
        bail!(
            "Action '{}' is type 'docker' but has 'manifest' field (manifest is only valid on pod actions)",
            action_name
        );
    }

    Ok(vec![])
}

fn validate_pod_action(action: &ActionDef, action_name: &str) -> Result<Vec<String>> {
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

    // Validate manifest if present — must be a JSON object
    if let Some(ref manifest) = action.manifest {
        if !manifest.is_object() {
            bail!(
                "Action '{}' has 'manifest' field but it is not an object",
                action_name
            );
        }
    }

    Ok(vec![])
}

fn validate_task_action(action: &ActionDef, action_name: &str) -> Result<Vec<String>> {
    // Task actions must have task field
    if action.task.is_none() {
        bail!(
            "Action '{}' is type 'task' but missing 'task' field",
            action_name
        );
    }

    // Task actions must not have cmd, script, image, runner, or manifest
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
    if action.manifest.is_some() {
        bail!(
            "Action '{}' is type 'task' but has 'manifest' field (task actions only reference another task)",
            action_name
        );
    }

    Ok(vec![])
}

/// Validates that a hook references an existing action (or a library action).
fn validate_hook_action_exists(label: &str, action: &str, config: &WorkflowConfig) -> Result<()> {
    if action.contains('/') {
        return Ok(()); // library action — skip
    }
    if !config.actions.contains_key(action) {
        bail!("{} references non-existent action '{}'", label, action);
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
                name: None,
                description: None,
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
            TriggerDef::Scheduler {
                cron: "not a cron".to_string(),
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
                name: None,
                description: None,
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
            TriggerDef::Scheduler {
                cron: "*/10 * * * * *".to_string(),
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
        // With the tagged enum, `cron` is a required field on the Scheduler variant.
        // Missing cron now causes a serde parse error rather than a validation error.
        let yaml = r#"
triggers:
  bad:
    type: scheduler
    task: test
"#;
        let result: Result<crate::models::workflow::WorkflowConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cron"),
            "Error should mention missing cron field: {err}"
        );
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
    name: github-push
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

    // --- manifest validation tests ---

    #[test]
    fn test_manifest_rejected_on_shell_local() {
        let yaml = r#"
actions:
  test:
    type: shell
    cmd: "echo hi"
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("manifest"));
    }

    #[test]
    fn test_manifest_rejected_on_shell_docker() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: docker
    cmd: "echo hi"
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("manifest"));
    }

    #[test]
    fn test_manifest_rejected_on_docker() {
        let yaml = r#"
actions:
  test:
    type: docker
    image: alpine:latest
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("manifest"));
    }

    #[test]
    fn test_manifest_rejected_on_task() {
        let yaml = r#"
actions:
  test:
    type: task
    task: other
    manifest:
      spec:
        serviceAccountName: my-sa
tasks:
  other:
    flow:
      s:
        action: test
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("manifest"));
    }

    #[test]
    fn test_manifest_accepted_on_pod_type() {
        let yaml = r#"
actions:
  test:
    type: pod
    image: alpine:latest
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_manifest_accepted_on_shell_runner_pod() {
        let yaml = r#"
actions:
  test:
    type: shell
    runner: pod
    cmd: "echo hi"
    manifest:
      spec:
        serviceAccountName: my-sa
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_manifest_must_be_object() {
        let yaml = r#"
actions:
  test:
    type: pod
    image: alpine:latest
    manifest: "not an object"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not an object"));
    }

    // --- webhook trigger validation tests ---

    #[test]
    fn test_webhook_trigger_requires_name() {
        // Webhook without `name` → serde parse error
        let yaml = r#"
triggers:
  on-push:
    type: webhook
    task: ci-pipeline
"#;
        let result: Result<WorkflowConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("name"),
            "Error should mention missing name field: {err}"
        );
    }

    #[test]
    fn test_webhook_trigger_name_url_safe() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
tasks:
  ci:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: "invalid name with spaces!"
    task: ci
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid webhook name"));
    }

    #[test]
    fn test_webhook_trigger_valid() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
tasks:
  ci:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci
    secret: "whsec_abc123"
    input:
      environment: staging
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_webhook_trigger_no_secret_ok() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
tasks:
  ci:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        let trigger = config.triggers.get("on-push").unwrap();
        match trigger {
            TriggerDef::Webhook { secret, .. } => assert!(secret.is_none()),
            _ => panic!("Expected Webhook variant"),
        }
    }

    #[test]
    fn test_webhook_trigger_accessors() {
        let yaml = r#"
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    input:
      env: staging
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("on-push").unwrap();
        assert_eq!(trigger.trigger_type_str(), "webhook");
        assert_eq!(trigger.task(), "ci-pipeline");
        assert!(trigger.enabled());
        assert_eq!(trigger.input().get("env").unwrap(), "staging");
    }

    #[test]
    fn test_webhook_trigger_disabled() {
        let yaml = r#"
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    enabled: false
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("on-push").unwrap();
        assert!(!trigger.enabled());
    }

    #[test]
    fn test_webhook_trigger_sync_mode() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: sync
    timeout_secs: 60
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        let trigger = config.triggers.get("on-push").unwrap();
        match trigger {
            TriggerDef::Webhook {
                mode, timeout_secs, ..
            } => {
                assert_eq!(mode.as_deref(), Some("sync"));
                assert_eq!(*timeout_secs, Some(60));
            }
            _ => panic!("Expected Webhook variant"),
        }
    }

    #[test]
    fn test_webhook_trigger_async_mode() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: async
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_webhook_trigger_invalid_mode() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: blocking
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid mode"));
        assert!(err.contains("blocking"));
    }

    #[test]
    fn test_webhook_trigger_timeout_zero() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: sync
    timeout_secs: 0
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("timeout_secs"));
    }

    #[test]
    fn test_webhook_trigger_timeout_too_large() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"
tasks:
  ci-pipeline:
    flow:
      step1:
        action: greet
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
    mode: sync
    timeout_secs: 500
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("timeout_secs"));
    }

    #[test]
    fn test_webhook_trigger_no_mode_defaults_none() {
        let yaml = r#"
triggers:
  on-push:
    type: webhook
    name: github-push
    task: ci-pipeline
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

    // --- workspace-level hook validation tests ---

    #[test]
    fn test_validate_workspace_hook_valid_action() {
        let yaml = r#"
actions:
  notify:
    type: shell
    cmd: "curl $WEBHOOK"

on_success:
  - action: notify
    input:
      message: "All good"
on_error:
  - action: notify
    input:
      message: "Something failed"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_workspace_hook_missing_action() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"

on_success:
  - action: nonexistent
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Workspace on_success[0]"));
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn test_validate_workspace_hook_on_error_missing_action() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"

on_error:
  - action: missing-action
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Workspace on_error[0]"));
        assert!(err.contains("missing-action"));
    }

    #[test]
    fn test_validate_workspace_hook_library_action() {
        let yaml = r#"
on_success:
  - action: common/slack-notify
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    // --- connection validation tests ---

    #[test]
    fn test_validate_connections_valid() {
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
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_connections_invalid_property_type() {
        let yaml = r#"
connection_types:
  custom:
    properties:
      data:
        type: array
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid type"));
        assert!(err.contains("array"));
    }

    #[test]
    fn test_validate_connections_missing_type() {
        let yaml = r#"
connections:
  prod_db:
    type: nonexistent
    host: "db.example.com"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("non-existent connection type"));
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn test_validate_connections_missing_required_field() {
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
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("missing required field"));
        assert!(err.contains("host"));
    }

    #[test]
    fn test_validate_connections_unknown_field_warning() {
        let yaml = r#"
connection_types:
  postgres:
    properties:
      host:
        type: string
        required: true

connections:
  prod_db:
    type: postgres
    host: "db.example.com"
    extra_field: "unknown"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert!(warnings.iter().any(|w| w.contains("extra_field")));
    }

    #[test]
    fn test_validate_connections_empty_value() {
        let yaml = r#"
connections:
  api:
    url: ""
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("empty value"));
    }

    #[test]
    fn test_validate_connections_untyped_passthrough() {
        let yaml = r#"
connections:
  custom_api:
    url: "https://example.com"
    token: "abc123"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_connection_input_valid() {
        let yaml = r#"
connection_types:
  postgres:
    properties:
      host:
        type: string
        required: true

actions:
  run-migration:
    type: shell
    cmd: "migrate"

tasks:
  deploy:
    input:
      db:
        type: postgres
      env:
        type: string
    flow:
      migrate:
        action: run-migration
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_connection_input_unknown_type() {
        let yaml = r#"
actions:
  run-migration:
    type: shell
    cmd: "migrate"

tasks:
  deploy:
    input:
      db:
        type: nonexistent_type
    flow:
      migrate:
        action: run-migration
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent_type"));
        assert!(err.contains("not a primitive or connection type"));
    }

    #[test]
    fn test_validate_connection_input_primitives_ok() {
        let yaml = r#"
actions:
  greet:
    type: shell
    cmd: "echo hello"

tasks:
  test:
    input:
      name:
        type: string
      count:
        type: integer
      ratio:
        type: number
      enabled:
        type: boolean
      start_date:
        type: date
      scheduled_at:
        type: datetime
    flow:
      step1:
        action: greet
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_text_input_type_ok() {
        let yaml = r#"
actions:
  run-query:
    type: shell
    cmd: "echo hello"
    input:
      query:
        type: text

tasks:
  test:
    input:
      sql:
        type: text
    flow:
      step1:
        action: run-query
        input:
          query: "{{ input.sql }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_date_and_datetime_input_types_ok() {
        let yaml = r#"
actions:
  generate:
    type: shell
    cmd: "echo report"
tasks:
  report:
    input:
      start_date:
        type: date
        default: "2026-01-01"
      scheduled_at:
        type: datetime
    flow:
      step1:
        action: generate
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "date/datetime should be valid primitive types: {:?}",
            result
        );
        let warnings = result.unwrap();
        assert_eq!(
            warnings.len(),
            0,
            "Expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_secret_input_with_default_is_valid() {
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

tasks:
  release:
    input:
      token:
        type: string
        secret: true
        default: "{{ secret.TOKEN }}"
    flow:
      step1:
        action: deploy
        input:
          api_key: "{{ input.token }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
    }

    #[test]
    fn test_secret_input_without_default_is_valid() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
    input:
      password:
        type: string
        secret: true
        required: true

tasks:
  release:
    input:
      password:
        type: string
        secret: true
        required: true
    flow:
      step1:
        action: deploy
        input:
          password: "{{ input.password }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
    }

    #[test]
    fn test_input_field_options_valid() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: [staging, production]
        default: staging
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert_eq!(
            warnings.len(),
            0,
            "Expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_input_field_options_valid_action_level() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
    input:
      env:
        type: string
        options: [staging, production]
        default: staging
tasks:
  test:
    flow:
      step1:
        action: deploy
        input:
          env: "{{ input.env }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert_eq!(
            warnings.len(),
            0,
            "Expected no warnings for action-level options, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_input_field_empty_options_warning_action_level() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
    input:
      env:
        type: string
        options: []
tasks:
  test:
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("Action") && w.contains("options list is empty")),
            "Expected action-level empty options warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_input_field_empty_options_warning() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: []
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("options list is empty")),
            "Expected empty options warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_input_field_allow_custom_without_options_warning() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        allow_custom: true
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("allow_custom has no effect")),
            "Expected allow_custom warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_input_field_default_not_in_options_warning() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: [staging, production]
        default: development
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("not in options list")),
            "Expected default-not-in-options warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_input_field_default_not_in_options_ok_with_allow_custom() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: [staging, production]
        default: development
        allow_custom: true
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert_eq!(
            warnings.len(),
            0,
            "Expected no warnings with allow_custom, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_input_field_duplicate_options_warning() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options: [staging, production, staging]
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("duplicate option")),
            "Expected duplicate option warning, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_input_field_empty_string_option_warning() {
        let yaml = r#"
actions:
  deploy:
    type: shell
    cmd: "echo deploy"
tasks:
  test:
    input:
      env:
        type: string
        options:
          - ""
          - staging
          - production
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("contains empty string")),
            "Expected empty string option warning, got: {:?}",
            warnings
        );
    }
}
