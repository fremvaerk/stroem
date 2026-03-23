use crate::dag;
use crate::models::workflow::{ActionDef, WorkflowConfig};
use anyhow::{bail, Result};

/// Validates a workflow config and returns list of warnings.
/// Errors are returned as Err.
///
/// When `libraries_resolved` is true, library references (names containing `.`)
/// are validated against the config (they should already be merged in).
/// When false (CLI validation without server context), library references
/// are skipped with a warning.
pub fn validate_workflow_config(config: &WorkflowConfig) -> Result<Vec<String>> {
    validate_workflow_config_inner(config, false)
}

/// Validates a workflow config with full library validation.
/// Use this after libraries have been resolved and merged into the config.
pub fn validate_workflow_config_with_libraries(config: &WorkflowConfig) -> Result<Vec<String>> {
    validate_workflow_config_inner(config, true)
}

fn validate_workflow_config_inner(
    config: &WorkflowConfig,
    libraries_resolved: bool,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    // Validate each action
    for (action_name, action) in &config.actions {
        warnings.extend(validate_action(action_name, action)?);

        // For type: task, verify the referenced task exists
        if action.action_type == "task" {
            let task_ref = action.task.as_ref().expect(
                "task field is required for action_type == task, enforced by validate_action",
            );
            if !libraries_resolved && task_ref.contains('.') {
                // Library task — skip when libraries not resolved
            } else if !config.tasks.contains_key(task_ref) {
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
        for (i, hook) in task.on_suspended.iter().enumerate() {
            check_task_self_reference(
                config,
                task_name,
                &hook.action,
                &format!("on_suspended[{i}]"),
            )?;
        }
    }

    // Validate each task
    for (task_name, task) in &config.tasks {
        // Validate that flow steps reference existing actions
        for (step_name, step) in &task.flow {
            let action_ref = &step.action;

            // Check if it's a library action (contains .)
            if !libraries_resolved && action_ref.contains('.') {
                // Library actions can't be validated without server context
                warnings.push(format!(
                    "Task '{}' step '{}' references library action '{}' - validation skipped",
                    task_name, step_name, action_ref
                ));
            } else {
                // Local action (or resolved library action) - must exist in this config
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

            // Validate when condition template syntax.
            //
            // Two-pass strategy: first try `one_off` (compile + execute) with an
            // empty context to catch syntax errors. If that fails (because the
            // expression references variables not in the empty context), fall back
            // to `add_raw_template` (compile only) to distinguish syntax errors
            // from missing-variable errors.
            //
            // Known limitation — variable references: we cannot validate which
            // step names are referenced in the expression at parse time, because
            // the full step output context is only built at runtime during
            // `promote_ready_steps`. A typo like `{{ step_nme.output.value }}`
            // (missing 'a') passes this check and surfaces as a condition
            // evaluation failure when the job runs.
            //
            // Known limitation — unknown Tera filters: Tera's compile-time check
            // only validates syntax, not filter names. An expression like
            // `{{ foo | nonexistent_filter }}` passes both the `add_raw_template`
            // compile step and the `one_off` call with an empty context (because
            // the variable is undefined and the filter is never invoked), but will
            // fail at render time when `foo` has a value. Unknown filters are
            // therefore caught only at job execution time, not at YAML parse time.
            if let Some(ref when_expr) = step.when {
                if tera::Tera::one_off(when_expr, &tera::Context::new(), false).is_err() {
                    // Only catch syntax errors — undefined variables are OK at validation time
                    let mut test_tera = tera::Tera::default();
                    if let Err(e) = test_tera.add_raw_template("__when__", when_expr) {
                        bail!(
                            "Task '{}' step '{}' has invalid when expression '{}': {}",
                            task_name,
                            step_name,
                            when_expr,
                            e
                        );
                    }
                }
            }

            // Validate step name does not contain brackets (reserved for for_each instances)
            if step_name.contains('[') || step_name.contains(']') {
                bail!(
                    "Task '{}' step '{}' name must not contain '[' or ']' (reserved for for_each instances)",
                    task_name,
                    step_name
                );
            }

            // Validate for_each expression
            if let Some(ref for_each) = step.for_each {
                match for_each {
                    serde_json::Value::String(expr) => {
                        // Validate as Tera template (same two-pass strategy as `when`)
                        if tera::Tera::one_off(expr, &tera::Context::new(), false).is_err() {
                            let mut test_tera = tera::Tera::default();
                            if let Err(e) = test_tera.add_raw_template("__for_each__", expr) {
                                bail!(
                                    "Task '{}' step '{}' has invalid for_each expression '{}': {}",
                                    task_name,
                                    step_name,
                                    expr,
                                    e
                                );
                            }
                        }
                    }
                    serde_json::Value::Array(arr) => {
                        if arr.len() > 10000 {
                            bail!(
                                "Task '{}' step '{}' for_each literal array has {} items (max 10000)",
                                task_name,
                                step_name,
                                arr.len()
                            );
                        }
                    }
                    _ => {
                        bail!(
                            "Task '{}' step '{}' for_each must be a string (Tera template) or array, got {:?}",
                            task_name,
                            step_name,
                            for_each
                        );
                    }
                }
            }

            // Warn if sequential without for_each
            if step.sequential && step.for_each.is_none() {
                warnings.push(format!(
                    "Task '{}' step '{}' has sequential: true but no for_each — sequential has no effect",
                    task_name,
                    step_name
                ));
            }

            // Validate step timeout (max 24h = 86400s)
            if let Some(ref timeout) = step.timeout {
                if timeout.as_secs() > 86400 {
                    bail!(
                        "Task '{}' step '{}' timeout {}s exceeds maximum of 86400s (24h)",
                        task_name,
                        step_name,
                        timeout.as_secs()
                    );
                }
            }
        }

        // Validate task/job timeout (max 7d = 604800s)
        if let Some(ref timeout) = task.timeout {
            if timeout.as_secs() > 604800 {
                bail!(
                    "Task '{}' timeout {}s exceeds maximum of 604800s (7d)",
                    task_name,
                    timeout.as_secs()
                );
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
                libraries_resolved,
            )?;
        }
        for (i, hook) in task.on_error.iter().enumerate() {
            validate_hook_action_exists(
                &format!("Task '{task_name}' on_error[{i}]"),
                &hook.action,
                config,
                libraries_resolved,
            )?;
        }
        for (i, hook) in task.on_suspended.iter().enumerate() {
            validate_hook_action_exists(
                &format!("Task '{task_name}' on_suspended[{i}]"),
                &hook.action,
                config,
                libraries_resolved,
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
            crate::models::workflow::TriggerDef::Scheduler { cron, timezone, .. } => {
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
                // Validate timezone is a valid IANA timezone
                if let Some(tz_str) = timezone {
                    if tz_str.parse::<chrono_tz::Tz>().is_err() {
                        bail!(
                            "Trigger '{}' has invalid timezone '{}' (must be IANA, e.g., 'Europe/Copenhagen')",
                            trigger_name,
                            tz_str
                        );
                    }
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
        validate_hook_action_exists(
            &format!("Workspace on_success[{i}]"),
            &hook.action,
            config,
            libraries_resolved,
        )?;
    }
    for (i, hook) in config.on_error.iter().enumerate() {
        validate_hook_action_exists(
            &format!("Workspace on_error[{i}]"),
            &hook.action,
            config,
            libraries_resolved,
        )?;
    }
    for (i, hook) in config.on_suspended.iter().enumerate() {
        validate_hook_action_exists(
            &format!("Workspace on_suspended[{i}]"),
            &hook.action,
            config,
            libraries_resolved,
        )?;
    }

    // Check for task name collisions after hyphen→underscore normalization.
    // Two tasks whose names differ only by hyphens vs underscores would map
    // to the same agent tool name, causing ambiguous tool routing.
    {
        let mut normalized_names: std::collections::HashMap<String, Vec<&str>> =
            std::collections::HashMap::new();
        for task_name in config.tasks.keys() {
            let normalized = task_name.replace('-', "_");
            normalized_names
                .entry(normalized)
                .or_default()
                .push(task_name);
        }
        for (normalized, names) in &normalized_names {
            if names.len() > 1 {
                bail!(
                    "Tasks {} have the same normalized name '{}' — agent tool names would collide. Rename one to avoid ambiguity.",
                    names.iter().map(|n| format!("'{}'", n)).collect::<Vec<_>>().join(" and "),
                    normalized
                );
            }
        }
    }

    // Same collision check for MCP server names.
    {
        let mut normalized_names: std::collections::HashMap<String, Vec<&str>> =
            std::collections::HashMap::new();
        for server_name in config.mcp_servers.keys() {
            let normalized = server_name.replace('-', "_");
            normalized_names
                .entry(normalized)
                .or_default()
                .push(server_name);
        }
        for (normalized, names) in &normalized_names {
            if names.len() > 1 {
                bail!(
                    "MCP servers {} have the same normalized name '{}' — tool name prefixes would collide. Rename one to avoid ambiguity.",
                    names.iter().map(|n| format!("'{}'", n)).collect::<Vec<_>>().join(" and "),
                    normalized
                );
            }
        }
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
    // Validate output field types if present — applies to all action types
    if let Some(ref output_def) = action.output {
        let valid_types = ["string", "integer", "number", "boolean", "array", "object"];
        for (field_name, field) in &output_def.properties {
            if !valid_types.contains(&field.field_type.as_str()) {
                bail!(
                    "Action '{}' output field '{}' has invalid type '{}'. Valid types: {}",
                    action_name,
                    field_name,
                    field.field_type,
                    valid_types.join(", ")
                );
            }
        }
    }

    match action.action_type.as_str() {
        "script" => validate_script_action(action, action_name),
        "docker" => validate_docker_action(action, action_name),
        "pod" => validate_pod_action(action, action_name),
        "task" => validate_task_action(action, action_name),
        "agent" => validate_agent_action(action, action_name),
        "approval" => validate_approval_action(action, action_name),
        other => bail!(
            "Action '{}' has invalid type '{}' (expected: script, docker, pod, task, agent, approval)",
            action_name,
            other
        ),
    }
}

fn validate_script_action(action: &ActionDef, action_name: &str) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    // cmd is not valid on type: script — use 'script' (inline) or 'source' (file)
    if action.cmd.is_some() {
        bail!(
            "Action '{}': 'cmd' is not valid on type: script — use 'script' (inline) or 'source' (file path)",
            action_name
        );
    }

    // Reject empty string values — an empty field is not a valid script or path
    if action.script.as_deref() == Some("") {
        bail!("Action '{}' has empty 'script' field", action_name);
    }
    if action.source.as_deref() == Some("") {
        bail!("Action '{}' has empty 'source' field", action_name);
    }

    // Script actions accept inline code (script) or a source file path
    let has_inline = action.script.is_some();
    let has_source = action.source.is_some();

    if !has_inline && !has_source {
        bail!(
            "Action '{}' is type 'script' but missing 'script' or 'source' field",
            action_name
        );
    }

    if has_inline && has_source {
        bail!(
            "Action '{}' has both inline script and source file — use only one",
            action_name
        );
    }

    // Script actions must not have 'image' — use type: docker (container action) or runner: docker (script-in-container) instead
    if action.image.is_some() {
        bail!(
            "Action '{}' is type 'script' but has 'image' field. Use type: docker for container images, or runner: docker for shell-in-container",
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

    // Script actions must not have entrypoint
    if action.entrypoint.is_some() {
        bail!(
            "Action '{}' is type 'script' but has 'entrypoint' field (only valid for docker/pod)",
            action_name
        );
    }

    // Manifest is only valid on script + runner: pod
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

    // Validate language field if present
    if let Some(ref lang) = action.language {
        if !crate::language::VALID_LANGUAGE_NAMES.contains(&lang.as_str()) {
            bail!(
                "Action '{}' has invalid language '{}' (expected: {})",
                action_name,
                lang,
                crate::language::VALID_LANGUAGE_NAMES.join(", ")
            );
        }
    }

    // Validate interpreter field - reject shell metacharacters
    if let Some(ref interp) = action.interpreter {
        if interp.is_empty() {
            bail!("Action '{}' has empty 'interpreter' field", action_name);
        }
        if !interp
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/+".contains(c))
        {
            bail!(
                "Action '{}' has invalid 'interpreter' value '{}': only alphanumeric characters, dots, dashes, underscores, slashes, and plus signs are allowed",
                action_name, interp
            );
        }
    }

    // Validate dependencies - reject shell metacharacters
    for dep in &action.dependencies {
        if dep.is_empty() {
            bail!("Action '{}' has empty dependency entry", action_name);
        }
        // Allow typical package specifier chars: alphanumeric, dots, dashes, underscores,
        // brackets, comparison operators, commas, at-signs, slashes, colons, tildes, spaces
        // Reject shell metacharacters: ; & | $ ` ( ) { } > < \ newlines quotes
        let has_shell_metachar = dep.chars().any(|c| {
            matches!(
                c,
                ';' | '&'
                    | '|'
                    | '$'
                    | '`'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '\\'
                    | '\n'
                    | '\r'
                    | '\''
                    | '"'
            )
        });
        if has_shell_metachar {
            bail!(
                "Action '{}' has dependency '{}' containing shell metacharacters",
                action_name,
                dep
            );
        }
    }

    // Warn if dependencies set without explicit language
    if !action.dependencies.is_empty() && action.language.is_none() {
        warnings.push(format!(
            "Action '{}' has 'dependencies' but no 'language' set (dependencies require a non-shell language)",
            action_name
        ));
    }

    // Warn if interpreter set without explicit language
    if action.interpreter.is_some() && action.language.is_none() {
        warnings.push(format!(
            "Action '{}' has 'interpreter' but no 'language' set",
            action_name
        ));
    }

    Ok(warnings)
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
            "Action '{}' is type 'docker' but has 'runner' field (runner is only for script actions)",
            action_name
        );
    }

    // Docker actions must not have script (inline code for type:script)
    if action.script.is_some() {
        bail!(
            "Action '{}' is type 'docker' but has 'script' field (script is for type:script actions, use cmd for docker)",
            action_name
        );
    }

    // Docker actions must not have source (file path for type:script)
    if action.source.is_some() {
        bail!(
            "Action '{}' is type 'docker' but has 'source' field (source is for type:script actions)",
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

    // Docker actions must not have language, dependencies, or interpreter
    if action.language.is_some() {
        bail!(
            "Action '{}' is type 'docker' but has 'language' field (only valid for script actions)",
            action_name
        );
    }
    if !action.dependencies.is_empty() {
        bail!(
            "Action '{}' is type 'docker' but has 'dependencies' field (only valid for script actions)",
            action_name
        );
    }
    if action.interpreter.is_some() {
        bail!(
            "Action '{}' is type 'docker' but has 'interpreter' field (only valid for script actions)",
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
            "Action '{}' is type 'pod' but has 'runner' field (runner is only for script actions)",
            action_name
        );
    }

    // Pod actions must not have script (inline code for type:script)
    if action.script.is_some() {
        bail!(
            "Action '{}' is type 'pod' but has 'script' field (script is for type:script actions, use cmd for pod)",
            action_name
        );
    }

    // Pod actions must not have source (file path for type:script)
    if action.source.is_some() {
        bail!(
            "Action '{}' is type 'pod' but has 'source' field (source is for type:script actions)",
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

    // Pod actions must not have language, dependencies, or interpreter
    if action.language.is_some() {
        bail!(
            "Action '{}' is type 'pod' but has 'language' field (only valid for script actions)",
            action_name
        );
    }
    if !action.dependencies.is_empty() {
        bail!(
            "Action '{}' is type 'pod' but has 'dependencies' field (only valid for script actions)",
            action_name
        );
    }
    if action.interpreter.is_some() {
        bail!(
            "Action '{}' is type 'pod' but has 'interpreter' field (only valid for script actions)",
            action_name
        );
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
    if action.source.is_some() {
        bail!(
            "Action '{}' is type 'task' but has 'source' field (task actions only reference another task)",
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

    // Task actions must not have language, dependencies, or interpreter
    if action.language.is_some() {
        bail!(
            "Action '{}' is type 'task' but has 'language' field (only valid for script actions)",
            action_name
        );
    }
    if !action.dependencies.is_empty() {
        bail!(
            "Action '{}' is type 'task' but has 'dependencies' field (only valid for script actions)",
            action_name
        );
    }
    if action.interpreter.is_some() {
        bail!(
            "Action '{}' is type 'task' but has 'interpreter' field (only valid for script actions)",
            action_name
        );
    }

    Ok(vec![])
}

fn validate_agent_action(action: &ActionDef, action_name: &str) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    // Required fields
    if action.provider.is_none() {
        bail!(
            "Action '{}' is type 'agent' but missing 'provider' field",
            action_name
        );
    }
    if action.prompt.is_none() {
        bail!(
            "Action '{}' is type 'agent' but missing 'prompt' field",
            action_name
        );
    }

    // Forbidden fields (incompatible with agent type)
    if action.cmd.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'cmd' field",
            action_name
        );
    }
    if action.script.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'script' field",
            action_name
        );
    }
    if action.source.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'source' field",
            action_name
        );
    }
    if action.image.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'image' field",
            action_name
        );
    }
    if action.runner.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'runner' field",
            action_name
        );
    }
    if action.manifest.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'manifest' field",
            action_name
        );
    }
    if action.task.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'task' field",
            action_name
        );
    }
    if action.language.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'language' field",
            action_name
        );
    }
    if action.entrypoint.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'entrypoint' field",
            action_name
        );
    }
    if !action.dependencies.is_empty() {
        bail!(
            "Action '{}' is type 'agent' but has 'dependencies' field",
            action_name
        );
    }
    if action.interpreter.is_some() {
        bail!(
            "Action '{}' is type 'agent' but has 'interpreter' field",
            action_name
        );
    }

    // Validate temperature range
    if let Some(temp) = action.temperature {
        if !(0.0..=2.0).contains(&temp) {
            bail!(
                "Action '{}' temperature must be between 0.0 and 2.0, got {}",
                action_name,
                temp
            );
        }
    }

    // Validate prompt is valid Tera template syntax (same two-pass strategy as `when`)
    if let Some(ref prompt) = action.prompt {
        if tera::Tera::one_off(prompt, &tera::Context::new(), false).is_err() {
            let mut test_tera = tera::Tera::default();
            if let Err(e) = test_tera.add_raw_template("__prompt__", prompt) {
                bail!(
                    "Action '{}' has invalid prompt template: {}",
                    action_name,
                    e
                );
            }
        }
    }

    // Same for system_prompt
    if let Some(ref sp) = action.system_prompt {
        if tera::Tera::one_off(sp, &tera::Context::new(), false).is_err() {
            let mut test_tera = tera::Tera::default();
            if let Err(e) = test_tera.add_raw_template("__system_prompt__", sp) {
                bail!(
                    "Action '{}' has invalid system_prompt template: {}",
                    action_name,
                    e
                );
            }
        }
    }

    // Validate max_turns range (1..=100)
    if let Some(turns) = action.max_turns {
        if turns == 0 || turns > 100 {
            bail!(
                "Action '{}' max_turns must be between 1 and 100, got {}",
                action_name,
                turns
            );
        }
    }

    // Warn: max_turns without tools has no effect
    if action.max_turns.is_some() && action.tools.is_empty() && !action.interactive {
        warnings.push(format!(
            "Action '{}': max_turns is set but the action has no tools defined — max_turns has no effect on single-turn agents",
            action_name
        ));
    }

    // Warn: interactive without tools
    if action.interactive && action.tools.is_empty() {
        warnings.push(format!(
            "Action '{}': interactive=true but the action has no tools defined — interactive mode enables ask_user suspensions but task tools and MCP tools are unavailable",
            action_name
        ));
    }

    // Warn: interactive without max_turns (infinite loop risk)
    if action.interactive && !action.tools.is_empty() && action.max_turns.is_none() {
        warnings.push(format!(
            "Action '{}': interactive=true with no max_turns set — consider setting max_turns to prevent unbounded conversation loops",
            action_name
        ));
    }

    // Validate tool references: empty strings are not allowed
    for tool_ref in &action.tools {
        match tool_ref {
            crate::models::workflow::AgentToolRef::Task { task } => {
                if task.is_empty() {
                    bail!(
                        "Action '{}' has a tool with empty task reference",
                        action_name
                    );
                }
            }
            crate::models::workflow::AgentToolRef::Mcp { mcp } => {
                if mcp.is_empty() {
                    bail!(
                        "Action '{}' has a tool with empty MCP server reference",
                        action_name
                    );
                }
            }
        }
    }

    Ok(warnings)
}

