use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use stroem_common::dag;
use stroem_common::gate::{gate, DepOutcome, Gate};
use stroem_common::models::job::SkipReason;
use stroem_common::models::workflow::{ActionDef, FlowStep, TaskDef, WorkspaceConfig};
use stroem_common::template::{
    evaluate_condition, merge_defaults, prepare_action_input, render_env_map, render_input_map,
    render_template, resolve_connection_inputs,
};
use stroem_common::workspace_loader;
use stroem_runner::{
    LogCallback, LogLine, LogStream, RunConfig, RunResult, Runner, RunnerMode, ShellRunner,
};
use tokio_util::sync::CancellationToken;

/// Run a task locally without a server.
pub async fn cmd_run(task_name: &str, path: &str, input: Option<&str>) -> Result<bool> {
    eprintln!("Loading workspace...");
    let workspace_path = Path::new(path)
        .canonicalize()
        .with_context(|| format!("Failed to resolve workspace path '{}'", path))?;

    let (config, warnings) = workspace_loader::load_workspace(&workspace_path)
        .with_context(|| format!("Failed to load workspace from {}", path))?;

    for w in &warnings {
        eprintln!("  WARN: {}", w);
    }

    let task = config
        .tasks
        .get(task_name)
        .with_context(|| format!("Task '{}' not found in workspace", task_name))?;

    // Validate DAG
    dag::validate_dag(&task.flow).context("Invalid task DAG")?;

    // Validate all actions are local script
    validate_actions_local(&task.flow, &config)?;

    // Parse user input
    let user_input: serde_json::Value = match input {
        Some(json_str) => serde_json::from_str(json_str).context("Invalid JSON input")?,
        None => json!({}),
    };

    // Merge task-level input defaults
    let ctx_for_defaults = json!({ "secret": &config.secrets });
    let merged_input = merge_defaults(&user_input, &task.input, &ctx_for_defaults)
        .context("Failed to merge input defaults")?;
    let lookup = stroem_common::template::SingleWorkspace {
        name: "local",
        config: &config,
    };
    let resolved_input = resolve_connection_inputs(&merged_input, &task.input, &lookup)
        .context("Failed to resolve connection inputs")?;

    eprintln!("Task: {} ({} steps)\n", task_name, task.flow.len());

    // Set up cancellation
    let cancel_token = CancellationToken::new();
    let cancel_clone = cancel_token.clone();
    ctrlc::set_handler(move || {
        eprintln!("\nCancelling...");
        cancel_clone.cancel();
    })
    .context("Failed to install Ctrl-C handler")?;

    // Run DAG
    let summary = run_dag(
        task,
        &config,
        &resolved_input,
        &workspace_path,
        &cancel_token,
    )
    .await?;

    eprintln!(
        "\n{} completed, {} skipped, {} failed",
        summary.completed, summary.skipped, summary.failed
    );
    if summary.outcome == RunOutcome::Completed && summary.failed > 0 {
        eprintln!("Every failure was caught by continue_on_failure.");
    }

    Ok(summary.outcome == RunOutcome::Completed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOutcome {
    Completed,
    Failed,
}

struct RunSummary {
    completed: usize,
    skipped: usize,
    failed: usize,
    outcome: RunOutcome,
}

fn skip_label(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::Condition => "condition false",
        SkipReason::Empty => "empty for_each",
        SkipReason::Cascade => "a dependency was skipped",
        SkipReason::Unreachable => "an upstream step failed",
    }
}

/// A failed step: its output is masked for templates and `error` exposed,
/// as on the server (render_context.rs:282-289).
fn record_failure(
    outcomes: &mut HashMap<String, DepOutcome>,
    outputs: &mut HashMap<String, Option<serde_json::Value>>,
    errors: &mut HashMap<String, String>,
    step: &str,
    msg: String,
) {
    outcomes.insert(step.to_string(), DepOutcome::Failed);
    outputs.insert(step.to_string(), None);
    errors.insert(step.to_string(), msg);
}

/// A `for_each` loop that failed without its own `continue_on_failure`: unlike
/// an ordinary failed step (`record_failure`, output always masked), its
/// per-iteration output array is still exposed to a dependent — mirrors the
/// scoping rule in `render_context.rs`'s failed-row branch (spec §4/§12):
/// only a tolerated loop rollup's output is newly exposed, never an ordinary
/// failed step's. `build_render_context` just renders whatever is in
/// `outputs`, so the scoping decision lives entirely here, at the call site.
fn record_failed_loop(
    outcomes: &mut HashMap<String, DepOutcome>,
    outputs: &mut HashMap<String, Option<serde_json::Value>>,
    errors: &mut HashMap<String, String>,
    step: &str,
    iter_outputs: Vec<serde_json::Value>,
    msg: String,
) {
    outcomes.insert(step.to_string(), DepOutcome::Failed);
    outputs.insert(step.to_string(), Some(json!(iter_outputs)));
    errors.insert(step.to_string(), msg);
}

/// Validate that all actions referenced by the task flow are `type: script` with local runner.
fn validate_actions_local(
    flow: &HashMap<String, FlowStep>,
    config: &WorkspaceConfig,
) -> Result<()> {
    for (step_name, step) in flow {
        let action = config.actions.get(&step.action).with_context(|| {
            format!(
                "Step '{}' references unknown action '{}'",
                step_name, step.action
            )
        })?;

        match action.action_type.as_str() {
            "script" => {
                let runner = action.runner.as_deref().unwrap_or("local");
                if runner != "local" {
                    bail!(
                        "Step '{}' uses runner '{}' — only 'local' runner is supported by `stroem run`",
                        step_name, runner
                    );
                }
            }
            other => {
                bail!(
                    "Step '{}' has type '{}' — only 'type: script' is supported by `stroem run`",
                    step_name,
                    other
                );
            }
        }
    }
    Ok(())
}

/// Run the task's DAG locally, returning a summary of results.
async fn run_dag(
    task: &TaskDef,
    config: &WorkspaceConfig,
    input: &serde_json::Value,
    workspace_path: &Path,
    cancel_token: &CancellationToken,
) -> Result<RunSummary> {
    let runner = ShellRunner::new();
    let mut outcomes: HashMap<String, DepOutcome> = HashMap::new();
    let mut outputs: HashMap<String, Option<serde_json::Value>> = HashMap::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    let mut failed_count: usize = 0;

    loop {
        if cancel_token.is_cancelled() {
            bail!("Cancelled by user");
        }
        // Steps the gate can decide now (spec 2026-09-26 §2.3). A skip that
        // is decided while a sibling is still pending is final.
        let mut decidable: Vec<(String, Gate)> = task
            .flow
            .iter()
            .filter(|(name, _)| !outcomes.contains_key(*name))
            .filter_map(|(name, fs)| {
                let g = gate(&fs.depends_on, |d| {
                    outcomes.get(d).copied().unwrap_or(DepOutcome::Pending)
                });
                (g != Gate::Wait).then(|| (name.clone(), g))
            })
            .collect();
        if decidable.is_empty() {
            break;
        }
        decidable.sort_by(|a, b| a.0.cmp(&b.0));

        for (step_name, g) in decidable {
            if cancel_token.is_cancelled() {
                bail!("Cancelled by user");
            }
            if g == Gate::Omitted {
                eprintln!(
                    "--- Step: {} [SKIPPED] (blocked by a dependency's outcome) ---",
                    step_name
                );
                outcomes.insert(step_name.clone(), DepOutcome::Omitted);
                outputs.insert(step_name, None);
                continue;
            }
            let step = &task.flow[&step_name];
            let action = &config.actions[&step.action];
            let ctx = build_render_context(input, &outputs, &errors, &config.secrets);

            if let Some(ref when_expr) = step.when {
                match evaluate_condition(when_expr, &ctx) {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!(
                            "--- Step: {} [SKIPPED] ({}) ---",
                            step_name,
                            skip_label(SkipReason::Condition)
                        );
                        outcomes.insert(step_name.clone(), DepOutcome::Skipped);
                        outputs.insert(step_name, None);
                        continue;
                    }
                    Err(e) => {
                        let msg = format!("when condition error: {:#}", e);
                        eprintln!("Step '{}' failed: {}", step_name, msg);
                        failed_count += 1;
                        record_failure(&mut outcomes, &mut outputs, &mut errors, &step_name, msg);
                        continue;
                    }
                }
            }

            if let Some(ref for_each_expr) = step.for_each {
                let items = match evaluate_for_each(for_each_expr, &ctx) {
                    Ok(items) => items,
                    Err(e) => {
                        let msg = format!("for_each expression error: {:#}", e);
                        eprintln!("Step '{}' failed: {}", step_name, msg);
                        failed_count += 1;
                        record_failure(&mut outcomes, &mut outputs, &mut errors, &step_name, msg);
                        continue;
                    }
                };
                if items.is_empty() {
                    eprintln!(
                        "--- Step: {} [SKIPPED] ({}) ---",
                        step_name,
                        skip_label(SkipReason::Empty)
                    );
                    outcomes.insert(step_name.clone(), DepOutcome::Skipped);
                    outputs.insert(step_name, None);
                    continue;
                }

                eprintln!(
                    "--- Step: {} (action: {}, {} iterations) ---",
                    step_name,
                    step.action,
                    items.len()
                );

                let mut iter_outputs = Vec::new();
                let mut any_failed = false;

                for (idx, item) in items.iter().enumerate() {
                    if cancel_token.is_cancelled() {
                        bail!("Cancelled by user");
                    }

                    eprintln!("  [{}] iteration {}/{}", step_name, idx + 1, items.len());

                    let mut iter_ctx = ctx.clone();
                    if let Some(obj) = iter_ctx.as_object_mut() {
                        obj.insert("each".to_string(), json!({ "item": item, "index": idx }));
                    }

                    match execute_step(
                        &step_name,
                        step,
                        action,
                        config,
                        &iter_ctx,
                        &runner,
                        workspace_path,
                        cancel_token,
                    )
                    .await
                    {
                        Ok(result) => {
                            if result.success() {
                                iter_outputs.push(result.output.clone().unwrap_or(json!(null)));
                            } else {
                                eprintln!(
                                    "  [{}] iteration {} failed (exit {})",
                                    step_name,
                                    idx + 1,
                                    result.exit_code
                                );
                                any_failed = true;
                                failed_count += 1;
                                if !step.continue_on_failure {
                                    break;
                                }
                                iter_outputs.push(json!(null));
                            }
                        }
                        Err(e) => {
                            eprintln!("  [{}] iteration {} error: {:#}", step_name, idx + 1, e);
                            any_failed = true;
                            failed_count += 1;
                            if !step.continue_on_failure {
                                break;
                            }
                            iter_outputs.push(json!(null));
                        }
                    }
                }

                if any_failed && !step.continue_on_failure {
                    record_failed_loop(
                        &mut outcomes,
                        &mut outputs,
                        &mut errors,
                        &step_name,
                        iter_outputs,
                        "for_each loop failed".to_string(),
                    );
                } else {
                    outcomes.insert(step_name.clone(), DepOutcome::Completed);
                    outputs.insert(step_name, Some(json!(iter_outputs)));
                }
                continue;
            }

            eprintln!("--- Step: {} (action: {}) ---", step_name, step.action);
            match execute_step(
                &step_name,
                step,
                action,
                config,
                &ctx,
                &runner,
                workspace_path,
                cancel_token,
            )
            .await
            {
                Ok(result) if result.success() => {
                    outcomes.insert(step_name.clone(), DepOutcome::Completed);
                    outputs.insert(step_name, result.output);
                }
                Ok(result) => {
                    eprintln!(
                        "Step '{}' failed (exit code {})",
                        step_name, result.exit_code
                    );
                    let mut msg = format!("Exit code: {}", result.exit_code);
                    if !result.stderr.is_empty() {
                        eprintln!("stderr: {}", result.stderr);
                        msg.push_str(&format!("\nStderr: {}", result.stderr));
                    }
                    failed_count += 1;
                    record_failure(&mut outcomes, &mut outputs, &mut errors, &step_name, msg);
                }
                Err(e) => {
                    eprintln!("Step '{}' error: {:#}", step_name, e);
                    failed_count += 1;
                    record_failure(
                        &mut outcomes,
                        &mut outputs,
                        &mut errors,
                        &step_name,
                        format!("{:#}", e),
                    );
                }
            }
        }
    }

    // Counts are diagnostic and keep 0.16's arithmetic (spec §6). `Skipped`
    // (own `when`/empty `for_each`) and `Omitted` (blocked by a dependency's
    // outcome) are both "skipped" for this diagnostic line.
    let skipped = outcomes
        .values()
        .filter(|o| matches!(o, DepOutcome::Skipped | DepOutcome::Omitted))
        .count();
    let completed = outcomes
        .len()
        .saturating_sub(skipped)
        .saturating_sub(failed_count);
    // Spec 2026-10-01 §6: a failed step's own `continue_on_failure` is the
    // only thing that excuses it — no more structural "caught somewhere
    // downstream" walk (`caught_steps`, deleted). Mirrors
    // `settlement/settle.rs::decide`.
    let uncaught = outcomes.iter().any(|(name, o)| {
        *o == DepOutcome::Failed
            && !task
                .flow
                .get(name)
                .is_some_and(|fs| fs.continue_on_failure)
    });
    Ok(RunSummary {
        completed,
        skipped,
        failed: failed_count,
        outcome: if uncaught {
            RunOutcome::Failed
        } else {
            RunOutcome::Completed
        },
    })
}

