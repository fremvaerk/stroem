use crate::dag;
use crate::git_ref::{classify_ref, RefKind};
use crate::models::workflow::{ActionDef, ConnectionTypeDef, HookDef, TaskDef, WorkspaceConfig};
use anyhow::{bail, Result};
use std::collections::HashMap;

/// Maximum allowed `FlowStep.timeout` (24h, in seconds).
///
/// Also the upper bound for `ServerConfig.default_step_timeout`. Mirrored in
/// the server-side config validation so the two checks can't drift.
pub const MAX_STEP_TIMEOUT_SECS: u64 = 86_400;

/// Maximum allowed `TaskDef.timeout` (7d, in seconds).
///
/// Also the upper bound for `ServerConfig.default_job_timeout`.
pub const MAX_JOB_TIMEOUT_SECS: u64 = 604_800;

/// Resolves cross-workspace references for server-side validation:
/// `(workspace, name) -> exists`. The server implements it over its loaded
/// workspace configs; the CLI has no resolver and warns instead.
pub trait CrossWorkspaceResolver {
    fn has_action(&self, workspace: &str, action: &str) -> bool;
    fn has_task(&self, workspace: &str, task: &str) -> bool;
}

/// Validates a workflow config and returns list of warnings.
/// Errors are returned as Err.
///
/// When `libraries_resolved` is true, library references (names containing `.`)
/// are validated against the config (they should already be merged in).
/// When false (CLI validation without server context), library references
/// are skipped with a warning.
pub fn validate_workflow_config(config: &WorkspaceConfig) -> Result<Vec<String>> {
    validate_workflow_config_inner(config, false, None)
}

/// Validates a workflow config with full library validation.
/// Use this after libraries have been resolved and merged into the config.
pub fn validate_workflow_config_with_libraries(config: &WorkspaceConfig) -> Result<Vec<String>> {
    validate_workflow_config_inner(config, true, None)
}

/// Validates a workflow config with full library validation, plus resolution of
/// cross-workspace action references (`owner_ws.action` in a flow step's `action`
/// field).
///
/// `resolver` should report `true` when `workspace` exists and has an action
/// (or task) with the given name. Intended for the server layer, which has
/// access to all loaded workspace configs — mirrors how library-prefixed
/// action keys are only accepted once merged into `config.actions`.
pub fn validate_workflow_config_with_cross_workspace_resolver(
    config: &WorkspaceConfig,
    resolver: &dyn CrossWorkspaceResolver,
) -> Result<Vec<String>> {
    validate_workflow_config_inner(config, true, Some(resolver))
}

/// Validates the dependency-shape rules for a single task's flow: the legacy
/// `continue_when_skipped` flag (removed in 0.18.0, a named error from this
/// function's callers below rather than silent acceptance-and-ignore — not
/// a parse error, and not caught by merely loading the workspace) and
/// `depends_on` tree shape
/// (duplicate siblings, empty groups, empty `accept` lists via
/// [`crate::depends_on::validate_tree`]).
///
/// Shared by the full-workspace validator above (every task) and by
/// job-creation-time / `stroem run` prechecks (just the triggered task),
/// which is why it's `pub`: those callers are not positioned to run the rest
/// of [`validate_workflow_config`] (it also validates actions, cross-task
/// self-references, and every other task's flow — `stroem-server`'s
/// job-creation path and `stroem run`'s execution path only ever resolve one
/// task at a time, and intentionally don't recurse into `type: task`
/// children, matching `job_creator.rs`'s existing literal-connection
/// prechecks' scope).
pub fn validate_task_dependency_shape(task_name: &str, task: &TaskDef) -> Result<()> {
    for (step_name, step) in &task.flow {
        // Legacy flag detection — a named, actionable error, not a generic "unknown field."
        if step.legacy_continue_when_skipped.is_some() {
            bail!(
                "Task '{}' step '{}': continue_when_skipped was removed in 0.18.0; \
                 see the 0.18 upgrade guide to choose the right `accept` set for \
                 this step's dependents — it depends on what else was present on \
                 this dependency.",
                task_name,
                step_name
            );
        }

        // Tree-shape validation (duplicate siblings, empty groups, empty accept lists).
        if let Err(tree_errors) = crate::depends_on::validate_tree(&step.depends_on) {
            bail!(
                "Task '{}' step '{}': {}",
                task_name,
                step_name,
                tree_errors.join("; ")
            );
        }
    }
    Ok(())
}

