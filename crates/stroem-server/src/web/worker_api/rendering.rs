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
    /// Structured state from the previous task state snapshot (state.json contents).
    /// Available in Tera templates as `{{ state.some_key }}`.
    pub state_json: Option<&'a serde_json::Value>,
    /// Structured global state from the workspace-scoped snapshot (state.json contents).
    /// Available in Tera templates as `{{ global_state.some_key }}`.
    pub global_state_json: Option<&'a serde_json::Value>,
    /// Owner workspace config for cross-workspace steps. When set, the action
    /// definition + connection-typed inputs are resolved against this workspace
    /// (the action body's owner) instead of the caller `workspace`. `None` ⇒
    /// local step: resolve against `workspace` (byte-for-byte today's behaviour).
    pub action_workspace: Option<&'a WorkspaceConfig>,
    /// Workspace revision pinned on the job at creation (git SHA or folder hash).
    /// Available in Tera templates as `{{ job.revision }}`.
    pub job_revision: Option<&'a str>,
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

    // Inject the previous task state into the template context so step inputs
    // can reference `{{ state.some_key }}`.
    if let Some(state_json) = ctx.state_json {
        context.insert("state".to_string(), state_json.clone());
    }

    // Inject the previous global workspace state into the template context so
    // step inputs can reference `{{ global_state.some_key }}`.
    if let Some(global_state_json) = ctx.global_state_json {
        context.insert("global_state".to_string(), global_state_json.clone());
    }

    // Job metadata: always present so `{{ job.revision }}` never hits a
    // Tera undefined-variable error (revision is null for pre-migration jobs).
    // Inserted BEFORE step outputs so a step literally named `job` shadows it
    // (backward compatibility for workflows predating job metadata).
    context.insert("job".to_string(), job_context(ctx.job_revision));

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