/// Execute a single step via ShellRunner.
#[allow(clippy::too_many_arguments)]
async fn execute_step(
    step_name: &str,
    step: &FlowStep,
    action: &ActionDef,
    config: &WorkspaceConfig,
    ctx: &serde_json::Value,
    runner: &ShellRunner,
    workspace_path: &Path,
    cancel_token: &CancellationToken,
) -> Result<RunResult> {
    // Render step input
    let rendered_input = render_input_map(&step.input, ctx)
        .with_context(|| format!("Step '{}': failed to render input", step_name))?;

    // Prepare action input (merge action defaults + resolve connections)
    let lookup = stroem_common::template::SingleWorkspace {
        name: "local",
        config,
    };
    let action_input = prepare_action_input(&rendered_input, &action.input, &lookup)
        .with_context(|| format!("Step '{}': failed to prepare action input", step_name))?;

    // Build context for script/env rendering: full step context + rendered input
    // The action's script templates may reference step outputs (e.g., {{ step1.output.val }})
    // and loop variables ({{ each.item }}), so we need the full context, not just input+secret.
    let mut action_ctx = ctx.clone();
    if let Some(obj) = action_ctx.as_object_mut() {
        obj.insert("input".to_string(), action_input);
    }

    let run_config = build_run_config(step_name, action, &action_ctx, workspace_path)
        .with_context(|| format!("Step '{}': failed to build run config", step_name))?;

    let log_callback: LogCallback = Box::new(|line: LogLine| match line.stream {
        LogStream::Stdout => println!("{}", line.line),
        LogStream::Stderr => eprintln!("{}", line.line),
    });

    let timeout_duration = step
        .timeout
        .as_ref()
        .map(|t| std::time::Duration::from_secs(t.as_secs()));

    let run_result = if let Some(duration) = timeout_duration {
        match tokio::time::timeout(
            duration,
            runner.execute(run_config, Some(log_callback), cancel_token.clone()),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => bail!(
                "Step '{}' timed out after {}",
                step_name,
                step.timeout.as_ref().unwrap()
            ),
        }
    } else {
        runner
            .execute(run_config, Some(log_callback), cancel_token.clone())
            .await
    };

    run_result.with_context(|| format!("Step '{}': execution failed", step_name))
}

