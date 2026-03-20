use anyhow::{Context, Result};
use stroem_common::models::workflow::WorkspaceConfig;
use stroem_common::template::{
    prepare_action_input, render_env_map, render_input_map, render_json_strings, render_string_opt,
};
use stroem_db::JobStepRow;

/// Context needed for rendering step input and action specs.
pub struct RenderContext<'a> {
    pub workspace: &'a WorkspaceConfig,
    pub task_name: &'a str,
    pub step: &'a JobStepRow,
    pub job_input: Option<&'a serde_json::Value>,
    /// Completed steps as (step_name, output) pairs
    pub completed_steps: &'a [(String, Option<serde_json::Value>)],
}

/// Result of rendering: rendered input, rendered action_spec, rendered image.
pub struct RenderResult {
    pub input: Option<serde_json::Value>,
    pub action_spec: Option<serde_json::Value>,
    pub image: Option<String>,
}

/// Render step input by evaluating Tera templates against the context.
///
/// Returns the raw stored input if the workspace/task/step cannot be found,
/// or if the flow step has no template input configured.
pub fn render_step_input(ctx: &RenderContext) -> Result<Option<serde_json::Value>> {
    let task = match ctx.workspace.tasks.get(ctx.task_name) {
        Some(t) => t,
        None => return Ok(ctx.step.input.clone()),
    };
    // For loop instance steps (e.g. "process[0]"), fall back to looking up by loop_source
    let flow_step = match task.flow.get(&ctx.step.step_name) {
        Some(fs) => fs,
        None => match ctx
            .step
            .loop_source
            .as_ref()
            .and_then(|src| task.flow.get(src))
        {
            Some(fs) => fs,
            None => return Ok(ctx.step.input.clone()),
        },
    };

    if flow_step.input.is_empty() {
        return Ok(ctx.step.input.clone());
    }

    // Build template context: { "input": job.input, "secret": ..., "step_name": { "output": ... }, ... }
    let mut context = serde_json::Map::new();
    if let Some(job_input) = ctx.job_input {
        context.insert("input".to_string(), job_input.clone());
    }

    if !ctx.workspace.secrets.is_empty() {
        if let Ok(secrets_value) = serde_json::to_value(&ctx.workspace.secrets) {
            context.insert("secret".to_string(), secrets_value);
        }
    }

    // Add completed step outputs to context.
    // Step names are sanitized (hyphens → underscores) so Tera can resolve
    // dotted paths like {{ step_name.output.key }}.
    for (step_name, output) in ctx.completed_steps {
        let mut step_ctx = serde_json::Map::new();
        if let Some(output) = output {
            step_ctx.insert("output".to_string(), output.clone());
        }
        let safe_name = step_name.replace('-', "_");
        context.insert(safe_name, serde_json::Value::Object(step_ctx));
    }

    // Inject `each` variable for loop instance steps
    if let (Some(ref loop_item), Some(loop_index)) = (&ctx.step.loop_item, ctx.step.loop_index) {
        context.insert(
            "each".to_string(),
            serde_json::json!({
                "item": loop_item,
                "index": loop_index,
                "total": ctx.step.loop_total,
            }),
        );
    }

    let context_value = serde_json::Value::Object(context);
    let rendered = render_input_map(&flow_step.input, &context_value)
        .context("Failed to render step input template")?;
    Ok(Some(rendered))
}