fn validate_approval_action(action: &ActionDef, action_name: &str) -> Result<Vec<String>> {
    // message is required for approval actions
    if action.message.is_none() {
        bail!(
            "Action '{}' is type 'approval' but missing 'message' field",
            action_name
        );
    }

    // Forbidden fields — approval gates have no execution context
    if action.cmd.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'cmd' field",
            action_name
        );
    }
    if action.script.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'script' field",
            action_name
        );
    }
    if action.source.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'source' field",
            action_name
        );
    }
    if action.image.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'image' field",
            action_name
        );
    }
    if action.runner.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'runner' field",
            action_name
        );
    }
    if action.manifest.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'manifest' field",
            action_name
        );
    }
    if action.task.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'task' field",
            action_name
        );
    }
    if action.entrypoint.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'entrypoint' field",
            action_name
        );
    }
    if action.command.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'command' field",
            action_name
        );
    }
    if action.provider.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'provider' field",
            action_name
        );
    }
    if action.prompt.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'prompt' field",
            action_name
        );
    }
    if action.system_prompt.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'system_prompt' field",
            action_name
        );
    }
    // Note: `output` is allowed on approval actions (it's documentation, not enforcement)
    if action.temperature.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'temperature' field",
            action_name
        );
    }
    if action.max_tokens.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'max_tokens' field",
            action_name
        );
    }
    if !action.tools.is_empty() {
        bail!(
            "Action '{}' is type 'approval' but has 'tools' field",
            action_name
        );
    }
    if action.max_turns.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'max_turns' field",
            action_name
        );
    }
    if action.language.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'language' field",
            action_name
        );
    }
    if !action.dependencies.is_empty() {
        bail!(
            "Action '{}' is type 'approval' but has 'dependencies' field",
            action_name
        );
    }
    if action.interpreter.is_some() {
        bail!(
            "Action '{}' is type 'approval' but has 'interpreter' field",
            action_name
        );
    }

    Ok(vec![])
}