/// Build the render context for template evaluation.
///
/// Produces `{ "input": ..., "step_name": { "output": ... }, "secret": ... }`
/// with step names sanitized (hyphens → underscores) for Tera compatibility.
fn build_render_context(
    input: &serde_json::Value,
    outputs: &HashMap<String, Option<serde_json::Value>>,
    errors: &HashMap<String, String>,
    secrets: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    let mut ctx = serde_json::Map::new();
    ctx.insert("input".to_string(), input.clone());

    for (step_name, output) in outputs {
        let sanitized = step_name.replace('-', "_");
        let mut entry = serde_json::Map::new();
        entry.insert("output".into(), output.clone().unwrap_or(json!(null)));
        if let Some(err) = errors.get(step_name) {
            entry.insert("error".into(), json!(err));
        }
        ctx.insert(sanitized, serde_json::Value::Object(entry));
    }

    ctx.insert(
        "secret".to_string(),
        serde_json::to_value(secrets).unwrap_or(json!({})),
    );

    serde_json::Value::Object(ctx)
}

/// Build a RunConfig from an ActionDef for local execution.
fn build_run_config(
    step_name: &str,
    action: &ActionDef,
    ctx: &serde_json::Value,
    workspace_path: &Path,
) -> Result<RunConfig> {
    let workdir = workspace_path.to_string_lossy().to_string();

    // Render inline script
    let cmd = match &action.script {
        Some(s) => Some(render_template(s, ctx).context("Failed to render script template")?),
        None => None,
    };

    // Resolve source path relative to workspace
    let script = match &action.source {
        Some(src) => {
            let rendered =
                render_template(src, ctx).context("Failed to render source path template")?;
            let src_path = workspace_path.join(&rendered);
            let canonical = src_path.canonicalize();
            if let Ok(ref canon) = canonical {
                if !canon.starts_with(workspace_path) {
                    bail!(
                        "source path '{}' resolves outside workspace directory",
                        rendered
                    );
                }
                Some(canon.to_string_lossy().to_string())
            } else {
                eprintln!(
                    "  WARN: source path '{}' does not exist, cannot verify it stays within workspace",
                    rendered
                );
                Some(src_path.to_string_lossy().to_string())
            }
        }
        None => None,
    };

    // Render env vars
    let mut env = match &action.env {
        Some(env_map) => render_env_map(env_map, ctx).context("Failed to render env vars")?,
        None => HashMap::new(),
    };

    // Inject standard env vars
    env.insert("STROEM_STEP_NAME".to_string(), step_name.to_string());

    // Render args templates
    let args: Vec<String> = action
        .args
        .iter()
        .map(|a| render_template(a, ctx).context("Failed to render args template"))
        .collect::<Result<Vec<_>>>()?;

    Ok(RunConfig {
        cmd,
        script,
        env,
        workdir,
        action_type: "script".to_string(),
        image: None,
        runner_mode: RunnerMode::WithWorkspace,
        runner_image: None,
        entrypoint: None,
        command: None,
        pod_manifest_overrides: None,
        language: action.language.clone(),
        dependencies: action.dependencies.clone(),
        interpreter: action.interpreter.clone(),
        args,
        state_dir: None,
        state_out_dir: None,
        global_state_dir: None,
        global_state_out_dir: None,
        artifacts_out_dir: None,
    })
}