/// Merge action-level input defaults and prepare final input.
///
/// Looks up the action definition for this step and applies defaults and
/// connection resolution. Falls through to the rendered input unchanged if
/// no action is found or if the action has no input schema.
pub fn prepare_step_action_input(
    rendered_input: Option<serde_json::Value>,
    ctx: &RenderContext,
) -> Result<Option<serde_json::Value>> {
    let task = match ctx.workspace.tasks.get(ctx.task_name) {
        Some(t) => t,
        None => return Ok(rendered_input),
    };
    // For loop instance steps, fall back to looking up by loop_source
    let flow_step = match task.flow.get(&ctx.step.step_name) {
        Some(fs) => fs,
        None => match ctx
            .step
            .loop_source
            .as_ref()
            .and_then(|src| task.flow.get(src))
        {
            Some(fs) => fs,
            None => return Ok(rendered_input),
        },
    };
    let action = match ctx.workspace.actions.get(&flow_step.action) {
        Some(a) => a,
        None => return Ok(rendered_input),
    };
    if action.input.is_empty() {
        return Ok(rendered_input);
    }

    let mut input_val = rendered_input.unwrap_or_else(|| serde_json::json!({}));

    // Merge missing fields from job input that match the action's input schema.
    // This handles the case where a flow step doesn't explicitly map a field
    // (e.g. a connection input), but the job-level input has it resolved.
    merge_missing_action_fields(&mut input_val, ctx.job_input, action.input.keys());

    let prepared = prepare_action_input(&input_val, &action.input, ctx.workspace)
        .context("Failed to prepare action input")?;
    Ok(Some(prepared))
}

/// Render action_spec templates (env, cmd, script, source, manifest).
///
/// Returns `None` if `action_spec` is `None`. Returns the spec unchanged if
/// it is not a JSON object.
pub fn render_action_spec(
    action_spec: Option<&serde_json::Value>,
    rendered_input: Option<&serde_json::Value>,
    secrets: &serde_json::Value,
) -> Result<Option<serde_json::Value>> {
    let original_spec = match action_spec {
        Some(s) => s,
        None => return Ok(None),
    };

    let mut spec_obj = match original_spec.as_object() {
        Some(obj) => obj.clone(),
        None => return Ok(Some(original_spec.clone())),
    };

    // Build context with rendered input + secrets
    let mut spec_ctx = serde_json::Map::new();
    if let Some(input_val) = rendered_input {
        spec_ctx.insert("input".to_string(), input_val.clone());
    }
    spec_ctx.insert("secret".to_string(), secrets.clone());
    let spec_context = serde_json::Value::Object(spec_ctx);

    // Render env values if present
    if let Some(env_val) = spec_obj.get("env") {
        if let Some(env_obj) = env_val.as_object() {
            let env_map: std::collections::HashMap<String, String> = env_obj
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            let rendered_env =
                render_env_map(&env_map, &spec_context).context("Failed to render env template")?;
            let rendered_env_value: serde_json::Map<String, serde_json::Value> = rendered_env
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            spec_obj.insert(
                "env".to_string(),
                serde_json::Value::Object(rendered_env_value),
            );
        }
    }

    // Render cmd if present
    if let Some(cmd_val) = spec_obj.get("cmd") {
        if let Some(cmd_str) = cmd_val.as_str() {
            let cmd_opt = Some(cmd_str.to_string());
            if let Some(rendered_cmd) = render_string_opt(&cmd_opt, &spec_context)
                .context("Failed to render cmd template")?
            {
                spec_obj.insert("cmd".to_string(), serde_json::Value::String(rendered_cmd));
            }
        }
    }

    // Render script (inline code) if present
    if let Some(script_val) = spec_obj.get("script") {
        if let Some(script_str) = script_val.as_str() {
            let script_opt = Some(script_str.to_string());
            if let Some(rendered_script) = render_string_opt(&script_opt, &spec_context)
                .context("Failed to render script template")?
            {
                spec_obj.insert(
                    "script".to_string(),
                    serde_json::Value::String(rendered_script),
                );
            }
        }
    }

    // Render source (file path) if present — may contain template expressions
    if let Some(source_val) = spec_obj.get("source") {
        if let Some(source_str) = source_val.as_str() {
            let source_opt = Some(source_str.to_string());
            if let Some(rendered_source) = render_string_opt(&source_opt, &spec_context)
                .context("Failed to render source template")?
            {
                spec_obj.insert(
                    "source".to_string(),
                    serde_json::Value::String(rendered_source),
                );
            }
        }
    }

    // Render manifest string values (e.g. serviceAccountName from input)
    if let Some(manifest_val) = spec_obj.get("manifest") {
        let rendered_manifest = render_json_strings(manifest_val, &spec_context)
            .context("Failed to render manifest template")?;
        spec_obj.insert("manifest".to_string(), rendered_manifest);
    }

    Ok(Some(serde_json::Value::Object(spec_obj)))
}