/// Validates that a hook references an existing action (or a library action).
fn validate_hook_action_exists(
    label: &str,
    action: &str,
    config: &WorkflowConfig,
    libraries_resolved: bool,
) -> Result<()> {
    if !libraries_resolved && action.contains('.') {
        return Ok(()); // library action — skip when libraries not resolved
    }
    if !config.actions.contains_key(action) {
        bail!("{} references non-existent action '{}'", label, action);
    }
    Ok(())
}

/// Validates a map of MCP server definitions. Returns `Ok(Vec<String>)` with warnings on success.
///
/// Validates each server has the correct fields for its transport type:
/// - `stdio`: requires `command`, rejects `url`
/// - `sse`: requires `url`, rejects `command`/`args`
pub fn validate_mcp_servers(
    servers: &std::collections::HashMap<String, crate::models::workflow::McpServerDef>,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    for (name, def) in servers {
        match def.transport.as_str() {
            "stdio" => {
                if def.command.is_none() {
                    bail!(
                        "MCP server '{}' uses stdio transport but is missing 'command' field",
                        name
                    );
                }
                if def.url.is_some() {
                    bail!(
                        "MCP server '{}' uses stdio transport but has 'url' field (only valid for sse transport)",
                        name
                    );
                }
            }
            "sse" => {
                if def.url.is_none() {
                    bail!(
                        "MCP server '{}' uses sse transport but is missing 'url' field",
                        name
                    );
                }
                if let Some(ref url) = def.url {
                    if url
                        .parse::<url::Url>()
                        .map_or(true, |u| u.scheme() != "http" && u.scheme() != "https")
                    {
                        bail!(
                            "MCP server '{}' url must be a valid http or https URL, got '{}'",
                            name,
                            url
                        );
                    }
                }
                if def.command.is_some() {
                    warnings.push(format!(
                        "MCP server '{}': 'command' field is ignored for sse transport",
                        name
                    ));
                }
                if def.args.is_some() {
                    warnings.push(format!(
                        "MCP server '{}': 'args' field is ignored for sse transport",
                        name
                    ));
                }
            }
            other => {
                bail!(
                    "MCP server '{}' has unknown transport type '{}' (expected: stdio, sse)",
                    name,
                    other
                );
            }
        }
    }

    Ok(warnings)
}