fn validate_workflow_config_inner(
    config: &WorkspaceConfig,
    libraries_resolved: bool,
    resolver: Option<&dyn CrossWorkspaceResolver>,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    // Validate each action
    for (action_name, action) in &config.actions {
        if let Some(r) = action.git_ref.as_deref() {
            if action.action_type != "task" {
                bail!(
                    "Action '{}' has `ref` but is type '{}': `ref` is only valid on type: task actions, where it qualifies `task`",
                    action_name,
                    action.action_type
                );
            }
            let label = format!("Action '{action_name}'");
            match classify_ref(r) {
                RefKind::Static => check_git_ref(&label, r)?,
                RefKind::Template => check_ref_template(&label, r)?,
            }
        }
        warnings.extend(validate_action(action_name, action)?);

        // For type: task, verify the referenced task exists — locally (incl.
        // library-flattened keys), or in another workspace via the resolver.
        if action.action_type == "task" {
            let task_ref = action.task.as_ref().expect(
                "task field is required for action_type == task, enforced by validate_action",
            );
            if let Some(r) = action.git_ref.as_deref() {
                let (at, when) = match classify_ref(r) {
                    RefKind::Static => (format!("at ref '{r}'"), "resolved at job creation"),
                    RefKind::Template => (
                        "at a templated ref".to_string(),
                        "resolved when the step starts",
                    ),
                };
                warnings.push(format!(
                    "Action '{}' references task '{}' {} - validation skipped ({})",
                    action_name, task_ref, at, when
                ));
            } else if config.tasks.contains_key(task_ref) {
                // local or library-flattened key
            } else if task_ref.starts_with('.') || task_ref.ends_with('.') {
                bail!(
                    "Action '{}' references malformed task '{}' (empty workspace or task name)",
                    action_name,
                    task_ref
                );
            } else if let (Some(ws), name) = crate::template::parse_qualified_ref(task_ref) {
                match (libraries_resolved, resolver) {
                    (false, _) => warnings.push(format!(
                        "Action '{}' references task '{}' outside this workspace - cannot validate cross-workspace task reference offline",
                        action_name, task_ref
                    )),
                    (true, Some(r)) if r.has_task(ws, name) => {}
                    (true, Some(_)) => bail!(
                        "action '{}': workspace '{}' has no task '{}'",
                        action_name,
                        ws,
                        name
                    ),
                    (true, None) => bail!(
                        "Action '{}' references non-existent task '{}'",
                        action_name,
                        task_ref
                    ),
                }
            } else {
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
        for (i, hook) in task.on_cancel.iter().enumerate() {
            check_task_self_reference(config, task_name, &hook.action, &format!("on_cancel[{i}]"))?;
        }
    }

    // Validate each task
    for (task_name, task) in &config.tasks {
        validate_task_dependency_shape(task_name, task)?;

        // Validate that flow steps reference existing actions
        for (step_name, step) in &task.flow {
            let action_ref = &step.action;

            // Check if it's a library action (contains .)
            if let Some(r) = step.git_ref.as_deref() {
                let label = format!("Task '{task_name}' step '{step_name}'");
                let at = match classify_ref(r) {
                    RefKind::Static => {
                        check_git_ref(&label, r)?;
                        format!("at ref '{r}' - validation skipped (resolved at job creation)")
                    }
                    RefKind::Template => {
                        check_ref_template(&label, r)?;
                        "at a templated ref - validation skipped (resolved when the step starts)"
                            .to_string()
                    }
                };
                warnings.push(format!(
                    "Task '{}' step '{}' references action '{}' {}",
                    task_name, step_name, action_ref, at
                ));
            } else if !libraries_resolved && action_ref.contains('.') {
                // Library actions can't be validated without server context
                warnings.push(format!(
                    "Task '{}' step '{}' references library action '{}' - validation skipped",
                    task_name, step_name, action_ref
                ));
            } else if !config.actions.contains_key(action_ref) {
                // Not a known local/library key. It may still be a valid
                // cross-workspace reference (`owner_ws.action`) — ask the resolver
                // if one was provided (server layer, with access to all workspaces).
                let (owner_ws, bare_action) = crate::template::parse_qualified_ref(action_ref);
                let cross_workspace_ok = match (owner_ws, resolver) {
                    (Some(ws), Some(resolver)) => {
                        if resolver.has_action(ws, bare_action) {
                            true
                        } else {
                            bail!(
                                "action '{}': workspace '{}' has no action '{}'",
                                action_ref,
                                ws,
                                bare_action
                            );
                        }
                    }
                    _ => false,
                };
                if !cross_workspace_ok {
                    bail!(
                        "Task '{}' step '{}' references non-existent action '{}'",
                        task_name,
                        step_name,
                        action_ref
                    );
                }
            }

            // Validate depends_on references — walks the whole tree, not just
            // the top level.
            let mut referenced = Vec::new();
            crate::depends_on::collect_names(&step.depends_on, &mut referenced);
            for dep in referenced {
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
            // `check_template_syntax` compiles the expression on the rendering
            // engine and runs it against an empty context: a compile error is
            // a validation error, a runtime error (undefined variable) is not.
            //
            // Known limitation — variable references: we cannot validate which
            // step names are referenced in the expression at parse time, because
            // the full step output context is only built at runtime by the
            // step cascade. A typo like `{{ step_nme.output.value }}`
            // (missing 'a') passes this check and surfaces as a condition
            // evaluation failure when the job runs.
            //
            // Unknown filters, tests and functions are rejected here: Tera 2
            // checks every reference when the template compiles, including
            // branches that never run (spec 2026-10-06 § 3.1).
            if let Some(ref when_expr) = step.when {
                if let Err(e) = crate::template::check_template_syntax(when_expr) {
                    return Err(anyhow::Error::new(e).context(format!(
                        "Task '{}' step '{}' has an invalid when expression",
                        task_name, step_name
                    )));
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
                        // Validate as Tera template (same check as `when`)
                        if let Err(e) = crate::template::check_template_syntax(expr) {
                            return Err(anyhow::Error::new(e).context(format!(
                                "Task '{}' step '{}' has an invalid for_each expression",
                                task_name, step_name
                            )));
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
                    // Name the JSON type only: a YAML literal is template
                    // source and can hold a literal secret (spec 2026-10-06
                    // § 3.3).
                    other => {
                        bail!(
                            "Task '{}' step '{}' field 'for_each' must be a string (Tera template) or array, got a JSON {}",
                            task_name,
                            step_name,
                            crate::template::json_type_name(other)
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

            // Validate step timeout (cap is shared with ServerConfig.default_step_timeout)
            if let Some(ref timeout) = step.timeout {
                if timeout.as_secs() > MAX_STEP_TIMEOUT_SECS {
                    bail!(
                        "Task '{}' step '{}' timeout {}s exceeds maximum of {}s (24h)",
                        task_name,
                        step_name,
                        timeout.as_secs(),
                        MAX_STEP_TIMEOUT_SECS
                    );
                }
            }

            // Validate step retry config
            if let Some(ref retry) = step.retry {
                validate_retry_config(
                    retry,
                    &format!("Task '{}' step '{}'", task_name, step_name),
                )?;
                // Warn if the referenced action type doesn't benefit from retry
                if let Some(action_def) = config.actions.get(&step.action) {
                    if matches!(action_def.action_type.as_str(), "approval" | "task") {
                        warnings.push(format!(
                            "Task '{}' step '{}' has retry on action type '{}' — retry is not meaningful for this type",
                            task_name, step_name, action_def.action_type
                        ));
                    }
                }
            }
        }

        // Validate task/job timeout (cap is shared with ServerConfig.default_job_timeout)
        if let Some(ref timeout) = task.timeout {
            if timeout.as_secs() > MAX_JOB_TIMEOUT_SECS {
                bail!(
                    "Task '{}' timeout {}s exceeds maximum of {}s (7d)",
                    task_name,
                    timeout.as_secs(),
                    MAX_JOB_TIMEOUT_SECS
                );
            }
        }

        // Validate task retry config
        if let Some(ref retry) = task.retry {
            validate_retry_config(retry, &format!("Task '{}'", task_name))?;
        }

        // Validate DAG (no cycles)
        dag::validate_dag(&task.flow)
            .map_err(|e| anyhow::anyhow!("Task '{}' has invalid DAG: {}", task_name, e))?;

        // Validate hooks reference existing actions
        for (i, hook) in task.on_success.iter().enumerate() {
            validate_hook_action_exists(
                &format!("Task '{task_name}' on_success[{i}]"),
                hook,
                config,
                libraries_resolved,
                &mut warnings,
            )?;
        }
        for (i, hook) in task.on_error.iter().enumerate() {
            validate_hook_action_exists(
                &format!("Task '{task_name}' on_error[{i}]"),
                hook,
                config,
                libraries_resolved,
                &mut warnings,
            )?;
        }
        for (i, hook) in task.on_suspended.iter().enumerate() {
            validate_hook_action_exists(
                &format!("Task '{task_name}' on_suspended[{i}]"),
                hook,
                config,
                libraries_resolved,
                &mut warnings,
            )?;
        }
        for (i, hook) in task.on_cancel.iter().enumerate() {
            validate_hook_action_exists(
                &format!("Task '{task_name}' on_cancel[{i}]"),
                hook,
                config,
                libraries_resolved,
                &mut warnings,
            )?;
        }
    }

    // Validate triggers reference existing tasks
    for (trigger_name, trigger) in &config.triggers {
        // EventSource validates both consumer and target tasks (and rejects
        // `ref`) in its own match arm below.
        if !matches!(
            trigger,
            crate::models::workflow::TriggerDef::EventSource { .. }
        ) {
            validate_trigger_task(
                trigger_name,
                trigger.task(),
                trigger.git_ref(),
                config,
                libraries_resolved,
                resolver,
                &mut warnings,
                matches!(trigger, crate::models::workflow::TriggerDef::Webhook { .. }),
            )?;
        }

        match trigger {
            crate::models::workflow::TriggerDef::Scheduler { cron, timezone, .. } => {
                // Validate cron expression syntax
                if crate::cron::parse(cron).is_err() {
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
            crate::models::workflow::TriggerDef::EventSource {
                task,
                target_task,
                enabled: _,
                input: _,
                env: _,
                restart_policy: _,
                backoff_secs,
                max_in_flight,
                git_ref,
            } => {
                if git_ref.is_some() {
                    bail!(
                        "Trigger '{}': `ref` is not supported on event_source triggers yet",
                        trigger_name
                    );
                }
                // Validate consumer task exists
                if task.contains('.') {
                    // Library reference — skip validation
                } else if !config.tasks.contains_key(task) {
                    bail!(
                        "Trigger '{}' references non-existent consumer task '{}'",
                        trigger_name,
                        task
                    );
                }

                // Validate target task exists
                if target_task.contains('.') {
                    // Library reference — skip validation
                } else if !config.tasks.contains_key(target_task) {
                    bail!(
                        "Trigger '{}' references non-existent target task '{}'",
                        trigger_name,
                        target_task
                    );
                }

                // Consumer and target must be different
                if task == target_task {
                    bail!(
                        "Trigger '{}': consumer task and target_task cannot be the same ('{}')",
                        trigger_name,
                        task
                    );
                }

                // Validate backoff
                if *backoff_secs < 1 {
                    bail!(
                        "Trigger '{}' has backoff_secs < 1 (got {})",
                        trigger_name,
                        backoff_secs
                    );
                }

                // Validate max_in_flight
                if let Some(mif) = max_in_flight {
                    if *mif < 1 {
                        bail!(
                            "Trigger '{}' has max_in_flight < 1 (got {})",
                            trigger_name,
                            mif
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
            hook,
            config,
            libraries_resolved,
            &mut warnings,
        )?;
    }
    for (i, hook) in config.on_error.iter().enumerate() {
        validate_hook_action_exists(
            &format!("Workspace on_error[{i}]"),
            hook,
            config,
            libraries_resolved,
            &mut warnings,
        )?;
    }
    for (i, hook) in config.on_suspended.iter().enumerate() {
        validate_hook_action_exists(
            &format!("Workspace on_suspended[{i}]"),
            hook,
            config,
            libraries_resolved,
            &mut warnings,
        )?;
    }
    for (i, hook) in config.on_cancel.iter().enumerate() {
        validate_hook_action_exists(
            &format!("Workspace on_cancel[{i}]"),
            hook,
            config,
            libraries_resolved,
            &mut warnings,
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
    warnings.extend(validate_input_options(config)?);

    // json field templates (spec 2026-10-06-json-input-type § 5.4)
    warnings.extend(validate_json_fields(config)?);

    // Secrets redaction cannot mask (spec D7)
    warnings.extend(warn_non_string_secrets(config));

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

/// Check one connection's values against its type: required properties present
/// (unless the type supplies a default), unknown fields (warning), empty
/// strings (error). `label` is a full noun phrase (`Connection 'db'`, `input
/// field 'db'`) and never a rendered value. Shared by load-time validation and by the resolver for
/// connections whose type lives in another workspace.
pub fn check_connection_values(
    label: &str,
    values: &HashMap<String, serde_json::Value>,
    type_name: &str,
    type_def: &ConnectionTypeDef,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for (prop_name, prop_def) in &type_def.properties {
        if prop_def.required && prop_def.default.is_none() && !values.contains_key(prop_name) {
            bail!("{label} is missing required field '{prop_name}' (type '{type_name}')");
        }
    }
    for key in values.keys() {
        if !type_def.properties.contains_key(key) {
            warnings.push(format!(
                "{label} has field '{key}' not defined in type '{type_name}'"
            ));
        }
    }
    for (key, value) in values {
        if value.as_str().is_some_and(str::is_empty) {
            bail!("{label} field '{key}' has an empty value");
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
fn validate_connections(config: &WorkspaceConfig) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let valid_prop_types = [
        "string", "text", "integer", "number", "boolean", "date", "datetime",
    ];
    // Connection-type names that would collide with a primitive input-field
    // type (or one of its aliases — see canonicalize_field_type). A task
    // input written as `type: <name>` is resolved as a primitive first; if a
    // connection type shadows that name, the reference would silently miss
    // the connection registry.
    let reserved_type_names = crate::template::RESERVED_TYPE_NAMES;

    // Validate connection type definitions
    for (type_name, type_def) in &config.connection_types {
        if reserved_type_names.contains(&type_name.as_str()) {
            bail!(
                "Connection type name '{}' is reserved (collides with a primitive input-field type or alias)",
                type_name
            );
        }
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
                warnings.extend(check_connection_values(
                    &format!("Connection '{}'", conn_name),
                    &conn.values,
                    type_name,
                    type_def,
                )?);
                continue;
            }
            if type_name.contains('.') {
                warnings.push(format!(
                    "Connection '{}': type '{}' is a cross-workspace reference; validated at job creation",
                    conn_name, type_name
                ));
                // Still reject empty strings below.
            } else {
                bail!(
                    "Connection '{}' references non-existent connection type '{}'",
                    conn_name,
                    type_name
                );
            }
        }
        // Untyped, or typed-but-unresolved (cross-workspace): still reject empty strings.
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

/// `json` field templates (spec 2026-10-06-json-input-type § 5.4): a string
/// that is not literal or a single `{{ expression }}` is an error; a single
/// expression ending in `json_encode` is a warning.
fn validate_json_fields(config: &WorkspaceConfig) -> Result<Vec<String>> {
    use crate::template::JSON_TYPE;
    let mut warnings = Vec::new();
    for (action_name, action) in &config.actions {
        for (name, f) in &action.input {
            if let (true, Some(d)) = (f.field_type == JSON_TYPE, &f.default) {
                lint_json(
                    &format!("Action '{action_name}' input '{name}' default"),
                    d,
                    &mut warnings,
                )?;
            }
        }
    }
    for (task_name, task) in &config.tasks {
        for (name, f) in &task.input {
            if let (true, Some(d)) = (f.field_type == JSON_TYPE, &f.default) {
                lint_json(
                    &format!("Task '{task_name}' input '{name}' default"),
                    d,
                    &mut warnings,
                )?;
            }
        }
        for (step_name, step) in &task.flow {
            // A ref'd step resolves at job creation against another commit.
            if step.git_ref.is_some() {
                continue;
            }
            let Some(schema) = step_target_schema(config, step) else {
                continue;
            };
            for (key, value) in &step.input {
                if schema.get(key).is_some_and(|f| f.field_type == JSON_TYPE) {
                    lint_json(
                        &format!("Task '{task_name}' step '{step_name}' input '{key}'"),
                        value,
                        &mut warnings,
                    )?;
                }
            }
        }
    }
    Ok(warnings)
}

/// The schema a flow step's `input:` lands in when it resolves locally: the
/// action's input, or for a `type: task` action the task's (spec D6).
/// Library (dotted) and cross-workspace names do not resolve here.
fn step_target_schema<'a>(
    config: &'a WorkspaceConfig,
    step: &'a crate::models::workflow::FlowStep,
) -> Option<&'a HashMap<String, crate::models::workflow::InputFieldDef>> {
    let action = match &step.inline_action {
        Some(inline) => inline,
        None => config.actions.get(&step.action)?,
    };
    if action.action_type == "task" {
        // A ref'd task action targets another commit's task schema.
        if action.git_ref.is_some() {
            return None;
        }
        config.tasks.get(action.task.as_deref()?).map(|t| &t.input)
    } else {
        Some(&action.input)
    }
}

fn lint_json(context: &str, value: &serde_json::Value, warnings: &mut Vec<String>) -> Result<()> {
    let lint = crate::json_field::lint_json_value(value);
    if lint.mixed {
        bail!("{context}: a json field takes a literal value or a single {{{{ expression }}}}");
    }
    if lint.encodes_to_text {
        warnings.push(format!(
            "{context}: the expression ends in `json_encode`, so the field receives JSON text \
             (a string); drop `| json_encode()` to pass the value itself"
        ));
    }
    Ok(())
}

/// Secrets redaction can never mask (spec D7, § 5.4): redaction matches
/// strings only. Names the secret and the JSON type — never the value.
fn warn_non_string_secrets(config: &WorkspaceConfig) -> Vec<String> {
    fn kinds(v: &serde_json::Value, out: &mut Vec<&'static str>) {
        match v {
            serde_json::Value::String(_) => {}
            serde_json::Value::Number(_) => out.push("is a number"),
            serde_json::Value::Bool(_) => out.push("is a boolean"),
            serde_json::Value::Null => out.push("is null"),
            serde_json::Value::Array(a) => a.iter().for_each(|x| kinds(x, out)),
            serde_json::Value::Object(m) => m.values().for_each(|x| kinds(x, out)),
        }
    }
    const HINT: &str = "so it is never masked in job output or errors; quote it to have it masked (values of 3 characters or fewer are never masked)";
    let mut warnings = Vec::new();
    for (name, value) in &config.secrets {
        let mut found = Vec::new();
        kinds(value, &mut found);
        found.sort_unstable();
        found.dedup();
        for k in found {
            warnings.push(format!("secret '{name}' {k}, {HINT}"));
        }
    }
    for (conn_name, conn) in &config.connections {
        let Some(type_def) = conn
            .connection_type
            .as_ref()
            .and_then(|t| config.connection_types.get(t))
        else {
            continue;
        };
        for (prop, def) in &type_def.properties {
            if !def.secret {
                continue;
            }
            let Some(value) = conn.values.get(prop) else {
                continue;
            };
            let mut found = Vec::new();
            kinds(value, &mut found);
            found.sort_unstable();
            found.dedup();
            for k in found {
                warnings.push(format!(
                    "connection '{conn_name}' secret property '{prop}' {k}, {HINT}"
                ));
            }
        }
    }
    warnings
}

/// Validates input field options across all actions and tasks.
/// Returns warnings for soft issues; bails for unrecoverable misconfigurations
/// (e.g., `multiple: true` combined with `secret: true` or a non-string `type`).
fn validate_input_options(config: &WorkspaceConfig) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    // Check action-level inputs
    for (action_name, action) in &config.actions {
        for (field_name, field) in &action.input {
            check_input_field_options(
                &format!("Action '{action_name}' input '{field_name}'"),
                field,
                &mut warnings,
            )?;
        }
    }

    // Check task-level inputs
    for (task_name, task) in &config.tasks {
        for (field_name, field) in &task.input {
            check_input_field_options(
                &format!("Task '{task_name}' input '{field_name}'"),
                field,
                &mut warnings,
            )?;
        }
    }

    Ok(warnings)
}

fn check_input_field_options(
    context: &str,
    field: &crate::models::workflow::InputFieldDef,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if field.field_type == crate::template::JSON_TYPE {
        if field.secret {
            bail!("{context}: secret is not supported on json fields");
        }
        if field.multiple {
            bail!("{context}: multiple is not supported on json fields");
        }
        if field.options.is_some() {
            bail!("{context}: options are not supported on json fields");
        }
        if field.allow_custom {
            bail!("{context}: allow_custom is not supported on json fields");
        }
    }

    // Hard errors for `multiple: true` misuse — these have no working semantics.
    if field.multiple {
        if field.secret {
            bail!("{context}: secret and multiple cannot be combined");
        }
        // v1 only supports string-typed multi-select (UI renders string options only).
        if field.field_type != "string" && field.field_type != "text" {
            bail!(
                "{context}: multiple is only supported on string/text fields (got '{}')",
                field.field_type
            );
        }
    }

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
        // Check default value against options based on cardinality
        if let Some(ref default) = field.default {
            if field.multiple {
                match default.as_array() {
                    None => warnings.push(format!(
                        "{context}: multiple is true but default is not an array"
                    )),
                    Some(items) => {
                        let mut seen_default = std::collections::HashSet::new();
                        for item in items {
                            let Some(s) = item.as_str() else {
                                warnings.push(format!(
                                    "{context}: default array items must be strings"
                                ));
                                continue;
                            };
                            if !seen_default.insert(s) {
                                warnings.push(format!("{context}: duplicate default item '{s}'"));
                            }
                            if !field.allow_custom && !options.iter().any(|o| o == s) {
                                warnings.push(format!(
                                    "{context}: default item '{s}' is not in options list"
                                ));
                            }
                        }
                    }
                }
            } else if !field.allow_custom {
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
    } else {
        if field.allow_custom {
            warnings.push(format!(
                "{context}: allow_custom has no effect without options"
            ));
        }
        if field.multiple {
            warnings.push(format!("{context}: multiple requires options to be set"));
        }
    }

    Ok(())
}

/// Validates that task input fields referencing connection types have valid references.
///
/// For each task input where `field_type` is not a primitive type (string/integer/number/boolean),
/// it's treated as a connection type reference and must exist in `connection_types`.
fn validate_connection_inputs(config: &WorkspaceConfig) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let primitives = crate::template::PRIMITIVE_TYPES;

    fn check(
        config: &WorkspaceConfig,
        warnings: &mut Vec<String>,
        primitives: &[&str],
        scope: &str,
        owner: &str,
        field_name: &str,
        field_def: &crate::models::workflow::InputFieldDef,
    ) -> Result<()> {
        if primitives.contains(&field_def.field_type.as_str()) {
            return Ok(());
        }
        // Non-primitive type — must be a connection type reference
        if !config.connection_types.contains_key(&field_def.field_type) {
            if field_def.field_type.contains('.') {
                warnings.push(format!(
                    "{} '{}' input '{}': type '{}' is a cross-workspace reference; validated at job creation",
                    scope, owner, field_name, field_def.field_type
                ));
                return Ok(());
            }
            bail!(
                "{} '{}' input '{}' references unknown type '{}' (not a primitive or connection type)",
                scope, owner, field_name, field_def.field_type
            );
        }
        if field_def.multiple {
            // Defense in depth — check_input_field_options already rejects non-(string|text)
            // + multiple, but a non-primitive type here means a connection ref, which is
            // also unsupported with multiple.
            bail!(
                "{} '{}' input '{}': multiple is not supported on connection-type fields (type '{}')",
                scope, owner, field_name, field_def.field_type
            );
        }
        Ok(())
    }

    for (action_name, action) in &config.actions {
        for (field_name, field_def) in &action.input {
            check(
                config,
                &mut warnings,
                primitives,
                "Action",
                action_name,
                field_name,
                field_def,
            )?;
        }
    }

    for (task_name, task) in &config.tasks {
        for (field_name, field_def) in &task.input {
            check(
                config,
                &mut warnings,
                primitives,
                "Task",
                task_name,
                field_name,
                field_def,
            )?;
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
        serde_json::Value::String(s) if s.is_empty() => {
            bail!("Secret '{}' has an empty value", path);
        }
        _ => {
            // Allow other JSON value types (numbers, booleans) and non-empty strings
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
    config: &WorkspaceConfig,
    task_name: &str,
    action_name: &str,
    context: &str,
) -> Result<()> {
    if let Some(action) = config.actions.get(action_name) {
        if action.action_type == "task" && action.git_ref.is_none() {
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

/// Syntax check for a `ref:` value (spec 2026-10-02 § 4.2).
fn check_git_ref(label: &str, git_ref: &str) -> Result<()> {
    crate::git_ref::parse_git_ref(git_ref)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{label} has invalid `ref` '{git_ref}': {e}"))
}

/// A templated `ref:` (spec 2026-10-15 § 7): syntax only, on the ref engine —
/// it has no `vals`, so `| vals` is an unknown filter here.
fn check_ref_template(label: &str, template: &str) -> Result<()> {
    crate::template::check_ref_template_syntax(template).map_err(|e| {
        anyhow::Error::new(e).context(format!("{label} has an invalid `ref` template"))
    })
}

/// A trigger's `task` (scheduler / webhook). With `ref`, the task is looked up
/// at that ref by the server when the trigger fires, so it is not checked
/// here. Without `ref`, `ws.task` is a cross-workspace trigger task.
#[allow(clippy::too_many_arguments)]
fn validate_trigger_task(
    trigger_name: &str,
    task: &str,
    git_ref: Option<&str>,
    config: &WorkspaceConfig,
    libraries_resolved: bool,
    resolver: Option<&dyn CrossWorkspaceResolver>,
    warnings: &mut Vec<String>,
    allow_template: bool,
) -> Result<()> {
    if let Some(r) = git_ref {
        let label = format!("Trigger '{trigger_name}'");
        match classify_ref(r) {
            RefKind::Static => {
                check_git_ref(&label, r)?;
                warnings.push(format!(
                    "Trigger '{trigger_name}' targets task '{task}' at ref '{r}' - validation skipped (resolved when the trigger fires)"
                ));
            }
            RefKind::Template if allow_template => {
                check_ref_template(&label, r)?;
                warnings.push(format!(
                    "Trigger '{trigger_name}' targets task '{task}' at a templated ref - validation skipped (resolved when the webhook fires)"
                ));
            }
            RefKind::Template => bail!(
                "Trigger '{trigger_name}': a templated `ref` is only supported on webhook triggers"
            ),
        }
        return Ok(());
    }
    if config.tasks.contains_key(task) {
        return Ok(());
    }
    match crate::template::parse_qualified_ref(task) {
        (Some(ws), name) => match (libraries_resolved, resolver) {
            (false, _) => {
                warnings.push(format!(
                    "Trigger '{trigger_name}' targets task '{task}' outside this workspace - cannot validate cross-workspace task reference offline"
                ));
                Ok(())
            }
            (true, Some(r)) if r.has_task(ws, name) => Ok(()),
            (true, Some(_)) => bail!(
                "trigger '{}': workspace '{}' has no task '{}'",
                trigger_name,
                ws,
                name
            ),
            (true, None) => bail!(
                "Trigger '{}' references non-existent task '{}'",
                trigger_name,
                task
            ),
        },
        (None, _) => bail!(
            "Trigger '{}' references non-existent task '{}'",
            trigger_name,
            task
        ),
    }
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

    // Validate args entries are non-empty
    for (i, arg) in action.args.iter().enumerate() {
        if arg.is_empty() {
            bail!(
                "Action '{}' has empty args entry at index {}",
                action_name,
                i
            );
        }
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
    if !action.args.is_empty() {
        bail!(
            "Action '{}' is type 'docker' but has 'args' field (only valid for script actions; use cmd/command/entrypoint for docker)",
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
    if !action.args.is_empty() {
        bail!(
            "Action '{}' is type 'pod' but has 'args' field (only valid for script actions; use cmd/command/entrypoint for pod)",
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
    if !action.args.is_empty() {
        bail!(
            "Action '{}' is type 'task' but has 'args' field (only valid for script actions)",
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
    if !action.args.is_empty() {
        bail!(
            "Action '{}' is type 'agent' but has 'args' field (only valid for script actions)",
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

    // Validate prompt / system_prompt template syntax (same check as `when`)
    if let Some(ref prompt) = action.prompt {
        if let Err(e) = crate::template::check_template_syntax(prompt) {
            return Err(anyhow::Error::new(e).context(format!(
                "Action '{}' has an invalid prompt template",
                action_name
            )));
        }
    }
    if let Some(ref sp) = action.system_prompt {
        if let Err(e) = crate::template::check_template_syntax(sp) {
            return Err(anyhow::Error::new(e).context(format!(
                "Action '{}' has an invalid system_prompt template",
                action_name
            )));
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
            crate::models::workflow::AgentToolRef::Task { task, git_ref } => {
                if task.is_empty() {
                    bail!(
                        "Action '{}' has a tool with empty task reference",
                        action_name
                    );
                }
                if git_ref.is_some() {
                    bail!(
                        "Action '{}' tool task '{}': `ref` is not supported on agent task tools yet",
                        action_name,
                        task
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

    // Approver forms render their own field types (ui approval-card.tsx); a
    // JSON editor there is a follow-up (spec D5).
    if let Some((field_name, _)) = action
        .input
        .iter()
        .find(|(_, f)| f.field_type == crate::template::JSON_TYPE)
    {
        bail!(
            "Action '{}' is type 'approval' but input '{}' is type json (not supported in approval forms)",
            action_name,
            field_name
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
    if !action.args.is_empty() {
        bail!(
            "Action '{}' is type 'approval' but has 'args' field (only valid for script actions)",
            action_name
        );
    }

    Ok(vec![])
}

/// Validates that a hook references an existing action (or a library action),
/// and that a `type: task` hook action does not point outside this workspace —
/// hook actions are never resolved cross-workspace.
fn validate_hook_action_exists(
    label: &str,
    hook: &HookDef,
    config: &WorkspaceConfig,
    libraries_resolved: bool,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if hook.git_ref.is_some() {
        bail!("{}: `ref` is not supported on hooks yet", label);
    }
    let action = hook.action.as_str();
    if !libraries_resolved && action.contains('.') {
        return Ok(()); // library action — skip when libraries not resolved
    }
    let Some(def) = config.actions.get(action) else {
        bail!("{} references non-existent action '{}'", label, action);
    };
    if def.action_type == "task" {
        if def.git_ref.is_some() {
            bail!(
                "{} uses action '{}' whose task reference carries `ref`: `ref` is not supported on hooks yet",
                label,
                action
            );
        }
        if let Some(task_ref) = def.task.as_deref() {
            if !config.tasks.contains_key(task_ref) && task_ref.contains('.') {
                if libraries_resolved {
                    bail!(
                        "{} uses action '{}' whose task '{}' is in another workspace; hook actions cannot call tasks across workspaces",
                        label, action, task_ref
                    );
                }
                warnings.push(format!(
                    "{} uses action '{}' whose task '{}' is not local - cannot validate offline",
                    label, action, task_ref
                ));
            }
        }
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

/// Compute the required *capability* for an action — the runner the worker
/// must support to execute it. Returned string is one of:
///
///   * `"script"`     — script action with local (or no) runner.
///   * `"docker"`     — script with `runner: docker`, or `type: docker`.
///   * `"kubernetes"` — script with `runner: pod`, or `type: pod`.
///   * `"agent"`      — agent LLM action.
///   * `""`           — action is server-dispatched (task/approval), no worker.
///
/// Paired with [`compute_required_tags`], this replaces the pre-041 model
/// where the capability token was prepended to the tag list. Now they're
/// two distinct dimensions on both worker and step:
///
///   * worker.capabilities contains the step's `required_ability` (subset).
///   * worker.tags is a subset of step's `required_tags` (taint semantics).
pub fn compute_required_ability(action: &ActionDef) -> String {
    match action.action_type.as_str() {
        "task" | "approval" => String::new(),
        "agent" => "agent".to_string(),
        "script" => match action.runner.as_deref() {
            Some("docker") => "docker".to_string(),
            Some("pod") => "kubernetes".to_string(),
            _ => "script".to_string(),
        },
        "docker" => "docker".to_string(),
        "pod" => "kubernetes".to_string(),
        _ => "script".to_string(),
    }
}

/// Compute the required tags for an action — just the user-declared tags,
/// no capability prefix. Server-dispatched actions (task/approval) return
/// empty.
///
/// These tags are taints on the worker side: a worker with `tags: ["gpu"]`
/// only claims steps whose `required_tags` include "gpu". A step with
/// `required_tags: []` reaches only workers with no tags.
pub fn compute_required_tags(action: &ActionDef) -> Vec<String> {
    if action.action_type == "task" || action.action_type == "approval" {
        return vec![];
    }
    // Dedup while preserving order.
    let mut tags: Vec<String> = Vec::with_capacity(action.tags.len());
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

/// Validate a retry configuration.
fn validate_retry_config(
    retry: &crate::models::workflow::RetryConfig,
    context: &str,
) -> Result<()> {
    if retry.max_attempts == 0 || retry.max_attempts > 10 {
        bail!(
            "{} retry max_attempts {} out of range (1..=10)",
            context,
            retry.max_attempts
        );
    }
    if retry.delay.as_secs() > 3600 {
        bail!(
            "{} retry delay {}s exceeds maximum of 3600s (1h)",
            context,
            retry.delay.as_secs()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