/// Build the `job` template variable: `{{ job.revision }}` etc.
pub(crate) fn job_context(revision: Option<&str>) -> serde_json::Value {
    serde_json::json!({ "revision": revision })
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
    // The action body (defaults + connection-typed inputs) belongs to the OWNER
    // workspace for cross-workspace steps. `action_workspace` is `Some` only when
    // the step references `owner.action`; for local steps it falls back to the
    // caller `workspace`, keeping today's behaviour byte-for-byte.
    //
    // Lookup key: only strip to the BARE name for genuine cross-workspace steps
    // (the owner stores the action unqualified). For LOCAL steps we MUST use the
    // full key — a library-imported action is stored under its dotted name
    // (e.g. `common.pg-query`) and never under the bare `pg-query`, so stripping
    // would miss it and skip default-merge + connection resolution.
    let action_ws = ctx.action_workspace.unwrap_or(ctx.workspace);
    let lookup: &str = if ctx.action_workspace.is_some() {
        stroem_common::template::parse_qualified_ref(&flow_step.action).1
    } else {
        &flow_step.action
    };
    let action = match action_ws.actions.get(lookup) {
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

    let prepared = prepare_action_input(&input_val, &action.input, action_ws)
        .context("Failed to prepare action input")?;
    Ok(Some(prepared))
}

/// Render action_spec templates (env, cmd, script, source, manifest, args).
///
/// Returns `None` if `action_spec` is `None`. Returns the spec unchanged if
/// it is not a JSON object.
#[allow(clippy::too_many_arguments)]
pub fn render_action_spec(
    action_spec: Option<&serde_json::Value>,
    rendered_input: Option<&serde_json::Value>,
    secrets: &serde_json::Value,
    completed_steps: &[(String, Option<serde_json::Value>)],
    loop_item: Option<&serde_json::Value>,
    loop_index: Option<i32>,
    loop_total: Option<i32>,
    state_json: Option<&serde_json::Value>,
    global_state_json: Option<&serde_json::Value>,
    job_revision: Option<&str>,
) -> Result<Option<serde_json::Value>> {
    let original_spec = match action_spec {
        Some(s) => s,
        None => return Ok(None),
    };

    let mut spec_obj = match original_spec.as_object() {
        Some(obj) => obj.clone(),
        None => return Ok(Some(original_spec.clone())),
    };

    // Build context with rendered input + secrets + completed step outputs
    let mut spec_ctx = serde_json::Map::new();
    if let Some(input_val) = rendered_input {
        spec_ctx.insert("input".to_string(), input_val.clone());
    }
    spec_ctx.insert("secret".to_string(), secrets.clone());
    // Task + global workspace state so `{{ state.* }}` and `{{ global_state.* }}`
    // resolve in action bodies (script, cmd, env, source, args, manifest).
    if let Some(state_val) = state_json {
        spec_ctx.insert("state".to_string(), state_val.clone());
    }
    if let Some(global_state_val) = global_state_json {
        spec_ctx.insert("global_state".to_string(), global_state_val.clone());
    }
    // Before step outputs: a step named `job` shadows the job metadata.
    spec_ctx.insert("job".to_string(), job_context(job_revision));
    for (step_name, output) in completed_steps {
        let mut step_ctx = serde_json::Map::new();
        if let Some(output) = output {
            step_ctx.insert("output".to_string(), output.clone());
        }
        let safe_name = step_name.replace('-', "_");
        spec_ctx.insert(safe_name, serde_json::Value::Object(step_ctx));
    }

    // Inject `each` variable for for_each loop instance steps
    if let (Some(item), Some(index)) = (loop_item, loop_index) {
        spec_ctx.insert(
            "each".to_string(),
            serde_json::json!({
                "item": item,
                "index": index,
                "total": loop_total,
            }),
        );
    }

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

    // Render args array elements (e.g. ["{{ input.target }}", "--region", "{{ input.region }}"])
    if let Some(args_val) = spec_obj.get("args") {
        let rendered_args = render_json_strings(args_val, &spec_context)
            .context("Failed to render args templates")?;
        spec_obj.insert("args".to_string(), rendered_args);
    }

    Ok(Some(serde_json::Value::Object(spec_obj)))
}

/// Render image template (e.g. `{{ input.image_tag }}`).
///
/// Returns the image unchanged if it contains no template syntax.
#[allow(clippy::too_many_arguments)]
pub fn render_image(
    image: Option<&str>,
    rendered_input: Option<&serde_json::Value>,
    secrets: &serde_json::Value,
    completed_steps: &[(String, Option<serde_json::Value>)],
    loop_item: Option<&serde_json::Value>,
    loop_index: Option<i32>,
    loop_total: Option<i32>,
    job_revision: Option<&str>,
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
    // Before step outputs: a step named `job` shadows the job metadata.
    spec_ctx.insert("job".to_string(), job_context(job_revision));
    for (step_name, output) in completed_steps {
        let mut step_ctx = serde_json::Map::new();
        if let Some(output) = output {
            step_ctx.insert("output".to_string(), output.clone());
        }
        let safe_name = step_name.replace('-', "_");
        spec_ctx.insert(safe_name, serde_json::Value::Object(step_ctx));
    }

    // Inject `each` variable for for_each loop instance steps
    if let (Some(item), Some(index)) = (loop_item, loop_index) {
        spec_ctx.insert(
            "each".to_string(),
            serde_json::json!({
                "item": item,
                "index": index,
                "total": loop_total,
            }),
        );
    }

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
            multiple: false,
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
            retry: None,
            inline_action: None,
        }
    }

    fn make_step_row(step_name: &str, input: Option<serde_json::Value>) -> JobStepRow {
        JobStepRow {
            step_name: step_name.to_string(),
            action_name: "test-action".to_string(),
            action_type: "script".to_string(),
            input,
            status: "ready".to_string(),
            required_ability: "script".to_string(),
            required_tags: json!([]),
            runner: "local".to_string(),
            retry_history: json!([]),
            ..Default::default()
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
        let result = render_action_spec(
            None,
            None,
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_render_action_spec_non_object_returns_unchanged() {
        let spec = json!("just a string");
        let secrets = json!({});
        let result = render_action_spec(
            Some(&spec),
            None,
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(result, Some(json!("just a string")));
    }

    #[test]
    fn test_render_action_spec_renders_env_template() {
        let spec = json!({"env": {"MY_VAR": "prefix-{{ input.key }}"}});
        let rendered_input = json!({"key": "world"});
        let secrets = json!({});

        let result = render_action_spec(
            Some(&spec),
            Some(&rendered_input),
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(result["env"]["MY_VAR"], "prefix-world");
    }

    #[test]
    fn test_render_action_spec_renders_cmd_template() {
        let spec = json!({"cmd": "echo {{ input.message }}"});
        let rendered_input = json!({"message": "hello"});
        let secrets = json!({});

        let result = render_action_spec(
            Some(&spec),
            Some(&rendered_input),
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(result["cmd"], "echo hello");
    }

    #[test]
    fn test_render_action_spec_renders_script_template() {
        let spec = json!({"script": "#!/bin/bash\necho {{ input.greeting }}"});
        let rendered_input = json!({"greeting": "hi"});
        let secrets = json!({});

        let result = render_action_spec(
            Some(&spec),
            Some(&rendered_input),
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
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

        let result = render_action_spec(
            Some(&spec),
            Some(&rendered_input),
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
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
        let result = render_image(None, None, &secrets, &[], None, None, None, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_render_image_no_template_syntax_returns_unchanged() {
        let secrets = json!({});
        let result = render_image(
            Some("my-registry/my-image:latest"),
            None,
            &secrets,
            &[],
            None,
            None,
            None,
            None,
        )
        .unwrap();
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
            &[],
            None,
            None,
            None,
            None,
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
            &[],
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(result, Some("private.registry.io/app:latest".to_string()));
    }

    #[test]
    fn test_render_action_spec_script_references_upstream_step_output() {
        let spec = json!({
            "script": "echo \"Category: {{ classify.output.category }}\"\necho \"Confidence: {{ classify.output.confidence }}\""
        });
        let rendered_input = json!({});
        let secrets = json!({});
        let completed_steps = vec![(
            "classify".to_string(),
            Some(json!({"category": "bug", "confidence": 0.95})),
        )];

        let result = render_action_spec(
            Some(&spec),
            Some(&rendered_input),
            &secrets,
            &completed_steps,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            result["script"],
            "echo \"Category: bug\"\necho \"Confidence: 0.95\""
        );
    }

    #[test]
    fn test_render_action_spec_env_references_upstream_step_output() {
        let spec = json!({"env": {"CATEGORY": "{{ classify.output.category }}"}});
        let secrets = json!({});
        let completed_steps = vec![("classify".to_string(), Some(json!({"category": "feature"})))];

        let result = render_action_spec(
            Some(&spec),
            None,
            &secrets,
            &completed_steps,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(result["env"]["CATEGORY"], "feature");
    }

    #[test]
    fn test_render_action_spec_sanitizes_step_name_hyphens() {
        let spec = json!({"script": "echo {{ my_step.output.value }}"});
        let secrets = json!({});
        let completed_steps = vec![("my-step".to_string(), Some(json!({"value": "hello"})))];

        let result = render_action_spec(
            Some(&spec),
            None,
            &secrets,
            &completed_steps,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(result["script"], "echo hello");
    }

    #[test]
    fn test_render_action_spec_renders_args_from_input() {
        let spec = serde_json::json!({
            "type": "script",
            "script": "echo test",
            "args": ["{{ input.target }}", "--region", "{{ input.region }}"]
        });
        let input = serde_json::json!({"target": "prod", "region": "us-east-1"});
        let secrets = serde_json::json!({});
        let result = render_action_spec(
            Some(&spec),
            Some(&input),
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let args = result["args"].as_array().unwrap();
        assert_eq!(args[0], "prod");
        assert_eq!(args[1], "--region");
        assert_eq!(args[2], "us-east-1");
    }

    #[test]
    fn test_render_action_spec_renders_args_from_step_output() {
        let spec = serde_json::json!({
            "type": "script",
            "script": "echo test",
            "args": ["{{ build.output.artifact }}"]
        });
        let input = serde_json::json!({});
        let secrets = serde_json::json!({});
        let completed = vec![(
            "build".to_string(),
            Some(serde_json::json!({"artifact": "app-v2.tar.gz"})),
        )];
        let result = render_action_spec(
            Some(&spec),
            Some(&input),
            &secrets,
            &completed,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let args = result["args"].as_array().unwrap();
        assert_eq!(args[0], "app-v2.tar.gz");
    }

    #[test]
    fn test_render_action_spec_renders_args_from_secret() {
        let spec = serde_json::json!({
            "type": "script",
            "script": "echo test",
            "args": ["--token", "{{ secret.api_token }}"]
        });
        let input = serde_json::json!({});
        let secrets = serde_json::json!({"api_token": "xyz123"});
        let result = render_action_spec(
            Some(&spec),
            Some(&input),
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let args = result["args"].as_array().unwrap();
        assert_eq!(args[0], "--token");
        assert_eq!(args[1], "xyz123");
    }

    #[test]
    fn test_render_action_spec_renders_args_with_hyphenated_step_name() {
        let spec = serde_json::json!({
            "type": "script",
            "script": "echo test",
            "args": ["{{ my_step.output.val }}"]
        });
        let input = serde_json::json!({});
        let secrets = serde_json::json!({});
        // Step name has hyphen — should be sanitized to underscore in context
        let completed = vec![(
            "my-step".to_string(),
            Some(serde_json::json!({"val": "foo"})),
        )];
        let result = render_action_spec(
            Some(&spec),
            Some(&input),
            &secrets,
            &completed,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let args = result["args"].as_array().unwrap();
        assert_eq!(args[0], "foo");
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
            retry: None,
            on_success: vec![],
            on_error: vec![],
            on_suspended: vec![],
            on_cancel: vec![],
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
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
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
    fn test_prepare_step_action_input_resolves_against_owner_workspace() {
        use stroem_common::models::workflow::{ConnectionDef, ConnectionTypeDef};

        // Owner workspace B: action `remote` with a connection-typed input
        // `conn: pg`, connection_type `pg`, and connection `prod` carrying a host.
        let mut owner_action = make_action("script");
        owner_action
            .input
            .insert("conn".to_string(), make_input_field("pg"));
        let mut owner = WorkspaceConfig::default();
        owner.actions.insert("remote".to_string(), owner_action);
        owner.connection_types.insert(
            "pg".to_string(),
            ConnectionTypeDef {
                properties: HashMap::new(),
            },
        );
        owner.connections.insert(
            "prod".to_string(),
            ConnectionDef {
                connection_type: Some("pg".to_string()),
                values: HashMap::from([("host".to_string(), json!("db.owner.internal"))]),
            },
        );

        // Caller workspace A: task `t`, flow step `s` referencing `B.remote`,
        // mapping input `{ conn: "prod" }`. The caller has NO `prod` connection,
        // so a correct resolution can only come from the OWNER workspace.
        let mut task = TaskDef {
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
            on_cancel: vec![],
        };
        let flow_input = HashMap::from([("conn".to_string(), json!("prod"))]);
        task.flow
            .insert("s".to_string(), make_flow_step("B.remote", flow_input));
        let mut caller = WorkspaceConfig::default();
        caller.tasks.insert("t".to_string(), task);

        let step = make_step_row("s", None);
        let ctx = RenderContext {
            workspace: &caller,
            task_name: "t",
            step: &step,
            job_input: None,
            completed_steps: &[],
            state_json: None,
            global_state_json: None,
            action_workspace: Some(&owner),
            job_revision: None,
        };
        // Rendered input mirrors the caller flow-step input.
        let rendered_input = Some(json!({"conn": "prod"}));

        let result = prepare_step_action_input(rendered_input, &ctx)
            .unwrap()
            .unwrap();

        // Resolution used the OWNER's `prod` connection → replaced with its
        // values object (host present). Proves OWNER-context resolution.
        assert_eq!(result["conn"]["host"], "db.owner.internal");
    }

    #[test]
    fn test_prepare_step_action_input_resolves_local_library_dotted_action() {
        use stroem_common::models::workflow::{ConnectionDef, ConnectionTypeDef};

        // A LOCAL library-imported action is stored under its full DOTTED key
        // (`common.pg-query`) — library flattening never stores the bare name.
        // The step references it by the dotted name and is LOCAL
        // (`action_workspace: None`). Resolution must use the FULL key, not the
        // bare `pg-query`, or connection-typed inputs would silently pass through
        // unresolved. This is the byte-for-byte backward-compat guarantee.
        let mut lib_action = make_action("script");
        lib_action
            .input
            .insert("conn".to_string(), make_input_field("pg"));

        let mut task = TaskDef {
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
            on_cancel: vec![],
        };
        let flow_input = HashMap::from([("conn".to_string(), json!("prod"))]);
        task.flow.insert(
            "s".to_string(),
            make_flow_step("common.pg-query", flow_input),
        );

        let mut workspace = WorkspaceConfig::default();
        workspace
            .actions
            .insert("common.pg-query".to_string(), lib_action);
        workspace.connection_types.insert(
            "pg".to_string(),
            ConnectionTypeDef {
                properties: HashMap::new(),
            },
        );
        workspace.connections.insert(
            "prod".to_string(),
            ConnectionDef {
                connection_type: Some("pg".to_string()),
                values: HashMap::from([("host".to_string(), json!("db.local.internal"))]),
            },
        );
        workspace.tasks.insert("t".to_string(), task);

        let step = make_step_row("s", None);
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "t",
            step: &step,
            job_input: None,
            completed_steps: &[],
            state_json: None,
            global_state_json: None,
            action_workspace: None, // LOCAL step
            job_revision: None,
        };
        let rendered_input = Some(json!({"conn": "prod"}));

        let result = prepare_step_action_input(rendered_input, &ctx)
            .unwrap()
            .unwrap();

        // Connection resolved via the full dotted key → values object with host.
        assert_eq!(result["conn"]["host"], "db.local.internal");
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

    #[test]
    fn test_render_action_spec_renders_args_with_each_context() {
        let spec = serde_json::json!({
            "type": "script",
            "script": "echo test",
            "args": ["{{ each.item }}", "--index", "{{ each.index }}"]
        });
        let input = serde_json::json!({});
        let secrets = serde_json::json!({});
        let loop_item = serde_json::json!("my-item");
        let result = render_action_spec(
            Some(&spec),
            Some(&input),
            &secrets,
            &[],
            Some(&loop_item),
            Some(2),
            Some(5),
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let args = result["args"].as_array().unwrap();
        assert_eq!(args[0], "my-item");
        assert_eq!(args[1], "--index");
        assert_eq!(args[2], "2");
    }

    #[test]
    fn test_render_action_spec_renders_env_with_each_context() {
        let spec = serde_json::json!({
            "type": "script",
            "script": "echo test",
            "env": {
                "ITEM": "{{ each.item }}",
                "INDEX": "{{ each.index }}"
            }
        });
        let input = serde_json::json!({});
        let secrets = serde_json::json!({});
        let loop_item = serde_json::json!("batch-42");
        let result = render_action_spec(
            Some(&spec),
            Some(&input),
            &secrets,
            &[],
            Some(&loop_item),
            Some(3),
            Some(10),
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let env = result["env"].as_object().unwrap();
        assert_eq!(env["ITEM"], "batch-42");
        assert_eq!(env["INDEX"], "3");
    }

    // -------------------------------------------------------------------------
    // state_json injection
    // -------------------------------------------------------------------------

    #[test]
    fn test_render_step_input_with_state_json() {
        // Flow step input template references {{ state.cursor }}
        let flow_input = HashMap::from([
            ("cursor".to_string(), json!("{{ state.cursor }}")),
            ("count".to_string(), json!("{{ state.count }}")),
        ]);
        let mut task = TaskDef {
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
            on_cancel: vec![],
        };
        task.flow.insert(
            "consume".to_string(),
            make_flow_step("my-action", flow_input),
        );
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("consume", None);
        let state_json = json!({"cursor": "abc123", "count": 42});
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
            state_json: Some(&state_json),
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
        };

        let result = render_step_input(&ctx).unwrap().unwrap();
        // Tera renders numbers as strings in template context
        assert_eq!(result["cursor"], "abc123");
    }

    #[test]
    fn test_render_step_input_state_json_none_does_not_inject() {
        // When state_json is None, "state" key is absent; a step NOT referencing
        // state should still render normally.
        let flow_input = HashMap::from([("greeting".to_string(), json!("Hello {{ input.name }}"))]);
        let mut task = TaskDef {
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
            on_cancel: vec![],
        };
        task.flow
            .insert("greet".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("greet", None);
        let job_input = json!({"name": "World"});
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: Some(&job_input),
            completed_steps: &[],
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
        };

        let result = render_step_input(&ctx).unwrap().unwrap();
        assert_eq!(result["greeting"], "Hello World");
    }

    // -------------------------------------------------------------------------
    // global_state_json injection
    // -------------------------------------------------------------------------

    #[test]
    fn test_render_step_input_with_global_state_json() {
        // Flow step input template references {{ global_state.last_cursor }}
        let flow_input = HashMap::from([(
            "cursor".to_string(),
            json!("{{ global_state.last_cursor }}"),
        )]);
        let mut task = TaskDef {
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
            on_cancel: vec![],
        };
        task.flow
            .insert("step1".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("step1", None);
        let global_state_json = json!({"last_cursor": "xyz789"});
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
            state_json: None,
            global_state_json: Some(&global_state_json),
            action_workspace: None,
            job_revision: None,
        };

        let result = render_step_input(&ctx).unwrap().unwrap();
        assert_eq!(result["cursor"], "xyz789");
    }

    #[test]
    fn test_render_step_input_global_state_json_none_does_not_inject() {
        // When global_state_json is None, "global_state" key is absent;
        // a step not referencing it should still render normally.
        let flow_input = HashMap::from([("greeting".to_string(), json!("Hello {{ input.name }}"))]);
        let mut task = TaskDef {
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
            on_cancel: vec![],
        };
        task.flow
            .insert("greet".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("greet", None);
        let job_input = json!({"name": "World"});
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: Some(&job_input),
            completed_steps: &[],
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
        };

        let result = render_step_input(&ctx).unwrap().unwrap();
        assert_eq!(result["greeting"], "Hello World");
    }

    #[test]
    fn test_render_step_input_global_state_and_task_state_both_available() {
        // Both state and global_state are injected simultaneously
        let flow_input = HashMap::from([
            ("task_cursor".to_string(), json!("{{ state.cursor }}")),
            (
                "global_cursor".to_string(),
                json!("{{ global_state.cursor }}"),
            ),
        ]);
        let mut task = TaskDef {
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
            on_cancel: vec![],
        };
        task.flow
            .insert("step1".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);

        let step = make_step_row("step1", None);
        let state_json = json!({"cursor": "task-cursor-val"});
        let global_state_json = json!({"cursor": "global-cursor-val"});
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
            state_json: Some(&state_json),
            global_state_json: Some(&global_state_json),
            action_workspace: None,
            job_revision: None,
        };

        let result = render_step_input(&ctx).unwrap().unwrap();
        assert_eq!(result["task_cursor"], "task-cursor-val");
        assert_eq!(result["global_cursor"], "global-cursor-val");
    }

    // -------------------------------------------------------------------------
    // job.revision in template contexts
    // -------------------------------------------------------------------------

    fn make_workspace_with_step(flow_input: HashMap<String, serde_json::Value>) -> WorkspaceConfig {
        let mut task = TaskDef {
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
            on_cancel: vec![],
        };
        task.flow
            .insert("step1".to_string(), make_flow_step("my-action", flow_input));
        let mut workspace = WorkspaceConfig::default();
        workspace.tasks.insert("my-task".to_string(), task);
        workspace
    }

    #[test]
    fn test_render_step_input_exposes_job_revision() {
        let flow_input = HashMap::from([("rev".to_string(), json!("{{ job.revision }}"))]);
        let workspace = make_workspace_with_step(flow_input);
        let step = make_step_row("step1", None);
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: Some("abc123def"),
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"rev": "abc123def"})));
    }

    #[test]
    fn test_render_step_input_job_revision_none_renders_empty() {
        // Jobs created before revisions existed have NULL revision — the
        // template must still render (empty string), not error.
        let flow_input = HashMap::from([("rev".to_string(), json!("{{ job.revision }}"))]);
        let workspace = make_workspace_with_step(flow_input);
        let step = make_step_row("step1", None);
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &[],
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: None,
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"rev": ""})));
    }

    #[test]
    fn test_render_action_spec_exposes_job_revision() {
        let spec = json!({"script": "echo building {{ job.revision }}"});
        let secrets = json!({});
        let result = render_action_spec(
            Some(&spec),
            None,
            &secrets,
            &[],
            None,
            None,
            None,
            None,
            None,
            Some("abc123def"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(result["script"], "echo building abc123def");
    }

    #[test]
    fn test_render_image_exposes_job_revision() {
        let secrets = json!({});
        let result = render_image(
            Some("my-registry/app:{{ job.revision }}"),
            None,
            &secrets,
            &[],
            None,
            None,
            None,
            Some("abc123def"),
        )
        .unwrap();

        assert_eq!(result, Some("my-registry/app:abc123def".to_string()));
    }

    // A completed step literally named `job` must keep its output in the
    // context — the step shadows the job metadata (backward compatibility).

    #[test]
    fn test_render_step_input_step_named_job_shadows_job_metadata() {
        let flow_input = HashMap::from([("value".to_string(), json!("{{ job.output.result }}"))]);
        let workspace = make_workspace_with_step(flow_input);
        let step = make_step_row("step1", None);
        let completed_steps = vec![("job".to_string(), Some(json!({"result": "step-wins"})))];
        let ctx = RenderContext {
            workspace: &workspace,
            task_name: "my-task",
            step: &step,
            job_input: None,
            completed_steps: &completed_steps,
            state_json: None,
            global_state_json: None,
            action_workspace: None,
            job_revision: Some("abc123def"),
        };

        let result = render_step_input(&ctx).unwrap();
        assert_eq!(result, Some(json!({"value": "step-wins"})));
    }

    #[test]
    fn test_render_action_spec_step_named_job_shadows_job_metadata() {
        let spec = json!({"script": "echo {{ job.output.result }}"});
        let secrets = json!({});
        let completed_steps = vec![("job".to_string(), Some(json!({"result": "step-wins"})))];
        let result = render_action_spec(
            Some(&spec),
            None,
            &secrets,
            &completed_steps,
            None,
            None,
            None,
            None,
            None,
            Some("abc123def"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(result["script"], "echo step-wins");
    }

    #[test]
    fn test_render_image_step_named_job_shadows_job_metadata() {
        let secrets = json!({});
        let completed_steps = vec![("job".to_string(), Some(json!({"tag": "step-wins"})))];
        let result = render_image(
            Some("registry/app:{{ job.output.tag }}"),
            None,
            &secrets,
            &completed_steps,
            None,
            None,
            None,
            Some("abc123def"),
        )
        .unwrap();

        assert_eq!(result, Some("registry/app:step-wins".to_string()));
    }
}