/// Compute the required worker tags for an action.
/// These tags must all be present on a worker for it to claim a step using this action.
pub fn compute_required_tags(action: &ActionDef) -> Vec<String> {
    // Task and approval actions are handled server-side, not by workers
    if action.action_type == "task" || action.action_type == "approval" {
        return vec![];
    }

    // Agent actions require a worker with the "agent" tag
    if action.action_type == "agent" {
        let mut tags = vec!["agent".to_string()];
        for tag in &action.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        return tags;
    }

    let base_tag = match action.action_type.as_str() {
        "script" => match action.runner.as_deref() {
            Some("docker") => "docker",
            Some("pod") => "kubernetes",
            _ => "script",
        },
        "docker" => "docker",
        "pod" => "kubernetes",
        _ => "script",
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
        "script" => action.runner.as_deref().unwrap_or("local").to_string(),
        "docker" | "pod" => "none".to_string(),
        "task" | "agent" | "approval" => "none".to_string(),
        _ => "local".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::workflow::{AgentToolRef, McpServerDef, TaskDef, TriggerDef};
    use std::collections::HashMap;

    #[test]
    fn test_validate_valid_workflow() {
        let yaml = r#"
actions:
  greet:
    type: script
    script: "echo Hello"
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
    type: script
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing 'script' or 'source'"));
    }

    #[test]
    fn test_validate_action_shell_both_cmd_and_script() {
        let yaml = r#"
actions:
  bad:
    type: script
    cmd: "echo test"
    script: "test.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not valid on type: script"));
    }

    #[test]
    fn test_validate_action_script_with_script_field() {
        let yaml = r#"
actions:
  greet:
    type: script
    script: "echo Hello"

tasks:
  test:
    flow:
      step1:
        action: greet
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        // No deprecation warning when using 'script' field
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_validate_action_script_with_source_field() {
        let yaml = r#"
actions:
  deploy:
    type: script
    source: "deploy.sh"

tasks:
  test:
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_validate_action_script_cmd_rejected() {
        let yaml = r#"
actions:
  greet:
    type: script
    cmd: "echo Hello"

tasks:
  test:
    flow:
      step1:
        action: greet
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not valid on type: script"));
    }

    #[test]
    fn test_validate_action_script_both_script_and_source() {
        let yaml = r#"
actions:
  bad:
    type: script
    script: "echo hello"
    source: "deploy.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("both inline script and source file"));
    }

    #[test]
    fn test_validate_docker_rejects_script_field() {
        let yaml = r#"
actions:
  bad:
    type: docker
    image: node:20
    script: "echo hello"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("has 'script' field"));
    }

    #[test]
    fn test_validate_docker_rejects_source_field() {
        let yaml = r#"
actions:
  bad:
    type: docker
    image: node:20
    source: "deploy.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("has 'source' field"));
    }

    #[test]
    fn test_validate_pod_rejects_source_field() {
        let yaml = r#"
actions:
  bad:
    type: pod
    image: node:20
    source: "deploy.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("has 'source' field"));
    }

    #[test]
    fn test_validate_task_rejects_source_field() {
        let yaml = r#"
actions:
  bad:
    type: task
    task: other-task
    source: "deploy.sh"

tasks:
  other-task:
    flow:
      s1:
        action: bad
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("has 'source' field"));
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
    type: script
    script: "echo test"

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
    type: script
    script: "echo test"

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
    type: script
    script: "echo test"

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
                timeout: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
            },
        );
        config.triggers.insert(
            "bad-cron".to_string(),
            TriggerDef::Scheduler {
                cron: "not a cron".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
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
                timeout: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
            },
        );
        config.triggers.insert(
            "every-10s".to_string(),
            TriggerDef::Scheduler {
                cron: "*/10 * * * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: None,
            },
        );

        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_trigger_valid_timezone() {
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
                timeout: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
            },
        );
        config.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: Some("Europe/Copenhagen".to_string()),
            },
        );

        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_trigger_utc_timezone() {
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
                timeout: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
            },
        );
        config.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: Some("UTC".to_string()),
            },
        );

        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_trigger_invalid_timezone() {
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
                timeout: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
            },
        );
        config.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: Some("Not/A/Timezone".to_string()),
            },
        );

        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid timezone"));
    }

    #[test]
    fn test_validate_trigger_empty_string_timezone() {
        // Empty string is Some("") which should fail timezone parsing
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
                timeout: None,
                on_success: vec![],
                on_error: vec![],
                on_suspended: vec![],
            },
        );
        config.triggers.insert(
            "nightly".to_string(),
            TriggerDef::Scheduler {
                cron: "0 2 * * *".to_string(),
                task: "test".to_string(),
                input: HashMap::new(),
                enabled: true,
                concurrency: Default::default(),
                timezone: Some("".to_string()),
            },
        );

        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid timezone"));
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
        action: common.slack-notify
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("library action"));
        assert!(warnings[0].contains("common.slack-notify"));
    }

    #[test]
    fn test_validate_shell_with_script() {
        let yaml = r#"
actions:
  deploy:
    type: script
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
    type: script
    image: node:20
    script: "npm run build"
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
    type: script
    script: "git clone repo"
  build:
    type: script
    runner: docker
    script: "npm run build"
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
    type: script
    script: "pg_dump"
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
    type: script
    script: "pg_dump"
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
    type: script
    runner: docker
    script: "npm test"
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
    type: script
    runner: invalid
    script: "npm test"
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
    type: script
    script: "echo hi"
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
        // docker action with image only (uses image default entrypoint)
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
    type: script
    runner: docker
    tags: ["node-20"]
    script: "npm test"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_compute_required_tags_script_local() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: script
script: "echo test"
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["script"]);
    }

    #[test]
    fn test_compute_required_tags_script_docker() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: script
runner: docker
script: "npm test"
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), vec!["docker"]);
    }

    #[test]
    fn test_compute_required_tags_script_docker_with_tags() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: script
runner: docker
tags: ["node-20"]
script: "npm test"
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
    fn test_compute_required_tags_script_pod() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: script
runner: pod
script: "npm test"
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
    type: script
    runner: pod
    script: "npm test"
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
    type: script
    runner: local
    script: "echo test"
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
type: script
script: "echo test"
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
type: script
runner: docker
script: "npm test"
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
type: script
runner: pod
script: "npm test"
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
    type: script
    script: "make deploy"
  notify:
    type: script
    script: "curl $WEBHOOK"

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
    type: script
    script: "make deploy"

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
    type: script
    script: "make deploy"