/// Render image template (e.g. `{{ input.image_tag }}`).
///
/// Returns the image unchanged if it contains no template syntax.
pub fn render_image(
    image: Option<&str>,
    rendered_input: Option<&serde_json::Value>,
    secrets: &serde_json::Value,
) -> Result<Option<String>> {
    let image_str = match image {
        Some(s) => s,
        None => return Ok(None),
    };
    if !image_str.contains("{{") {
        return Ok(Some(image_str.to_string()));
    }

    let mut spec_ctx = serde_json::Map::new();
    if let Some(input_val) = rendered_input {
        spec_ctx.insert("input".to_string(), input_val.clone());
    }
    spec_ctx.insert("secret".to_string(), secrets.clone());
    let spec_context = serde_json::Value::Object(spec_ctx);

    let img_opt = Some(image_str.to_string());
    render_string_opt(&img_opt, &spec_context).context("Failed to render image template")
}

/// Merge missing fields from job-level input into step input for fields declared
/// in the action's input schema. Step-level values always take precedence.
/// Null values in job input are skipped to avoid breaking downstream resolution.
pub fn merge_missing_action_fields<'a>(
    input_val: &mut serde_json::Value,
    job_input: Option<&serde_json::Value>,
    action_field_names: impl Iterator<Item = &'a String>,
) {
    let (Some(job_input), Some(input_obj)) = (job_input, input_val.as_object_mut()) else {
        return;
    };
    let Some(job_map) = job_input.as_object() else {
        return;
    };
    for field_name in action_field_names {
        if !input_obj.contains_key(field_name) {
            if let Some(val) = job_map.get(field_name) {
                if !val.is_null() {
                    input_obj.insert(field_name.clone(), val.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use stroem_common::models::workflow::{
        ActionDef, FlowStep, InputFieldDef, TaskDef, WorkspaceConfig,
    };
    use stroem_db::JobStepRow;
    use uuid::Uuid;

    fn field_names(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn make_input_field(field_type: &str) -> InputFieldDef {
        InputFieldDef {
            field_type: field_type.to_string(),
            name: None,
            description: None,
            required: false,
            secret: false,
            default: None,
            options: None,
            allow_custom: false,
            order: None,
        }
    }

    fn make_action(action_type: &str) -> ActionDef {
        ActionDef {
            action_type: action_type.to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: None,
            source: None,
            runner: None,
            language: None,
            dependencies: vec![],
            interpreter: None,
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
            output_schema: None,
            temperature: None,
            max_tokens: None,
            tools: vec![],
            max_turns: None,
            message: None,
        }
    }

    fn make_flow_step(action: &str, input: HashMap<String, serde_json::Value>) -> FlowStep {
        FlowStep {
            action: action.to_string(),
            name: None,
            description: None,
            depends_on: vec![],
            input,
            continue_on_failure: false,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            inline_action: None,
        }
    }

    fn make_step_row(step_name: &str, input: Option<serde_json::Value>) -> JobStepRow {
        JobStepRow {
            job_id: Uuid::nil(),
            step_name: step_name.to_string(),
            action_name: "test-action".to_string(),
            action_type: "script".to_string(),
            action_image: None,
            action_spec: None,
            input,
            output: None,
            status: "ready".to_string(),
            worker_id: None,
            started_at: None,
            completed_at: None,
            error_message: None,
            required_tags: json!([]),
            runner: "local".to_string(),
            timeout_secs: None,
            when_condition: None,
            for_each_expr: None,
            loop_source: None,
            loop_index: None,
            loop_total: None,
            loop_item: None,
            agent_state: None,
            suspended_at: None,
        }
    }

    // -------------------------------------------------------------------------
    // render_step_input
    // -------------------------------------------------------------------------

    #[test]
    fn test_render_step_input_task_not_found_returns_step_input() {
        let workspace = WorkspaceConfig::default();
        let step = make_step_row("step1", Some(json!({"key": "value"})));
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "nonexistent-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"key": "value"})));
    }

    #[test]
    fn test_render_step_input_step_not_found_returns_step_input() {
        let mut task = TaskDef {
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
        };
        task.flow.insert(
            "other-step".to_string(),
            make_flow_step("my-action", HashMap::new()),
        );
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("missing-step", Some(json!({"original": true})));
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"original": true})));
    }

    #[test]
    fn test_render_step_input_empty_flow_step_input_returns_step_input() {
        // When flow_step.input is empty, the stored step input is returned as-is.
        let mut task = TaskDef {
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
        };
        // flow step has no input mapping
        task.flow.insert(
            "step1".to_string(),
            make_flow_step("my-action", HashMap::new()),
        );
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("step1", Some(json!({"stored": "value"})));
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"stored": "value"})));
    }

    #[test]
    fn test_render_step_input_renders_job_input_template() {
        let flow_input = HashMap::from([("greeting".to_string(), json!("Hello {{ input.name }}"))]);
        let mut task = TaskDef {
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
        };
        task.flow
            .insert("step1".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("step1", None);
        let job_input = json!({"name": "World"});
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: Some(&job_input),
            completed_steps: &[],
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"greeting": "Hello World"})));
    }

    #[test]
    fn test_render_step_input_renders_step_output_reference() {
        let flow_input =
            HashMap::from([("value".to_string(), json!("{{ step_a.output.result }}"))]);
        let mut task = TaskDef {
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
        };
        task.flow
            .insert("step1".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("step1", None);
        let completed_steps = vec![(
            "step-a".to_string(),
            Some(json!({"result": "computed-value"})),
        )];
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &completed_steps,
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"value": "computed-value"})));
    }

    #[test]
    fn test_render_step_input_sanitizes_hyphens_to_underscores() {
        // Step names with hyphens must be accessed via underscores in templates.
        let flow_input =
            HashMap::from([("out".to_string(), json!("{{ say_hello.output.message }}"))]);
        let mut task = TaskDef {
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
        };
        task.flow
            .insert("step2".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("step2", None);
        // Completed step name has a hyphen — sanitized to underscore in context.
        let completed_steps = vec![(
            "say-hello".to_string(),
            Some(json!({"message": "hi there"})),
        )];
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &completed_steps,
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"out": "hi there"})));
    }

    #[test]
    fn test_render_step_input_includes_secrets_in_context() {
        let flow_input = HashMap::from([("token".to_string(), json!("{{ secret.API_TOKEN }}"))]);
        let mut task = TaskDef {
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
        };
        task.flow
            .insert("step1".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);
        workspace
            .secrets
            .insert("API_TOKEN".to_string(), json!("secret-value-123"));

        let step = make_step_row("step1", None);
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"token": "secret-value-123"})));
    }

    // -------------------------------------------------------------------------
    // render_action_spec
    // -------------------------------------------------------------------------

    #[test]
    fn test_render_action_spec_none_returns_none() {
        let secrets = json!({});
        let result = render_action_spec(None, None, &secrets).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_render_action_spec_non_object_returns_unchanged() {
        let spec = json!("just a string");
        let secrets = json!({});
        let result = render_action_spec(Some(&spec), None, &secrets).unwrap();
        assert_eq!(result, Some(json!("just a string")));
    }

    #[test]
    fn test_render_action_spec_renders_env_template() {
        let spec = json!({"env": {"MY_VAR": "prefix-{{ input.key }}"}});
        let rendered_input = json!({"key": "world"});
        let secrets = json!({});

        let result = render_action_spec(Some(&spec), Some(&rendered_input), &secrets)
            .unwrap()
            .unwrap();

        assert_eq!(result["env"]["MY_VAR"], "prefix-world");
    }

    #[test]
    fn test_render_action_spec_renders_cmd_template() {
        let spec = json!({"cmd": "echo {{ input.message }}"});
        let rendered_input = json!({"message": "hello"});
        let secrets = json!({});

        let result = render_action_spec(Some(&spec), Some(&rendered_input), &secrets)
            .unwrap()
            .unwrap();

        assert_eq!(result["cmd"], "echo hello");
    }

    #[test]
    fn test_render_action_spec_renders_script_template() {
        let spec = json!({"script": "#!/bin/bash\necho {{ input.greeting }}"});
        let rendered_input = json!({"greeting": "hi"});
        let secrets = json!({});

        let result = render_action_spec(Some(&spec), Some(&rendered_input), &secrets)
            .unwrap()
            .unwrap();

        assert_eq!(result["script"], "#!/bin/bash\necho hi");
    }

    #[test]
    fn test_render_action_spec_renders_manifest_templates() {
        let spec = json!({
            "manifest": {
                "spec": {
                    "serviceAccountName": "{{ input.sa_name }}"
                }
            }
        });
        let rendered_input = json!({"sa_name": "my-service-account"});
        let secrets = json!({});

        let result = render_action_spec(Some(&spec), Some(&rendered_input), &secrets)
            .unwrap()
            .unwrap();

        assert_eq!(
            result["manifest"]["spec"]["serviceAccountName"],
            "my-service-account"
        );
    }

    // -------------------------------------------------------------------------
    // render_image
    // -------------------------------------------------------------------------

    #[test]
    fn test_render_image_none_returns_none() {
        let secrets = json!({});
        let result = render_image(None, None, &secrets).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_render_image_no_template_syntax_returns_unchanged() {
        let secrets = json!({});
        let result = render_image(Some("my-registry/my-image:latest"), None, &secrets).unwrap();
        assert_eq!(result, Some("my-registry/my-image:latest".to_string()));
    }

    #[test]
    fn test_render_image_renders_input_tag_template() {
        let rendered_input = json!({"tag": "v1.2.3"});
        let secrets = json!({});
        let result = render_image(
            Some("my-registry/app:{{ input.tag }}"),
            Some(&rendered_input),
            &secrets,
        )
        .unwrap();
        assert_eq!(result, Some("my-registry/app:v1.2.3".to_string()));
    }

    #[test]
    fn test_render_image_renders_secret_registry_template() {
        let rendered_input = json!({});
        let secrets = json!({"registry": "private.registry.io"});
        let result = render_image(
            Some("{{ secret.registry }}/app:latest"),
            Some(&rendered_input),
            &secrets,
        )
        .unwrap();
        assert_eq!(result, Some("private.registry.io/app:latest".to_string()));
    }

    // -------------------------------------------------------------------------
    // prepare_step_action_input
    // -------------------------------------------------------------------------

    #[test]
    fn test_prepare_step_action_input_task_not_found_returns_rendered_input() {
        let workspace = WorkspaceConfig::default();
        let step = make_step_row("step1", None);
        let job_input = json!({"name": "Alice"});
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "nonexistent",
            step: &step,
            job_input: Some(&job_input),
            completed_steps: &[],
        };
        let rendered_input = Some(json!({"foo": "bar"}));

        let result = prepare_step_action_input(rendered_input.clone(), &ctx).unwrap();
        assert_eq!(result, rendered_input);
    }

    #[test]
    fn test_prepare_step_action_input_action_not_found_returns_rendered_input() {
        let mut task = TaskDef {
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
        };
        task.flow.insert(
            "step1".to_string(),
            make_flow_step("nonexistent-action", HashMap::new()),
        );
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("step1", None);
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
        };
        let rendered_input = Some(json!({"foo": "bar"}));

        let result = prepare_step_action_input(rendered_input.clone(), &ctx).unwrap();
        assert_eq!(result, rendered_input);
    }

    #[test]
    fn test_prepare_step_action_input_merges_missing_job_input_fields() {
        // When a flow step doesn't map a field but the job input has it,
        // and the action's schema declares it, it should be merged in.
        let mut action = make_action("script");
        action
            .input
            .insert("sql".to_string(), make_input_field("string"));
        action
            .input
            .insert("extra".to_string(), make_input_field("string"));

        let mut task = TaskDef {
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
        };
        // flow step only maps "sql", not "extra"
        let flow_input = HashMap::from([("sql".to_string(), json!("SELECT 1"))]);
        task.flow
            .insert("step1".to_string(), make_flow_step("run-query", flow_input));

        let mut workspace = WorkspaceConfig::default();
        workspace.actions.insert("run-query".to_string(), action);
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("step1", None);
        // job_input has "extra" which should be merged for action schema fields
        let job_input = json!({"sql": "SELECT 1", "extra": "from-job"});
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: Some(&job_input),
            completed_steps: &[],
        };
        // rendered_input only contains "sql"
        let rendered_input = Some(json!({"sql": "SELECT 1"}));

        let result = prepare_step_action_input(rendered_input, &ctx)
            .unwrap()
            .unwrap();

        assert_eq!(result["sql"], "SELECT 1");
        assert_eq!(result["extra"], "from-job");
    }

    #[test]
    fn test_merge_missing_action_fields_from_job_input() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({
            "sql": "SELECT 1",
            "clickhouse": {
                "host": "ch.example.com",
                "port": 9000,
                "database": "analytics"
            }
        });
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert_eq!(input_val["clickhouse"]["host"], "ch.example.com");
        assert_eq!(input_val["clickhouse"]["port"], 9000);
        assert_eq!(input_val["clickhouse"]["database"], "analytics");
    }

    #[test]
    fn test_merge_step_input_takes_precedence() {
        let mut input_val = json!({"sql": "SELECT 2", "clickhouse": "step-override"});
        let job_input = json!({
            "sql": "SELECT 1",
            "clickhouse": {"host": "ch.example.com"}
        });
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 2");
        assert_eq!(input_val["clickhouse"], "step-override");
    }

    #[test]
    fn test_merge_skipped_when_job_input_is_none() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, None, names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert!(input_val.get("clickhouse").is_none());
    }

    #[test]
    fn test_merge_skipped_when_job_input_is_not_object() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!("some raw string");
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert!(input_val.get("clickhouse").is_none());
    }

    #[test]
    fn test_merge_skipped_when_input_val_is_not_object() {
        let mut input_val = json!("raw");
        let job_input = json!({"clickhouse": {"host": "h"}});
        let names = field_names(&["clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val, json!("raw"));
    }

    #[test]
    fn test_merge_skips_fields_not_in_job_input() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({"sql": "SELECT 1"});
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert!(input_val.get("clickhouse").is_none());
    }

    #[test]
    fn test_merge_skips_null_job_input_fields() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({"sql": "SELECT 1", "clickhouse": null});
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert!(input_val.get("clickhouse").is_none());
    }

    #[test]
    fn test_merge_multiple_missing_fields_all_filled() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({
            "sql": "SELECT 1",
            "clickhouse": {"host": "ch.example.com"},
            "s3_bucket": "my-bucket",
            "redis": {"url": "redis://localhost"}
        });
        let names = field_names(&["sql", "clickhouse", "s3_bucket", "redis"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert_eq!(input_val["clickhouse"]["host"], "ch.example.com");
        assert_eq!(input_val["s3_bucket"], "my-bucket");
        assert_eq!(input_val["redis"]["url"], "redis://localhost");
    }

    #[test]
    fn test_merge_ignores_job_fields_not_in_action_schema() {
        let mut input_val = json!({"sql": "SELECT 1"});
        let job_input = json!({
            "sql": "SELECT 1",
            "clickhouse": {"host": "ch.example.com"},
            "extra_field": "should not appear"
        });
        let names = field_names(&["sql", "clickhouse"]);

        merge_missing_action_fields(&mut input_val, Some(&job_input), names.iter());

        assert_eq!(input_val["sql"], "SELECT 1");
        assert_eq!(input_val["clickhouse"]["host"], "ch.example.com");
        assert!(input_val.get("extra_field").is_none());
    }
}
