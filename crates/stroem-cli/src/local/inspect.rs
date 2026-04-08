use anyhow::{Context, Result};
use stroem_common::models::workflow::WorkspaceConfig;

pub fn cmd_inspect(config: &WorkspaceConfig, task_name: &str) -> Result<()> {
    let task = config
        .tasks
        .get(task_name)
        .with_context(|| format!("Task '{}' not found in workspace", task_name))?;

    // Header
    println!("Task: {}", task_name);
    let mut meta_parts = vec![format!("Mode: {}", task.mode)];
    if let Some(ref folder) = task.folder {
        meta_parts.push(format!("Folder: {}", folder));
    }
    if let Some(ref timeout) = task.timeout {
        meta_parts.push(format!("Timeout: {}", timeout));
    }
    println!("  {}", meta_parts.join(" | "));
    if let Some(ref desc) = task.description {
        println!("  Description: {}", desc);
    }

    // Input schema
    if !task.input.is_empty() {
        println!("\nInput:");
        let mut inputs: Vec<_> = task.input.iter().collect();
        inputs.sort_by_key(|(_, f)| f.order.unwrap_or(i32::MAX));
        for (name, field) in &inputs {
            let req = if field.required {
                "required"
            } else {
                "optional"
            };
            let mut parts = vec![format!("{:<15} {:<10} {}", name, field.field_type, req)];
            if let Some(ref default) = field.default {
                parts.push(format!("default: {}", default));
            }
            if let Some(ref opts) = field.options {
                let opts_str: Vec<&str> = opts.iter().map(|s| s.as_str()).collect();
                parts.push(format!("options: [{}]", opts_str.join(", ")));
            }
            println!("  {}", parts.join("  "));
        }
    }

    // Flow steps
    if !task.flow.is_empty() {
        println!("\nFlow ({} steps):", task.flow.len());
        let mut steps: Vec<_> = task.flow.iter().collect();
        steps.sort_by_key(|(name, _)| name.as_str());

        for (name, step) in &steps {
            let deps = if step.depends_on.is_empty() {
                "-".to_string()
            } else {
                step.depends_on.join(", ")
            };

            let mut extras = Vec::new();
            if let Some(ref when) = step.when {
                extras.push(format!("when: {}", when));
            }
            if step.for_each.is_some() {
                let seq = if step.sequential { " (sequential)" } else { "" };
                extras.push(format!("for_each{}", seq));
            }
            if let Some(ref timeout) = step.timeout {
                extras.push(format!("timeout: {}", timeout));
            }
            if step.continue_on_failure {
                extras.push("continue_on_failure".to_string());
            }

            let extras_str = if extras.is_empty() {
                String::new()
            } else {
                format!("  [{}]", extras.join(", "))
            };

            println!(
                "  {:<20} action: {:<20} deps: {}{}",
                name, step.action, deps, extras_str
            );
        }
    }

    // Hooks
    let has_hooks =
        !task.on_success.is_empty() || !task.on_error.is_empty() || !task.on_suspended.is_empty();
    if has_hooks {
        println!("\nHooks:");
        if !task.on_success.is_empty() {
            let actions: Vec<&str> = task.on_success.iter().map(|h| h.action.as_str()).collect();
            println!("  on_success: [{}]", actions.join(", "));
        }
        if !task.on_error.is_empty() {
            let actions: Vec<&str> = task.on_error.iter().map(|h| h.action.as_str()).collect();
            println!("  on_error:   [{}]", actions.join(", "));
        }
        if !task.on_suspended.is_empty() {
            let actions: Vec<&str> = task
                .on_suspended
                .iter()
                .map(|h| h.action.as_str())
                .collect();
            println!("  on_suspended: [{}]", actions.join(", "));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use stroem_common::models::workflow::{FlowStep, HookDef, InputFieldDef, TaskDef};

    fn make_step(action: &str, depends_on: Vec<&str>) -> FlowStep {
        FlowStep {
            action: action.to_string(),
            name: None,
            description: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
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
    fn inspect_missing_task_errors() {
        let config = WorkspaceConfig::new();
        let result = cmd_inspect(&config, "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn inspect_basic_task() {
        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        flow.insert("build".to_string(), make_step("build-action", vec![]));
        flow.insert("test".to_string(), make_step("test-action", vec!["build"]));

        let task = TaskDef {
            name: None,
            description: Some("Deploy to production".to_string()),
            mode: "distributed".to_string(),
            folder: Some("infra".to_string()),
            input: HashMap::new(),
            flow,
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_task_with_input_and_hooks() {
        let mut config = WorkspaceConfig::new();
        let mut input = HashMap::new();
        input.insert(
            "env".to_string(),
            InputFieldDef {
                field_type: "string".to_string(),
                name: None,
                description: None,
                required: true,
                secret: false,
                default: None,
                options: Some(vec!["staging".to_string(), "production".to_string()]),
                allow_custom: false,
                order: Some(1),
            },
        );

        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_step("act", vec![]));

        let task = TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input,
            flow,
            timeout: None,
            retry: None,
            on_success: vec![HookDef {
                action: "slack-notify".to_string(),
                input: HashMap::new(),
            }],
            on_error: vec![HookDef {
                action: "pagerduty".to_string(),
                input: HashMap::new(),
            }],
            on_suspended: vec![],
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_step_with_when_condition() {
        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        let mut step = make_step("act", vec![]);
        step.when = Some("{{ input.env == 'prod' }}".to_string());
        flow.insert("step1".to_string(), step);

        let task = TaskDef {
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
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_step_with_for_each() {
        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        let mut step = make_step("act", vec![]);
        step.for_each = Some(serde_json::json!(["a", "b", "c"]));
        flow.insert("step1".to_string(), step);

        let task = TaskDef {
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
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_step_with_for_each_sequential() {
        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        let mut step = make_step("act", vec![]);
        step.for_each = Some(serde_json::json!(["a", "b"]));
        step.sequential = true;
        flow.insert("step1".to_string(), step);

        let task = TaskDef {
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
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_step_with_timeout() {
        use stroem_common::duration::HumanDuration;

        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        let mut step = make_step("act", vec![]);
        step.timeout = Some(HumanDuration(300));
        flow.insert("step1".to_string(), step);

        let task = TaskDef {
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
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_step_with_continue_on_failure() {
        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        let mut step = make_step("act", vec![]);
        step.continue_on_failure = true;
        flow.insert("step1".to_string(), step);

        let task = TaskDef {
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
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_task_with_timeout() {
        use stroem_common::duration::HumanDuration;

        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_step("act", vec![]));

        let task = TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow,
            timeout: Some(HumanDuration(1800)),
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_task_with_on_suspended() {
        let mut config = WorkspaceConfig::new();
        let mut flow = HashMap::new();
        flow.insert("step1".to_string(), make_step("act", vec![]));

        let task = TaskDef {
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
            on_suspended: vec![HookDef {
                action: "notify".to_string(),
                input: HashMap::new(),
            }],
        };
        config.tasks.insert("deploy".to_string(), task);

        assert!(cmd_inspect(&config, "deploy").is_ok());
    }

    #[test]
    fn inspect_empty_flow() {
        let mut config = WorkspaceConfig::new();

        let task = TaskDef {
            name: None,
            description: None,
            mode: "distributed".to_string(),
            folder: None,
            input: HashMap::new(),
            flow: HashMap::new(),
            timeout: None,
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
        };
        config.tasks.insert("empty".to_string(), task);

        let result = cmd_inspect(&config, "empty");
        assert!(result.is_ok());
    }
}