tasks:
  release:
    flow:
      step1:
        action: deploy
    on_success:
      - action: common.slack-notify
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hi"
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
    type: script
    runner: docker
    script: "echo hi"
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
    type: script
    runner: pod
    script: "echo hi"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo hello"
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
    type: script
    script: "curl $WEBHOOK"

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
    type: script
    script: "echo hello"

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
    type: script
    script: "echo hello"

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
  - action: common.slack-notify
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
    host:
      type: string
      required: true

actions:
  run-migration:
    type: script
    script: "migrate"

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
    type: script
    script: "migrate"

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
    type: script
    script: "echo hello"

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
    type: script
    script: "echo hello"
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
    type: script
    script: "echo report"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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
    type: script
    script: "echo deploy"
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

    // --- validate_workflow_config_with_libraries tests ---

    #[test]
    fn test_validate_with_libraries_rejects_missing_library_action() {
        // Task step references a library action that is NOT in config.actions.
        // In server mode (libraries_resolved = true) this must be an error.
        let yaml = r#"
tasks:
  test:
    flow:
      step1:
        action: common.slack-notify
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config_with_libraries(&config);
        assert!(result.is_err(), "Expected Err for missing library action");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-existent action"),
            "Error should mention 'non-existent action', got: {err}"
        );
        assert!(
            err.contains("common.slack-notify"),
            "Error should mention the action name, got: {err}"
        );
    }

    #[test]
    fn test_validate_with_libraries_accepts_present_library_action() {
        // Library action is defined in config.actions AND referenced by a task step.
        // Validation must succeed with no errors.
        let yaml = r#"
actions:
  common.slack-notify:
    type: script
    script: "echo notify"
tasks:
  test:
    flow:
      step1:
        action: common.slack-notify
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config_with_libraries(&config);
        assert!(
            result.is_ok(),
            "Expected Ok for present library action, got: {:?}",
            result.unwrap_err()
        );
        let warnings = result.unwrap();
        assert!(
            warnings.is_empty(),
            "Expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_validate_with_libraries_rejects_missing_library_task_ref() {
        // A type:task action references a library task that is NOT in config.tasks.
        // In server mode this must be an error.
        let yaml = r#"
actions:
  deploy:
    type: task
    task: common.deploy-service
tasks:
  caller:
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config_with_libraries(&config);
        assert!(result.is_err(), "Expected Err for missing library task ref");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-existent task"),
            "Error should mention 'non-existent task', got: {err}"
        );
        assert!(
            err.contains("common.deploy-service"),
            "Error should mention the task name, got: {err}"
        );
    }

    #[test]
    fn test_validate_with_libraries_accepts_present_library_task_ref() {
        // A type:task action references a library task that IS in config.tasks.
        // The child task itself uses a local action.
        let yaml = r#"
actions:
  deploy:
    type: task
    task: common.deploy-service
  run-step:
    type: script
    script: "echo running"
tasks:
  common.deploy-service:
    flow:
      build:
        action: run-step
  caller:
    flow:
      step1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config_with_libraries(&config);
        assert!(
            result.is_ok(),
            "Expected Ok when library task ref is present, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_validate_with_libraries_rejects_missing_hook_action() {
        // A task on_error hook references a library action that is NOT in config.actions.
        let yaml = r#"
actions:
  build:
    type: script
    script: "echo build"
tasks:
  test:
    flow:
      step1:
        action: build
    on_error:
      - action: common.alert
        input:
          message: "build failed"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config_with_libraries(&config);
        assert!(result.is_err(), "Expected Err for missing hook action");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-existent action"),
            "Error should mention 'non-existent action', got: {err}"
        );
        assert!(
            err.contains("common.alert"),
            "Error should mention the action name, got: {err}"
        );
    }

    #[test]
    fn test_validate_with_libraries_accepts_present_hook_action() {
        // Library alert action is in config.actions; task on_error hook references it.
        let yaml = r#"
actions:
  build:
    type: script
    script: "echo build"
  common.alert:
    type: script
    script: "echo alert"
tasks:
  test:
    flow:
      step1:
        action: build
    on_error:
      - action: common.alert
        input:
          message: "build failed"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config_with_libraries(&config);
        assert!(
            result.is_ok(),
            "Expected Ok when hook action is present, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_validate_with_libraries_rejects_missing_workspace_hook_action() {
        // Workspace-level on_error hook references a library action not in config.actions.
        let yaml = r#"
actions:
  build:
    type: script
    script: "echo build"
tasks:
  test:
    flow:
      step1:
        action: build
on_error:
  - action: common.notify
    input:
      message: "workspace job failed"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config_with_libraries(&config);
        assert!(
            result.is_err(),
            "Expected Err for missing workspace-level hook action"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-existent action"),
            "Error should mention 'non-existent action', got: {err}"
        );
        assert!(
            err.contains("common.notify"),
            "Error should mention the action name, got: {err}"
        );
    }

    #[test]
    fn test_validate_with_libraries_mixed_local_and_library_actions() {
        // Both a local action and a fully-resolved library action are defined.
        // A task uses both in its flow steps. Validation must succeed with no warnings.
        let yaml = r#"
actions:
  build:
    type: script
    script: "echo build"
  common.deploy:
    type: script
    script: "echo deploy"
tasks:
  release:
    flow:
      step-build:
        action: build
      step-deploy:
        action: common.deploy
        depends_on:
          - step-build
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config_with_libraries(&config);
        assert!(
            result.is_ok(),
            "Expected Ok for mixed local and library actions, got: {:?}",
            result.unwrap_err()
        );
        let warnings = result.unwrap();
        assert!(
            warnings.is_empty(),
            "Expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_validate_step_timeout_within_limit() {
        let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    flow:
      build:
        action: build
        timeout: 10m
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Expected Ok, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_validate_step_timeout_exceeds_limit() {
        let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    flow:
      build:
        action: build
        timeout: 86401
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("exceeds maximum of 86400s"));
    }

    #[test]
    fn test_validate_task_timeout_within_limit() {
        let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    timeout: 30m
    flow:
      build:
        action: build
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Expected Ok, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_validate_task_timeout_exceeds_limit() {
        let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    timeout: 604801
    flow:
      build:
        action: build
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("exceeds maximum of 604800s"));
    }

    #[test]
    fn test_validate_concurrency_policy_deserializes() {
        let yaml = r#"
actions:
  build:
    type: script
    script: "make build"
tasks:
  deploy:
    flow:
      build:
        action: build
triggers:
  hourly:
    type: scheduler
    cron: "0 * * * *"
    task: deploy
    concurrency: skip
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Expected Ok, got: {:?}",
            result.unwrap_err()
        );
        let trigger = config.triggers.get("hourly").unwrap();
        assert_eq!(
            trigger.concurrency(),
            crate::models::workflow::ConcurrencyPolicy::Skip
        );
    }

    #[test]
    fn test_validate_concurrency_policy_cancel_previous() {
        let yaml = r#"
actions:
  etl:
    type: script
    script: "python etl.py"
tasks:
  pipeline:
    flow:
      run:
        action: etl
triggers:
  hourly:
    type: scheduler
    cron: "0 * * * *"
    task: pipeline
    concurrency: cancel_previous
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("hourly").unwrap();
        assert_eq!(
            trigger.concurrency(),
            crate::models::workflow::ConcurrencyPolicy::CancelPrevious
        );
    }

    #[test]
    fn test_validate_concurrency_default_is_allow() {
        let yaml = r#"
actions:
  build:
    type: script
    script: "make"
tasks:
  ci:
    flow:
      build:
        action: build
triggers:
  every_min:
    type: scheduler
    cron: "* * * * *"
    task: ci
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let trigger = config.triggers.get("every_min").unwrap();
        assert_eq!(
            trigger.concurrency(),
            crate::models::workflow::ConcurrencyPolicy::Allow
        );
    }

    // --- script action language / interpreter / dependency validation tests ---

    #[test]
    fn test_validate_script_action_invalid_language() {
        let yaml = r#"
actions:
  run:
    type: script
    script: "ruby script.rb"
    language: ruby
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid language"));
    }

    #[test]
    fn test_validate_script_action_valid_languages() {
        for lang in &["python", "javascript", "typescript", "go", "js", "ts"] {
            let yaml = format!(
                r#"
actions:
  run:
    type: script
    script: "echo hello"
    language: {lang}
"#
            );
            let config: WorkflowConfig = serde_yaml::from_str(&yaml).unwrap();
            let result = validate_workflow_config(&config);
            assert!(
                result.is_ok(),
                "language '{lang}' should be valid, got: {:?}",
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn test_validate_script_action_deps_without_language_warns() {
        let yaml = r#"
actions:
  run:
    type: script
    script: "python script.py"
    dependencies:
      - requests
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "Should not error, got: {:?}", result);
        let warnings = result.unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("dependencies") && w.contains("language")),
            "Expected warning about dependencies without language, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_validate_script_action_interpreter_without_language_warns() {
        let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    interpreter: "python3.12"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "Should not error, got: {:?}", result);
        let warnings = result.unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("interpreter") && w.contains("language")),
            "Expected warning about interpreter without language, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_validate_script_action_full_valid() {
        let yaml = r#"
actions:
  run:
    type: script
    script: "python script.py"
    language: python
    dependencies:
      - requests
    interpreter: "python3.12"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
        let warnings = result.unwrap();
        assert!(
            warnings.is_empty(),
            "Expected no warnings, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_validate_script_action_interpreter_shell_metachar() {
        let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    language: python
    interpreter: "python3; rm -rf /"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid 'interpreter' value"));
    }

    #[test]
    fn test_validate_script_action_interpreter_empty() {
        let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    language: python
    interpreter: ""
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("empty 'interpreter'"));
    }

    #[test]
    fn test_validate_script_action_deps_shell_metachar() {
        let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    language: python
    dependencies:
      - "requests; evil"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("shell metacharacters"));
    }

    #[test]
    fn test_validate_script_action_deps_empty_entry() {
        let yaml = r#"
actions:
  run:
    type: script
    script: "script.py"
    language: python
    dependencies:
      - ""
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("empty dependency entry"));
    }

    #[test]
    fn test_validate_docker_action_rejects_language() {
        let yaml = r#"
actions:
  run:
    type: docker
    image: python:3.12
    language: python
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'language' field"));
    }

    #[test]
    fn test_validate_docker_action_rejects_dependencies() {
        let yaml = r#"
actions:
  run:
    type: docker
    image: python:3.12
    dependencies:
      - requests
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("'dependencies' field"));
    }

    #[test]
    fn test_validate_docker_action_rejects_interpreter() {
        let yaml = r#"
actions:
  run:
    type: docker
    image: python:3.12
    interpreter: "python3.12"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("'interpreter' field"));
    }

    #[test]
    fn test_validate_pod_action_rejects_language() {
        let yaml = r#"
actions:
  run:
    type: pod
    image: python:3.12
    language: python
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'language' field"));
    }

    #[test]
    fn test_validate_task_action_rejects_language() {
        let yaml = r#"
actions:
  run-child:
    type: task
    task: other
    language: python
tasks:
  other:
    flow:
      s:
        action: run-child
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'language' field"));
    }

    #[test]
    fn test_validate_action_script_cmd_and_source_rejected() {
        // cmd is not valid on type: script at all
        let yaml = r#"
actions:
  my-action:
    type: script
    cmd: "echo hello"
    source: "deploy.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "cmd on type: script should be rejected");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not valid on type: script"),
            "Error should mention 'not valid on type: script'"
        );
    }

    #[test]
    fn test_validate_action_script_all_three_fields_rejected() {
        // cmd is not valid on type: script — error fires before checking script/source
        let yaml = r#"
actions:
  my-action:
    type: script
    cmd: "echo hello"
    script: "echo world"
    source: "deploy.sh"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "cmd on type: script should be rejected");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not valid on type: script"),
            "Error should mention 'not valid on type: script'"
        );
    }

    #[test]
    fn test_validate_action_script_empty_script_field() {
        let yaml = r#"
actions:
  my-action:
    type: script
    script: ""
tasks:
  test-task:
    flow:
      step1:
        action: my-action
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Empty 'script' field should be rejected");
        assert!(
            result.unwrap_err().to_string().contains("empty 'script'"),
            "Error should mention empty 'script'"
        );
    }

    #[test]
    fn test_validate_action_script_empty_source_field() {
        let yaml = r#"
actions:
  my-action:
    type: script
    source: ""
tasks:
  test-task:
    flow:
      step1:
        action: my-action
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Empty 'source' field should be rejected");
        assert!(
            result.unwrap_err().to_string().contains("empty 'source'"),
            "Error should mention empty 'source'"
        );
    }

    #[test]
    fn test_validate_action_script_empty_cmd_field() {
        // Any cmd on type: script is rejected — empty or not
        let yaml = r#"
actions:
  my-action:
    type: script
    cmd: ""
tasks:
  test-task:
    flow:
      step1:
        action: my-action
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "'cmd' on type: script should be rejected");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not valid on type: script"),
            "Error should mention 'not valid on type: script'"
        );
    }

    #[test]
    fn test_validate_task_action_with_script_rejected() {
        // type:task with a script field — should fail since script is not allowed on task actions
        let yaml = r#"
actions:
  run-child:
    type: task
    task: some-task
    script: "echo hi"
tasks:
  some-task:
    flow:
      s:
        action: run-child
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_err(),
            "type:task with 'script' field should be rejected"
        );
        assert!(
            result.unwrap_err().to_string().contains("'script'"),
            "Error should mention 'script' field"
        );
    }

    #[test]
    fn test_validate_when_valid_template() {
        let yaml = r#"
            actions:
              check:
                type: script
                script: "echo ok"
            tasks:
              test-task:
                flow:
                  step1:
                    action: check
                    when: "{{ input.deploy }}"
        "#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Valid when expression should pass: {:?}",
            result
        );
    }

    #[test]
    fn test_validate_when_invalid_template_syntax() {
        let yaml = r#"
            actions:
              check:
                type: script
                script: "echo ok"
            tasks:
              test-task:
                flow:
                  step1:
                    action: check
                    when: "{% if %}"
        "#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Invalid when syntax should fail");
        assert!(result.unwrap_err().to_string().contains("when expression"));
    }

    #[test]
    fn test_validate_when_with_tera_logic() {
        let yaml = r#"
            actions:
              check:
                type: script
                script: "echo ok"
            tasks:
              test-task:
                flow:
                  step1:
                    action: check
                    when: "{% if input.env == \"production\" %}true{% endif %}"
        "#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Tera logic in when should pass: {:?}",
            result
        );
    }

    // --- for_each validation tests ---

    #[test]
    fn test_for_each_string_expression_valid() {
        let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      fetch:
        action: process
      loop-step:
        action: process
        depends_on: [fetch]
        for_each: "{{ fetch.output.items }}"
        input:
          item: "{{ each.item }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Valid for_each string should pass: {:?}",
            result
        );
    }

    #[test]
    fn test_for_each_literal_array_valid() {
        let yaml = r#"
actions:
  deploy:
    type: script
    script: echo deploy
tasks:
  main:
    flow:
      deploy:
        action: deploy
        for_each: ["us-east-1", "eu-west-1", "ap-south-1"]
        input:
          region: "{{ each.item }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Valid for_each array should pass: {:?}",
            result
        );
    }

    #[test]
    fn test_for_each_invalid_type_rejected() {
        let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      step:
        action: process
        for_each: 42
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "for_each: 42 should be rejected");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("for_each must be a string"),
            "Error should mention 'for_each must be a string'"
        );
    }

    #[test]
    fn test_for_each_invalid_tera_syntax() {
        let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      step:
        action: process
        for_each: "{% if %}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_err(),
            "Invalid Tera syntax in for_each should be rejected"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid for_each expression"),
            "Error should mention 'invalid for_each expression'"
        );
    }

    #[test]
    fn test_step_name_with_brackets_rejected() {
        let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      "step[0]":
        action: process
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Step name with '[' should be rejected");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must not contain '['"),
            "Error should mention that '[' is not allowed"
        );
    }

    #[test]
    fn test_sequential_without_for_each_warns() {
        let yaml = r#"
actions:
  process:
    type: script
    script: echo hello
tasks:
  main:
    flow:
      step:
        action: process
        sequential: true
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = validate_workflow_config(&config).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("sequential") && w.contains("no for_each")),
            "Expected warning about sequential without for_each, got: {:?}",
            warnings
        );
    }

    #[test]
    fn test_for_each_with_sequential() {
        let yaml = r#"
actions:
  migrate:
    type: script
    script: echo migrate
tasks:
  main:
    flow:
      migrate:
        action: migrate
        for_each: ["schema1", "schema2"]
        sequential: true
        input:
          schema: "{{ each.item }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "for_each with sequential should be valid: {:?}",
            result
        );
        assert!(result.unwrap().is_empty(), "Expected no warnings");
    }

    // --- agent action validation tests ---

    #[test]
    fn test_validate_agent_action_valid() {
        let yaml = r#"
actions:
  summarise:
    type: agent
    provider: openai
    prompt: "Summarise {{ input.text }}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Valid agent action should pass: {:?}",
            result
        );
    }

    #[test]
    fn test_validate_agent_action_with_all_optional_fields() {
        let yaml = r#"
actions:
  analyse:
    type: agent
    provider: anthropic
    model: claude-opus-4-5
    system_prompt: "You are a helpful assistant."
    prompt: "Analyse {{ input.data }}"
    temperature: 0.7
    max_tokens: 2048
    max_turns: 5
    output:
      result:
        type: string
    tools:
      - task: run-tool
      - mcp: my-mcp-server
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Agent action with all optional fields should pass: {:?}",
            result
        );
    }

    #[test]
    fn test_validate_agent_action_missing_provider() {
        let yaml = r#"
actions:
  bad:
    type: agent
    prompt: "Do something"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Missing provider should fail");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing 'provider'"),
            "Error should mention missing 'provider'"
        );
    }

    #[test]
    fn test_validate_agent_action_missing_prompt() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Missing prompt should fail");
        assert!(
            result.unwrap_err().to_string().contains("missing 'prompt'"),
            "Error should mention missing 'prompt'"
        );
    }

    #[test]
    fn test_validate_agent_action_rejects_script_field() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    script: "echo hi"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Agent with 'script' should fail");
        assert!(result.unwrap_err().to_string().contains("'script' field"));
    }

    #[test]
    fn test_validate_agent_action_rejects_image_field() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    image: "my-image:latest"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Agent with 'image' should fail");
        assert!(result.unwrap_err().to_string().contains("'image' field"));
    }

    #[test]
    fn test_validate_agent_action_rejects_task_field() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    task: some-task
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Agent with 'task' field should fail");
        assert!(result.unwrap_err().to_string().contains("'task' field"));
    }

    #[test]
    fn test_validate_agent_action_temperature_out_of_range() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    temperature: 2.5
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Temperature > 2.0 should fail");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("temperature must be between"),
            "Error should mention temperature range"
        );
    }

    #[test]
    fn test_validate_agent_action_temperature_zero_valid() {
        let yaml = r#"
actions:
  deterministic:
    type: agent
    provider: openai
    prompt: "Classify {{ input.text }}"
    temperature: 0.0
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Temperature 0.0 should be valid: {:?}",
            result
        );
    }

    #[test]
    fn test_validate_agent_action_temperature_max_valid() {
        let yaml = r#"
actions:
  creative:
    type: agent
    provider: openai
    prompt: "Write a poem about {{ input.topic }}"
    temperature: 2.0
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_ok(),
            "Temperature 2.0 should be valid: {:?}",
            result
        );
    }

    #[test]
    fn test_validate_agent_action_output_invalid_field_type() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    output:
      result:
        type: invalid_type
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err(), "Invalid output field type should fail");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid type 'invalid_type'"),
            "Error should mention invalid type"
        );
    }

    #[test]
    fn test_validate_agent_action_output_rejects_text_type() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "Do something"
    output:
      result:
        type: text
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_err(),
            "type: text must be rejected for output fields"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid type 'text'"), "got: {}", msg);
    }

    #[test]
    fn test_validate_script_action_output_invalid_type_rejected() {
        let yaml = r#"
actions:
  export:
    type: script
    script: "echo done"
    output:
      result:
        type: invalid_type
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_err(),
            "script action with invalid output type should now fail"
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid type 'invalid_type'"));
    }

    #[test]
    fn test_validate_agent_action_invalid_prompt_template() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    prompt: "{% if %}"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_err(),
            "Invalid prompt template syntax should fail"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid prompt template"),
            "Error should mention invalid prompt template"
        );
    }

    #[test]
    fn test_validate_agent_action_invalid_system_prompt_template() {
        let yaml = r#"
actions:
  bad:
    type: agent
    provider: openai
    system_prompt: "{% if %}"
    prompt: "Do something"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(
            result.is_err(),
            "Invalid system_prompt template syntax should fail"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid system_prompt template"),
            "Error should mention invalid system_prompt template"
        );
    }

    #[test]
    fn test_compute_required_tags_agent() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: agent