/// Evaluate a for_each expression into a Vec of items.
fn evaluate_for_each(
    expr: &serde_json::Value,
    ctx: &serde_json::Value,
) -> Result<Vec<serde_json::Value>> {
    const MAX_FOR_EACH_ITEMS: usize = 10_000;

    let arr = match expr {
        serde_json::Value::Array(arr) => arr.clone(),
        serde_json::Value::String(s) => {
            let rendered = render_template(s, ctx).context("Failed to render for_each template")?;
            let parsed: serde_json::Value = serde_json::from_str(&rendered).with_context(|| {
                format!(
                    "for_each template rendered to '{}' which is not valid JSON",
                    rendered
                )
            })?;
            match parsed {
                serde_json::Value::Array(arr) => arr,
                _ => bail!("for_each expression must evaluate to an array"),
            }
        }
        _ => bail!("for_each must be a string template or a JSON array"),
    };

    if arr.len() > MAX_FOR_EACH_ITEMS {
        bail!(
            "for_each produced {} items (max {})",
            arr.len(),
            MAX_FOR_EACH_ITEMS
        );
    }

    Ok(arr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use stroem_common::models::workflow::{ActionDef, FlowStep};

    fn make_action(action_type: &str) -> ActionDef {
        ActionDef {
            action_type: action_type.to_string(),
            name: None,
            description: None,
            task: None,
            cmd: None,
            script: Some("echo hello".to_string()),
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

    fn make_step(action: &str, depends_on: Vec<&str>) -> FlowStep {
        FlowStep {
            action: action.to_string(),
            name: None,
            description: None,
            depends_on: depends_on
                .iter()
                .map(|s| stroem_common::depends_on::DependsOnEntry::Name(s.to_string()))
                .collect(),
            input: HashMap::new(),
            continue_on_failure: false,
            legacy_continue_when_skipped: None,
            timeout: None,
            when: None,
            for_each: None,
            sequential: false,
            retry: None,
            inline_action: None,
        }
    }

    // --- build_render_context tests ---

    #[test]
    fn test_build_render_context_basic() {
        let input = json!({"env": "staging"});
        let mut outputs = HashMap::new();
        outputs.insert("step1".to_string(), Some(json!({"result": "ok"})));
        let secrets = HashMap::new();

        let ctx = build_render_context(&input, &outputs, &HashMap::new(), &secrets);
        assert_eq!(ctx["input"]["env"], "staging");
        assert_eq!(ctx["step1"]["output"]["result"], "ok");
    }

    #[test]
    fn test_build_render_context_hyphen_sanitization() {
        let input = json!({});
        let mut outputs = HashMap::new();
        outputs.insert("say-hello".to_string(), Some(json!({"greeting": "hi"})));
        let secrets = HashMap::new();

        let ctx = build_render_context(&input, &outputs, &HashMap::new(), &secrets);
        // Hyphenated name is sanitized to underscore
        assert_eq!(ctx["say_hello"]["output"]["greeting"], "hi");
    }

    #[test]
    fn test_build_render_context_with_secrets() {
        let input = json!({});
        let outputs = HashMap::new();
        let mut secrets = HashMap::new();
        secrets.insert("api_key".to_string(), json!("secret123"));

        let ctx = build_render_context(&input, &outputs, &HashMap::new(), &secrets);
        assert_eq!(ctx["secret"]["api_key"], "secret123");
    }

    #[test]
    fn test_build_render_context_null_output() {
        let input = json!({});
        let mut outputs = HashMap::new();
        outputs.insert("skipped_step".to_string(), None);
        let secrets = HashMap::new();

        let ctx = build_render_context(&input, &outputs, &HashMap::new(), &secrets);
        assert!(ctx["skipped_step"]["output"].is_null());
    }

    #[test]
    fn test_build_render_context_failed_step_exposes_error_not_output() {
        let input = json!({});
        let mut outputs = HashMap::new();
        outputs.insert("s".to_string(), None);
        outputs.insert("ok_step".to_string(), Some(json!({"result": "done"})));
        let mut errors = HashMap::new();
        errors.insert("s".to_string(), "boom".to_string());
        let secrets = HashMap::new();

        let ctx = build_render_context(&input, &outputs, &errors, &secrets);
        assert!(ctx["s"]["output"].is_null());
        assert_eq!(ctx["s"]["error"], "boom");
        assert!(ctx["ok_step"].get("error").is_none());
    }

    // --- build_run_config tests ---

    #[test]
    fn test_build_run_config_inline_script() {
        let action = ActionDef {
            script: Some("echo {{ input.msg }}".to_string()),
            ..make_action("script")
        };
        let ctx = json!({"input": {"msg": "hello"}, "secret": {}});
        let config = build_run_config("test-step", &action, &ctx, Path::new(".")).unwrap();

        assert_eq!(config.cmd.as_deref(), Some("echo hello"));
        assert!(config.script.is_none());
        assert_eq!(config.env["STROEM_STEP_NAME"], "test-step");
        assert_eq!(config.action_type, "script");
    }

    #[test]
    fn test_build_run_config_source_path() {
        let action = ActionDef {
            script: None,
            source: Some("scripts/deploy.sh".to_string()),
            ..make_action("script")
        };
        let ctx = json!({"input": {}, "secret": {}});
        let config = build_run_config("deploy", &action, &ctx, Path::new(".")).unwrap();

        assert!(config.cmd.is_none());
        assert!(config.script.is_some());
        let script_path = config.script.unwrap();
        assert!(script_path.contains("scripts/deploy.sh") || script_path.ends_with("deploy.sh"));
    }

    #[test]
    fn test_build_run_config_with_env() {
        let mut env = HashMap::new();
        env.insert("DB_HOST".to_string(), "{{ input.host }}".to_string());
        let action = ActionDef {
            env: Some(env),
            ..make_action("script")
        };
        let ctx = json!({"input": {"host": "localhost"}, "secret": {}});
        let config = build_run_config("step1", &action, &ctx, Path::new(".")).unwrap();

        assert_eq!(config.env["DB_HOST"], "localhost");
    }

    #[test]
    fn test_build_run_config_language_and_deps() {
        let action = ActionDef {
            language: Some("python".to_string()),
            dependencies: vec!["requests".to_string()],
            interpreter: Some("/usr/bin/python3".to_string()),
            ..make_action("script")
        };
        let ctx = json!({"input": {}, "secret": {}});
        let config = build_run_config("py-step", &action, &ctx, Path::new(".")).unwrap();

        assert_eq!(config.language.as_deref(), Some("python"));
        assert_eq!(config.dependencies, vec!["requests"]);
        assert_eq!(config.interpreter.as_deref(), Some("/usr/bin/python3"));
    }

    #[test]
    fn test_build_run_config_renders_args_templates() {
        let mut action = make_action("script");
        action.args = vec!["{{ input.env }}".to_string(), "--flag".to_string()];
        let ctx = json!({"input": {"env": "staging"}, "secret": {}});
        let workspace = std::path::Path::new("/tmp/test-workspace");

        let config = build_run_config("step1", &action, &ctx, workspace).unwrap();
        assert_eq!(config.args, vec!["staging", "--flag"]);
    }

    #[test]
    fn test_build_run_config_args_template_error_propagated() {
        let mut action = make_action("script");
        // Use an invalid Tera expression to trigger an error
        action.args = vec!["{{ invalid | nonexistent_filter }}".to_string()];
        let ctx = json!({"input": {}, "secret": {}});
        let workspace = std::path::Path::new("/tmp/test-workspace");

        let result = build_run_config("step1", &action, &ctx, workspace);
        assert!(result.is_err(), "template error should propagate");
    }

    // --- validate_actions_local tests ---

    #[test]
    fn test_validate_actions_rejects_docker() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("build", vec![]));
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("build".to_string(), make_action("docker"));

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("type 'docker'"));
    }

    #[test]
    fn test_validate_actions_rejects_pod() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("deploy", vec![]));
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("deploy".to_string(), make_action("pod"));

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("type 'pod'"));
    }

    #[test]
    fn test_validate_actions_rejects_task() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("sub", vec![]));
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("sub".to_string(), make_action("task"));

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("type 'task'"));
    }

    #[test]
    fn test_validate_actions_rejects_agent() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("llm", vec![]));
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("llm".to_string(), make_action("agent"));

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("type 'agent'"));
    }

    #[test]
    fn test_validate_actions_rejects_approval() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("gate", vec![]));
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("gate".to_string(), make_action("approval"));

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("type 'approval'"));
    }

    #[test]
    fn test_validate_actions_rejects_docker_runner() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("build", vec![]));
        let mut config = WorkspaceConfig::new();
        let mut action = make_action("script");
        action.runner = Some("docker".to_string());
        config.actions.insert("build".to_string(), action);

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("runner 'docker'"));
    }

    #[test]
    fn test_validate_actions_accepts_local_script() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("greet", vec![]));
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("greet".to_string(), make_action("script"));

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_actions_unknown_action() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("nonexistent", vec![]));
        let config = WorkspaceConfig::new();

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown action"));
    }

    // --- evaluate_for_each tests ---

    #[test]
    fn test_evaluate_for_each_literal_array() {
        let expr = json!(["a", "b", "c"]);
        let ctx = json!({});
        let result = evaluate_for_each(&expr, &ctx).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "a");
    }

    #[test]
    fn test_evaluate_for_each_template_string() {
        let expr = json!("{{ input.items | json_encode() }}");
        let ctx = json!({"input": {"items": [1, 2, 3]}});
        let result = evaluate_for_each(&expr, &ctx).unwrap();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_evaluate_for_each_non_array_errors() {
        let expr = json!("not a template");
        let ctx = json!({});
        // "not a template" renders as-is, which is not valid JSON
        let result = evaluate_for_each(&expr, &ctx);
        assert!(result.is_err());
    }

    // --- run_dag / dependency-gate tests ---

    async fn run_yaml(yaml: &str, task: &str) -> RunSummary {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.yaml"), yaml).unwrap();
        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        run_dag(
            &config.tasks[task],
            &config,
            &json!({}),
            dir.path(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_run_independent_branch_continues_after_failure() {
        let s = run_yaml(
            r#"
actions:
  fail: { type: script, script: exit 1 }
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      a: { action: fail }
      b: { action: ok, depends_on: [a] }
      z: { action: ok }
"#,
            "t",
        )
        .await;
        assert_eq!((s.completed, s.skipped, s.failed), (1, 1, 1));
        assert_eq!(s.outcome, RunOutcome::Failed);
    }

    // `test_run_failure_caught_downstream_completes` is retired, NOT replaced
    // with a mechanical equivalent — a genuinely ambiguous case, reported
    // here rather than guessed at (surprise finding beyond this task's named
    // fallout: run.rs:1055,1268 — see the Task 6 report).
    //
    // Pre-0.18.0 this exercised the deleted `caught_steps` structural walk
    // TWICE over, in a way the new per-edge `accept` model cannot reproduce
    // with one obvious equivalent:
    //   1. `a` (own cof: false) failed; `b`'s gate read A's OWN cof (false)
    //      — not b's own cof: true — so `b` was skipped (Unreachable), never
    //      ran, regardless of its own flag.
    //   2. `c`'s gate then read the SKIPPED `b`'s own cof (true) and treated
    //      that as license to run past a `Skipped(Unreachable)` dependency —
    //      `c` completed without ever seeing `b` actually execute.
    //   3. Job status: `caught_steps` walked a's dependents (`[b]`) and
    //      found `b.continue_on_failure == true`, so `a` itself counted as
    //      "caught" — the job completed despite the (real, upstream) failure
    //      never being excused by its OWN flag.
    // None of these three steps exists anymore: `gate()` now reads only the
    // DEPENDENT's own `accept` set, never a dependency's flags; job status
    // (`settlement/settle.rs::decide`, mirrored above) now reads only the
    // FAILING row's own `continue_on_failure`. Re-expressing "b still runs"
    // needs `accept: [failed]` on b's edge to a — straightforward — but
    // getting the OVERALL RUN back to `Completed` needs `a` to carry its
    // OWN `continue_on_failure: true` instead of `b`'s, which moves the
    // flag to a different step than the test originally authored and changes
    // what the test is actually demonstrating. Two non-equivalent rewrites
    // are defensible; picking one is a design call, not a mechanical port.

    // `test_run_merge_needs_cws_on_skipped_branch` (pre-0.18.0: asserted that
    // `y`'s own `continue_when_skipped: true` let `m` run despite `y` being
    // skipped) is retired, not mechanically renamed: `continue_when_skipped`
    // is no longer read for ANY behavior (`legacy_continue_when_skipped` is
    // detection-only, spec 2026-10-01 §6 — `m` would now cascade-omit
    // regardless of the flag's value, since nothing on `y` can affect `m`
    // anymore; only `m`'s OWN `depends_on` entry can). The equivalent is
    // `test_run_accept_skipped_replaces_retired_continue_when_skipped` below,
    // via `accept: [skipped]` on the dependent.
    #[tokio::test]
    async fn test_run_when_error_fails_the_step_not_the_run() {
        let s = run_yaml(
            r#"
actions:
  ok: { type: script, script: echo ok }
tasks:
  t:
    flow:
      bad: { action: ok, when: "{{ nope.nope }}" }
      z: { action: ok }
"#,
            "t",
        )
        .await;
        assert_eq!((s.completed, s.failed), (1, 1));
    }

    #[tokio::test]
    async fn test_run_failed_loop_output_is_exposed_to_an_accepting_dependent_cof_loop_unaffected() {
        // Pre-0.18.0 this asserted the OPPOSITE of the current intent — that
        // L's failed-loop output stayed masked as `null` downstream, and that
        // B's OWN `continue_on_failure` let it run past L's failure
        // (`caught_steps`-era semantics, see the retired test above). Both
        // premises are gone: `continue_on_failure` is self-scoped (spec §6),
        // so B must `accept: [failed]` on its edge to L; and spec §4/§12's
        // loop-output fix means L's PARTIAL per-iteration output (produced
        // before it stopped on "bad", the first item) is now exposed to C,
        // not masked to `null` — this is the scope of Task 6 itself. K (own
        // `continue_on_failure`) still completes normally with one null per
        // instance, unaffected, and D reads its length as before.
        let s = run_yaml(
            r#"
actions:
  ok: { type: script, script: "true" }
  item: { type: script, script: "test '{{ each.item }}' = good" }
  check:
    type: script
    input:
      c: { type: string }
      want: { type: string }
    script: "test '{{ input.c }}' = '{{ input.want }}'"
tasks:
  t:
    flow:
      L: { action: item, for_each: ["bad", "good"] }
      B:
        action: ok
        depends_on:
          - step: L
            accept: [failed]
      C: { action: check, depends_on: [B], input: { c: "{{ L.output | json_encode() }}", want: "[]" } }
      K: { action: item, for_each: ["bad", "good"], continue_on_failure: true }
      D: { action: check, depends_on: [K], input: { c: "{{ K.output | length }}", want: "2" } }
"#,
            "t",
        )
        .await;
        // `completed`/`failed` are diagnostic counters, not per-step outcome
        // tallies (`failed` counts every failing EXECUTION, including a
        // tolerated loop's own failing iteration — K's "bad" iteration
        // contributes to `failed` even though K's own outcome is Completed;
        // see "Counts are diagnostic..." above `run_dag`'s return). Five
        // steps total, none skipped, two failing executions (L's stopping
        // iteration + K's tolerated one) → completed = 5 - 0 - 2 = 3.
        assert_eq!(s.failed, 2, "L's stopping iteration + K's tolerated one");
        assert_eq!(s.skipped, 0);
        assert_eq!(s.completed, 3);
        // L has no `continue_on_failure` of its own (self-scoped, spec §6):
        // B and C accepting/reading its failure does not excuse it — the
        // overall run still fails.
        assert_eq!(s.outcome, RunOutcome::Failed);
    }

    /// CLI mirror of the server-side four-layer loop-output fix
    /// (cascade.rs's `phase_rollup` R6 + `Snapshot::apply` + `render_context.rs`'s
    /// scoped failed-row branch): a `for_each` loop that fails WITHOUT its own
    /// `continue_on_failure` still exposes whatever per-iteration output it
    /// produced before stopping — it is no longer silently dropped to `null` —
    /// to a dependent that explicitly accepts the failed outcome. Exercises the
    /// CLI's own rollup path end to end: `record_failed_loop` (the CLI's
    /// layers 2+3 — in-memory "snapshot" and the only persistence this
    /// local runner has) feeding `build_render_context` (layer 4).
    #[tokio::test]
    async fn cli_for_each_rollup_exposes_output_on_a_tolerated_failure_too() {
        let s = run_yaml(
            r#"
actions:
  item: { type: script, script: "test '{{ each.item }}' = good" }
  check:
    type: script
    input:
      c: { type: string }
    script: "test '{{ input.c }}' = '[null]'"
tasks:
  t:
    flow:
      L: { action: item, for_each: ["good", "bad", "good"] }
      C:
        action: check
        depends_on:
          - step: L
            accept: [failed]
        input: { c: "{{ L.output | json_encode() }}" }
"#,
            "t",
        )
        .await;
        assert_eq!(s.failed, 1, "L's bad iteration fails the loop");
        assert_eq!(
            s.completed, 1,
            "C ran and its check passed against L's partial (non-null) output array"
        );
        // L has no `continue_on_failure` of its own (self-scoped, spec §6) —
        // tolerated-by-C or not, L's failure still fails the overall run.
        assert_eq!(s.outcome, RunOutcome::Failed);
    }

    // --- Integration tests ---

    #[tokio::test]
    async fn test_run_simple_task() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo "hello world"
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["hello"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
    }

    #[tokio::test]
    async fn test_run_two_step_chain() {
        let dir = tempfile::tempdir().unwrap();
        // Note: YAML requires quoting strings containing {{ }} to avoid
        // ambiguity with YAML flow mapping syntax.
        std::fs::write(
            dir.path().join("test.yaml"),
            "actions:\n  produce:\n    type: script\n    script: \"echo 'OUTPUT: {\\\"val\\\":\\\"42\\\"}'\"\n  consume:\n    type: script\n    script: \"echo got {{ produce.output.val }}\"\ntasks:\n  chain:\n    flow:\n      produce:\n        action: produce\n      consume:\n        action: consume\n        depends_on: [produce]\n",
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["chain"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn test_run_failing_step() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  fail:
    type: script
    script: exit 1
tasks:
  bad:
    flow:
      step1:
        action: fail
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["bad"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.failed, 1);
    }

    #[tokio::test]
    async fn test_run_when_false_skips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo hello
tasks:
  conditional:
    flow:
      step1:
        action: greet
        when: "false"
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["conditional"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.skipped, 1);
    }

    // `test_run_continue_when_skipped_runs_after_false_condition` (pre-0.18.0:
    // a dependency's own `continue_when_skipped: true` let its dependent run
    // despite the dependency being skipped) is retired for the same reason as
    // the test above: the flag has no live effect anymore. Replaced by the
    // test immediately below, which pins the identical intent — "a dependent
    // can tolerate its dependency's own-choice skip" — through the new
    // model's `accept: [skipped]` on the DEPENDENT instead.
    #[tokio::test]
    async fn test_run_accept_skipped_replaces_retired_continue_when_skipped() {
        // `y` is skipped by its own `when`. `report`'s depends_on entry
        // explicitly accepts that skip and runs; `follow`'s bare-name entry
        // defaults to `accept: [completed]` and is cascade-omitted.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo hello
tasks:
  conditional:
    flow:
      y:
        action: greet
        when: "false"
      report:
        action: greet
        depends_on:
          - step: y
            accept: [skipped]
      follow:
        action: greet
        depends_on: [y]
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["conditional"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 1, "report ran (its accept tolerates y's skip)");
        assert_eq!(
            summary.skipped, 2,
            "y (own condition) and follow (omitted by the gate)"
        );
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn test_run_for_each() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  print:
    type: script
    script: echo "item={{ each.item }}"
tasks:
  loop_task:
    flow:
      step1:
        action: print
        for_each: ["a", "b", "c"]
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["loop_task"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.failed, 0);
    }

    #[test]
    fn test_run_missing_task() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo hi
tasks:
  hello:
    flow:
      step1:
        action: greet
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        assert!(!config.tasks.contains_key("nonexistent"));
    }

    #[test]
    fn test_run_unsupported_type_rejected() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("docker-build", vec![]));
        let mut config = WorkspaceConfig::new();
        config
            .actions
            .insert("docker-build".to_string(), make_action("docker"));

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
    }

    // --- Additional validate_actions_local tests ---

    #[test]
    fn test_validate_actions_rejects_pod_runner() {
        let mut flow = HashMap::new();
        flow.insert("s1".to_string(), make_step("deploy", vec![]));
        let mut config = WorkspaceConfig::new();
        let mut action = make_action("script");
        action.runner = Some("pod".to_string());
        config.actions.insert("deploy".to_string(), action);

        let result = validate_actions_local(&flow, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("runner 'pod'"));
    }

    // --- Additional evaluate_for_each tests ---

    #[test]
    fn test_evaluate_for_each_json_object_errors() {
        // A template that renders to a JSON object (not array) must error.
        let expr = json!("{{ input.data | json_encode() }}");
        let ctx = json!({"input": {"data": {"key": "val"}}});
        let result = evaluate_for_each(&expr, &ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must evaluate to an array"),
            "error should mention 'must evaluate to an array'"
        );
    }

    #[test]
    fn test_evaluate_for_each_exceeds_limit() {
        // A literal array with more than 10,000 items must error.
        let big_array: Vec<serde_json::Value> = vec![json!(1); 10_001];
        let expr = json!(big_array);
        let ctx = json!({});
        let result = evaluate_for_each(&expr, &ctx);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("max 10000"),
            "error should mention 'max 10000'"
        );
    }

    // --- Additional integration tests ---

    #[tokio::test]
    async fn test_run_continue_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  fail:
    type: script
    script: exit 1
  greet:
    type: script
    script: echo hello
tasks:
  test:
    flow:
      step1:
        action: fail
        continue_on_failure: true
      step2:
        action: greet
        # Pre-0.18.0 this ran because step1's OWN continue_on_failure caught
        # it for every dependent, structurally (`caught_steps`, deleted).
        # continue_on_failure is now self-scoped only (spec §6): step2 must
        # explicitly accept step1's failed outcome to still run.
        depends_on:
          - step: step1
            accept: [failed]
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["test"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 1, "step2 should complete");
        assert_eq!(summary.failed, 1, "step1 should fail");
        assert_eq!(summary.skipped, 0, "no steps should be skipped");
    }

    #[tokio::test]
    async fn test_run_for_each_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  greet:
    type: script
    script: echo hello