provider: openai
prompt: "Do something"
"#,
        )
        .unwrap();
        // Agent steps now require a worker with the "agent" tag
        assert_eq!(compute_required_tags(&action), vec!["agent".to_string()]);
    }

    #[test]
    fn test_compute_required_tags_agent_with_extra_tags() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: agent
provider: openai
prompt: "Do something"
tags: [gpu, custom]
"#,
        )
        .unwrap();
        // "agent" tag is always first, extra tags appended
        assert_eq!(
            compute_required_tags(&action),
            vec!["agent".to_string(), "gpu".to_string(), "custom".to_string()]
        );
    }

    #[test]
    fn test_derive_runner_agent() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: agent
provider: openai
prompt: "Do something"
"#,
        )
        .unwrap();
        assert_eq!(derive_runner(&action), "none");
    }

    // ---- approval action tests ----

    #[test]
    fn test_validate_approval_action_valid() {
        let yaml = r#"
actions:
  gate:
    type: approval
    message: "Please review the deployment before proceeding."

tasks:
  deploy:
    flow:
      gate:
        action: gate
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "expected ok but got: {:?}", result);
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_validate_approval_action_missing_message() {
        let yaml = r#"
actions:
  gate:
    type: approval
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing 'message'"),
            "expected missing message error"
        );
    }

    #[test]
    fn test_validate_approval_action_rejects_cmd() {
        let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    cmd: "echo bad"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'cmd' field"));
    }

    #[test]
    fn test_validate_approval_action_rejects_script() {
        let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    script: "echo bad"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'script' field"));
    }

    #[test]
    fn test_validate_approval_action_rejects_image() {
        let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    image: "ubuntu:22.04"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'image' field"));
    }

    #[test]
    fn test_validate_approval_action_rejects_runner() {
        let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    runner: docker
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'runner' field"));
    }

    #[test]
    fn test_validate_approval_action_rejects_task_field() {
        let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    task: other-task
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'task' field"));
    }

    #[test]
    fn test_validate_approval_action_rejects_provider() {
        let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"
    provider: openai
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("'provider' field"));
    }

    #[test]
    fn test_compute_required_tags_approval() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: approval
message: "Please review"
"#,
        )
        .unwrap();
        assert_eq!(compute_required_tags(&action), Vec::<String>::new());
    }

    #[test]
    fn test_derive_runner_approval() {
        use crate::models::workflow::ActionDef;
        let action: ActionDef = serde_yaml::from_str(
            r#"
type: approval
message: "Please review"
"#,
        )
        .unwrap();
        assert_eq!(derive_runner(&action), "none");
    }

    #[test]
    fn test_validate_approval_inline_in_flow() {
        let yaml = r#"
tasks:
  deploy:
    flow:
      gate:
        type: approval
        message: "Approve the deployment to production?"
      run:
        depends_on: [gate]
        type: script
        script: "echo deploying"
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "expected ok but got: {:?}", result);
    }

    #[test]
    fn test_validate_on_suspended_hook_valid() {
        let yaml = r#"
actions:
  notify:
    type: script
    script: "echo notifying"
  gate:
    type: approval
    message: "Approve?"

tasks:
  deploy:
    on_suspended:
      - action: notify
    flow:
      gate:
        action: gate
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "expected ok but got: {:?}", result);
    }

    #[test]
    fn test_validate_on_suspended_hook_missing_action() {
        let yaml = r#"
actions:
  gate:
    type: approval
    message: "Approve?"

tasks:
  deploy:
    on_suspended:
      - action: nonexistent_action
    flow:
      gate:
        action: gate
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("nonexistent_action"),
            "expected missing action error"
        );
    }

    #[test]
    fn test_validate_workspace_on_suspended_hook_valid() {
        let yaml = r#"
actions:
  notify:
    type: script
    script: "echo notifying"
  gate:
    type: approval
    message: "Approve?"

on_suspended:
  - action: notify

tasks:
  deploy:
    flow:
      gate:
        action: gate
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "expected ok but got: {:?}", result);
    }

    #[test]
    fn test_validate_workspace_on_suspended_hook_missing_action() {
        let yaml = r#"
on_suspended:
  - action: does_not_exist
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("does_not_exist"),
            "expected missing action error"
        );
    }

    // ─── Helpers for multi-turn agent and MCP validation tests ───────────────

    /// Create a minimal valid agent ActionDef and apply a mutation closure.
    fn make_agent_action_with(
        f: impl FnOnce(&mut crate::models::workflow::ActionDef),
    ) -> crate::models::workflow::ActionDef {
        let mut action: crate::models::workflow::ActionDef = serde_yaml::from_str(
            r#"
type: agent
provider: anthropic
prompt: "Do something useful"
"#,
        )
        .unwrap();
        f(&mut action);
        action
    }

    /// Wrap a single action into a minimal WorkflowConfig.
    fn make_config_with_action(
        name: &str,
        action: crate::models::workflow::ActionDef,
    ) -> WorkflowConfig {
        let mut config = WorkflowConfig::default();
        config.actions.insert(name.to_string(), action);
        config
    }

    // ─── Multi-turn agent tool validation ────────────────────────────────

    #[test]
    fn test_validate_agent_action_max_turns_zero() {
        let action = make_agent_action_with(|a| {
            a.max_turns = Some(0);
            a.tools = vec![AgentToolRef::Task { task: "t".into() }];
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("max_turns must be between 1 and 100"));
    }

    #[test]
    fn test_validate_agent_action_max_turns_101() {
        let action = make_agent_action_with(|a| {
            a.max_turns = Some(101);
            a.tools = vec![AgentToolRef::Task { task: "t".into() }];
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("max_turns must be between 1 and 100"));
    }

    #[test]
    fn test_validate_agent_action_max_turns_valid_boundaries() {
        // max_turns=1 with tools should be valid
        let action = make_agent_action_with(|a| {
            a.max_turns = Some(1);
            a.tools = vec![AgentToolRef::Task { task: "t".into() }];
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_ok());

        // max_turns=100 with tools should be valid
        let action = make_agent_action_with(|a| {
            a.max_turns = Some(100);
            a.tools = vec![AgentToolRef::Task { task: "t".into() }];
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_agent_action_max_turns_no_tools_warns() {
        let action = make_agent_action_with(|a| {
            a.max_turns = Some(5);
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.contains("max_turns") && w.contains("no tools")));
    }

    #[test]
    fn test_validate_agent_action_interactive_no_tools_warns() {
        let action = make_agent_action_with(|a| {
            a.interactive = true;
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.contains("interactive=true") && w.contains("no tools")));
    }

    #[test]
    fn test_validate_agent_action_interactive_no_max_turns_warns() {
        let action = make_agent_action_with(|a| {
            a.interactive = true;
            a.tools = vec![AgentToolRef::Task { task: "t".into() }];
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert!(warnings
            .iter()
            .any(|w| w.contains("interactive=true") && w.contains("no max_turns")));
    }

    #[test]
    fn test_validate_agent_action_empty_task_ref() {
        let action = make_agent_action_with(|a| {
            a.tools = vec![AgentToolRef::Task { task: "".into() }];
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("empty task reference"));
    }

    #[test]
    fn test_validate_agent_action_empty_mcp_ref() {
        let action = make_agent_action_with(|a| {
            a.tools = vec![AgentToolRef::Mcp { mcp: "".into() }];
        });
        let result = validate_workflow_config(&make_config_with_action("a", action));
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("empty MCP server reference"));
    }

    #[test]
    fn test_validate_mcp_servers_empty() {
        let result = validate_mcp_servers(&std::collections::HashMap::new());
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_validate_mcp_servers_stdio_missing_command() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "test".to_string(),
            McpServerDef {
                transport: "stdio".to_string(),
                command: None,
                args: None,
                url: None,
                auth_token: None,
                env: Default::default(),
                timeout_secs: None,
            },
        );
        let result = validate_mcp_servers(&servers);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("missing 'command'"));
    }

    #[test]
    fn test_validate_mcp_servers_stdio_has_url() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "test".to_string(),
            McpServerDef {
                transport: "stdio".to_string(),
                command: Some("npx".to_string()),
                args: None,
                url: Some("http://localhost".to_string()),
                auth_token: None,
                env: Default::default(),
                timeout_secs: None,
            },
        );
        let result = validate_mcp_servers(&servers);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("has 'url' field"));
    }

    #[test]
    fn test_validate_mcp_servers_sse_missing_url() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "test".to_string(),
            McpServerDef {
                transport: "sse".to_string(),
                command: None,
                args: None,
                url: None,
                auth_token: None,
                env: Default::default(),
                timeout_secs: None,
            },
        );
        let result = validate_mcp_servers(&servers);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("missing 'url'"));
    }

    #[test]
    fn test_validate_mcp_servers_unknown_transport() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "test".to_string(),
            McpServerDef {
                transport: "grpc".to_string(),
                command: None,
                args: None,
                url: None,
                auth_token: None,
                env: Default::default(),
                timeout_secs: None,
            },
        );
        let result = validate_mcp_servers(&servers);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("unknown transport type"));
    }

    #[test]
    fn test_task_name_collision_hyphen_underscore() {
        let yaml = r#"
actions:
  deploy:
    type: script
    script: echo deploy
tasks:
  deploy-app:
    flow:
      s1:
        action: deploy
  deploy_app:
    flow:
      s1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("normalized name") && err.contains("deploy_app"),
            "Error: {}",
            err
        );
    }

    #[test]
    fn test_task_name_no_collision_when_different() {
        let yaml = r#"
actions:
  deploy:
    type: script
    script: echo deploy
tasks:
  deploy-app:
    flow:
      s1:
        action: deploy
  deploy-service:
    flow:
      s1:
        action: deploy
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_ok(), "Expected ok, got: {:?}", result.err());
    }

    #[test]
    fn test_mcp_server_name_collision_hyphen_underscore() {
        let yaml = r#"
actions: {}
tasks: {}
mcp_servers:
  my-server:
    transport: stdio
    command: echo
  my_server:
    transport: stdio
    command: echo
"#;
        let config: WorkflowConfig = serde_yaml::from_str(yaml).unwrap();
        let result = validate_workflow_config(&config);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("normalized name") && err.contains("my_server"),
            "Error: {}",
            err
        );
    }
}