tasks:
  test:
    flow:
      step1:
        action: greet
        for_each: []
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["test"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 0, "no steps should complete");
        assert_eq!(
            summary.skipped, 1,
            "step1 should be skipped (empty for_each)"
        );
        assert_eq!(summary.failed, 0, "no steps should fail");
    }

    #[tokio::test]
    async fn test_run_when_with_step_output() {
        // step1 outputs {"deploy": "true", "skip": "false"}.
        // step2 has when: "{{ step1.output.deploy }}" — truthy, should run.
        // step3 has when: "{{ step1.output.skip }}" — "false" is falsy, should be skipped.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            "actions:\n  produce:\n    type: script\n    script: \"echo 'OUTPUT: {\\\"deploy\\\":\\\"true\\\",\\\"skip\\\":\\\"false\\\"}'\"\n  consume:\n    type: script\n    script: echo deployed\ntasks:\n  test:\n    flow:\n      step1:\n        action: produce\n      step2:\n        action: consume\n        depends_on: [step1]\n        when: \"{{ step1.output.deploy }}\"\n      step3:\n        action: consume\n        depends_on: [step1]\n        when: \"{{ step1.output.skip }}\"\n",
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["test"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.completed, 2, "step1 and step2 should complete");
        assert_eq!(
            summary.skipped, 1,
            "step3 should be skipped (condition false)"
        );
        assert_eq!(summary.failed, 0, "no steps should fail");
    }

    #[tokio::test]
    async fn test_run_for_each_continue_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            "actions:\n  check:\n    type: script\n    script: \"test '{{ each.item }}' != bad\"\ntasks:\n  test:\n    flow:\n      step1:\n        action: check\n        for_each: [\"ok\", \"bad\", \"fine\"]\n        continue_on_failure: true\n",
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["test"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(summary.failed, 1, "one iteration should fail");
        assert_eq!(summary.skipped, 0, "no steps should be skipped");
    }

    // --- Error path tests ---

    #[test]
    fn test_err_nonexistent_workspace_path() {
        let result = workspace_loader::load_workspace(Path::new("/tmp/nonexistent_stroem_99999"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("does not exist") || msg.contains("nonexistent_stroem_99999"),
            "Expected path-not-found error, got: {}",
            msg
        );
    }

    #[test]
    fn test_err_invalid_input_json() {
        let result = serde_json::from_str::<serde_json::Value>("not-json");
        assert!(
            result.is_err(),
            "Malformed JSON input must be rejected by serde_json"
        );
    }

    #[test]
    fn test_err_cyclic_dag_rejected() {
        let mut flow = HashMap::new();
        flow.insert("a".to_string(), make_step("act", vec!["b"]));
        flow.insert("b".to_string(), make_step("act", vec!["a"]));
        let result = dag::validate_dag(&flow);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Cycle detected"),
            "Expected 'Cycle detected' in dag error message"
        );
    }

    // --- Integration tests ---

    #[tokio::test]
    async fn test_integ_for_each_output_accessible_downstream() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            "actions:\n  produce:\n    type: script\n    script: \"echo 'OUTPUT: {\\\"val\\\":\\\"{{ each.item }}\\\"}'\"\n  consume:\n    type: script\n    script: echo done\ntasks:\n  test:\n    flow:\n      loop:\n        action: produce\n        for_each: [\"x\", \"y\"]\n      after:\n        action: consume\n        depends_on: [loop]\n",
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["test"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();

        assert_eq!(
            summary.completed, 2,
            "loop placeholder and after step should both complete"
        );
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
    }

    #[tokio::test]
    async fn test_integ_workspace_path_used_as_workdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            "actions:\n  check_dir:\n    type: script\n    script: pwd\ntasks:\n  test:\n    flow:\n      s1:\n        action: check_dir\n",
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["test"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();

        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn test_integ_cascade_skip_diamond_dag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            "actions:\n  greet:\n    type: script\n    script: echo hello\ntasks:\n  test:\n    flow:\n      root:\n        action: greet\n        when: \"false\"\n      left:\n        action: greet\n        depends_on: [root]\n      right:\n        action: greet\n        depends_on: [root]\n      join:\n        action: greet\n        depends_on: [left, right]\n",
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["test"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();

        assert_eq!(summary.completed, 0, "No steps should run to completion");
        assert_eq!(summary.skipped, 4, "All four steps must be cascade-skipped");
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn test_run_for_each_all_fail_continue_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.yaml"),
            r#"
actions:
  fail:
    type: script
    script: exit 1
tasks:
  test:
    flow:
      step1:
        action: fail
        for_each: ["a", "b", "c"]
        continue_on_failure: true
"#,
        )
        .unwrap();

        let (config, _) = workspace_loader::load_workspace(dir.path()).unwrap();
        let task = &config.tasks["test"];
        let input = json!({});
        let cancel = CancellationToken::new();

        let summary = run_dag(task, &config, &input, dir.path(), &cancel)
            .await
            .unwrap();
        // 3 iterations all fail — completed_count should be 0 (not underflow)
        assert_eq!(
            summary.completed, 0,
            "no steps should complete successfully"
        );
        assert_eq!(summary.failed, 3, "all three iterations should fail");
        assert_eq!(summary.skipped, 0);
    }

    #[test]
    fn local_run_rejects_qualified_connection_with_server_hint() {
        let yaml = r#"
connection_types:
  pg:
    host: { type: string }
tasks:
  t:
    input:
      db: { type: pg, default: other.prod }
    flow:
      s:
        action: a
actions:
  a:
    type: script
    script: echo hi
"#;
        let config: stroem_common::models::workflow::WorkspaceConfig =
            serde_yaml::from_str(yaml).unwrap();
        let task = &config.tasks["t"];
        let merged = stroem_common::template::merge_defaults(
            &serde_json::json!({}),
            &task.input,
            &serde_json::json!({}),
        )
        .unwrap();
        let err = stroem_common::template::resolve_connection_inputs(
            &merged,
            &task.input,
            &stroem_common::template::SingleWorkspace {
                name: "local",
                config: &config,
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("require a server"), "{err:#}");
    }
}
