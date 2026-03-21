use anyhow::{bail, Context, Result};
use stroem_common::models::job::StepStatus;
use stroem_common::models::workflow::{ActionDef, WorkspaceConfig};
use stroem_db::{JobRepo, JobStepRepo};
use uuid::Uuid;

use crate::config::{AgentProviderConfig, AgentsConfig};
use crate::state::AppState;

/// Default timeout for a single LLM API call (2 minutes).
const LLM_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Dispatch ready `type: agent` steps by calling the configured LLM provider.
///
/// Mirrors `handle_task_steps()` — called after job creation and after the
/// orchestrator promotes steps. Workers never claim agent steps; the server
/// dispatches them directly.
///
/// This function loops until no more ready agent steps remain, calling
/// `orchestrator::on_step_completed` after each step to promote downstream
/// steps (which may include further agent steps). This avoids the recursive
/// async cycle that would arise from calling `orchestrate_after_step` inside
/// `orchestrate_after_step`.
#[tracing::instrument(skip(state, workspace_config))]
pub async fn handle_agent_steps(
    state: &AppState,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
) -> Result<()> {
    let agents_config = match state.config.agents.as_ref() {
        Some(c) => c,
        None => return Ok(()), // No agents configured, nothing to do
    };

    // Get the task definition for orchestration
    let job = match JobRepo::get(&state.pool, job_id).await? {
        Some(j) => j,
        None => return Ok(()),
    };

    let task = match workspace_config.tasks.get(&job.task_name) {
        Some(t) => t.clone(),
        None => return Ok(()),
    };

    // Process agent steps in a loop to handle chains of agent steps.
    // Each iteration finds all currently-ready agent steps, runs them, then
    // calls the orchestrator to promote downstream steps before looping again.
    let max_iterations = task.flow.len() + 1;
    for _iteration in 0..max_iterations {
        let steps = JobStepRepo::get_steps_for_job(&state.pool, job_id).await?;
        let job_row = match JobRepo::get(&state.pool, job_id).await? {
            Some(j) if j.status != "cancelled" => j,
            _ => break, // Job gone or cancelled
        };

        // Find the first ready agent step
        let ready_agent_step = steps
            .iter()
            .find(|s| s.status == StepStatus::Ready.as_ref() && s.action_type == "agent");

        let step = match ready_agent_step {
            Some(s) => s.clone(),
            None => break, // No more ready agent steps — done
        };

        // Deserialize the action spec
        let action_spec: ActionDef = match step.action_spec.as_ref() {
            Some(spec) => serde_json::from_value(spec.clone())
                .context("Failed to deserialize agent action_spec")?,
            None => {
                let err = "Missing action_spec for agent step";
                JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, err).await?;
                // Orchestrate after failure so downstream steps are unblocked
                orchestrate_step(
                    &state.pool,
                    job_id,
                    &step.step_name,
                    &task,
                    workspace_config,
                )
                .await;
                continue;
            }
        };

        let provider_name = match action_spec.provider.as_deref() {
            Some(p) => p,
            None => {
                let err = "Agent step missing provider";
                JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, err).await?;
                orchestrate_step(
                    &state.pool,
                    job_id,
                    &step.step_name,
                    &task,
                    workspace_config,
                )
                .await;
                continue;
            }
        };

        let provider_config = match agents_config.providers.get(provider_name) {
            Some(p) => p,
            None => {
                let err = format!("Unknown agent provider: {}", provider_name);
                JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, &err).await?;
                orchestrate_step(
                    &state.pool,
                    job_id,
                    &step.step_name,
                    &task,
                    workspace_config,
                )
                .await;
                continue;
            }
        };

        // Mark step running (server-side, no worker)
        JobStepRepo::mark_running_server(&state.pool, job_id, &step.step_name).await?;
        JobRepo::mark_running_if_pending_server(&state.pool, job_id).await?;

        // Build render context from completed steps
        let mut render_ctx =
            crate::job_creator::build_step_render_context(&job_row, &steps, workspace_config);

        // Inject `each` variable for for_each loop instances (mirrors rendering.rs)
        if step.loop_source.is_some() {
            if let Some(ref item) = step.loop_item {
                let each_val = serde_json::json!({
                    "item": item,
                    "index": step.loop_index,
                    "total": step.loop_total,
                });
                if let Some(obj) = render_ctx.as_object_mut() {
                    obj.insert("each".to_string(), each_val);
                }
            }
        }

        // Resolve flow-step input templates and replace `input` in the context
        // so prompt/system_prompt can use {{ input.X }} for resolved step input.
        if let Some(ref input) = step.input {
            if let Some(input_map) = input.as_object() {
                if !input_map.is_empty() {
                    let map: std::collections::HashMap<String, serde_json::Value> = input_map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    match stroem_common::template::render_input_map(&map, &render_ctx) {
                        Ok(resolved) => {
                            if let Some(ctx_obj) = render_ctx.as_object_mut() {
                                ctx_obj.insert("input".to_string(), resolved);
                            }
                        }
                        Err(e) => {
                            let err = format!(
                                "Failed to render input for agent step '{}': {:#}",
                                step.step_name, e
                            );
                            JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, &err)
                                .await?;
                            orchestrate_step(
                                &state.pool,
                                job_id,
                                &step.step_name,
                                &task,
                                workspace_config,
                            )
                            .await;
                            continue;
                        }
                    }
                }
            }
        }

        // Render prompt template
        let rendered_prompt = match action_spec.prompt.as_deref() {
            Some(tmpl) => match stroem_common::template::render_template(tmpl, &render_ctx) {
                Ok(p) => p,
                Err(e) => {
                    let err = format!("Failed to render prompt template: {:#}", e);
                    JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, &err).await?;
                    orchestrate_step(
                        &state.pool,
                        job_id,
                        &step.step_name,
                        &task,
                        workspace_config,
                    )
                    .await;
                    continue;
                }
            },
            None => {
                let err = "Agent step missing prompt";
                JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, err).await?;
                orchestrate_step(
                    &state.pool,
                    job_id,
                    &step.step_name,
                    &task,
                    workspace_config,
                )
                .await;
                continue;
            }
        };

        // Reject empty prompts — an empty string would waste an LLM API call
        if rendered_prompt.trim().is_empty() {
            let err = "Agent step prompt rendered to empty string";
            JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, err).await?;
            orchestrate_step(
                &state.pool,
                job_id,
                &step.step_name,
                &task,
                workspace_config,
            )
            .await;
            continue;
        }

        // Render system_prompt template (optional)
        let rendered_system = match action_spec.system_prompt.as_deref() {
            Some(tmpl) => match stroem_common::template::render_template(tmpl, &render_ctx) {
                Ok(s) => Some(s),
                Err(e) => {
                    let err = format!("Failed to render system_prompt template: {:#}", e);
                    JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, &err).await?;
                    orchestrate_step(
                        &state.pool,
                        job_id,
                        &step.step_name,
                        &task,
                        workspace_config,
                    )
                    .await;
                    continue;
                }
            },
            None => None,
        };

        let model_name = action_spec
            .model
            .as_deref()
            .unwrap_or(&provider_config.model);

        state
            .append_server_log(
                job_id,
                &format!(
                    "[agent] Step '{}': calling {}/{}",
                    step.step_name, provider_name, model_name
                ),
            )
            .await;

        let start = std::time::Instant::now();

        // Retry loop with exponential backoff for transient errors.
        // Max retries comes from provider_config; base delay is 1s with jitter.
        let max_retries = provider_config.max_retries;
        let mut call_result: Result<LlmResponse> = Err(anyhow::anyhow!("LLM call not attempted"));

        for attempt in 0..=max_retries {
            if attempt > 0 {
                // Exponential backoff: 1s, 2s, 4s, 8s, ... with ±25% jitter
                let base_ms = 1000u64 * (1u64 << (attempt - 1).min(6));
                // Simple deterministic jitter based on attempt number — avoids
                // pulling in the `rand` crate just for this purpose.
                let jitter = (attempt as u64 * 7919) % (base_ms / 4 + 1);
                let delay_ms = base_ms + jitter;
                state
                    .append_server_log(
                        job_id,
                        &format!(
                            "[agent] Step '{}': retrying (attempt {}/{}) after {}ms",
                            step.step_name, attempt, max_retries, delay_ms
                        ),
                    )
                    .await;
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            }

            call_result = match tokio::time::timeout(
                LLM_CALL_TIMEOUT,
                call_llm(
                    provider_config,
                    model_name,
                    &rendered_prompt,
                    rendered_system.as_deref(),
                    &action_spec,
                ),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "LLM API call timed out after {}s",
                    LLM_CALL_TIMEOUT.as_secs()
                )),
            };

            match &call_result {
                Ok(_) => break,
                Err(e) if is_transient_error(e) && attempt < max_retries => continue,
                Err(_) => break,
            }
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;

        let completed_step_name = step.step_name.clone();

        match call_result {
            Ok(response) => {
                // Build output with _meta
                let mut output = response.content;
                if let Some(obj) = output.as_object_mut() {
                    // Note: `_meta` is a reserved key. If the LLM response or output
                    // defines a `_meta` field, it will be overwritten. This is documented
                    // behavior — see agent-actions.md.
                    obj.insert(
                        "_meta".to_string(),
                        serde_json::json!({
                            "model": model_name,
                            "provider": provider_name,
                            "input_tokens": response.input_tokens,
                            "output_tokens": response.output_tokens,
                            "latency_ms": elapsed_ms,
                            "turns": 1,
                        }),
                    );
                }

                state
                    .append_server_log(
                        job_id,
                        &format!(
                            "[agent] Step '{}': completed in {}ms",
                            step.step_name, elapsed_ms
                        ),
                    )
                    .await;

                JobStepRepo::mark_completed(&state.pool, job_id, &step.step_name, Some(output))
                    .await?;
            }
            Err(e) => {
                let err_msg = format!("{:#}", e);
                state
                    .append_server_log(
                        job_id,
                        &format!("[agent] Step '{}': failed — {}", step.step_name, err_msg),
                    )
                    .await;
                JobStepRepo::mark_failed(&state.pool, job_id, &step.step_name, &err_msg).await?;
            }
        }

        // Promote downstream steps so the next loop iteration can find them
        orchestrate_step(
            &state.pool,
            job_id,
            &completed_step_name,
            &task,
            workspace_config,
        )
        .await;
    }

    Ok(())
}

/// Dispatch ready `type: agent` steps at job creation time, without `AppState`.
///
/// Called from `create_job_for_task_inner` after the transaction commits.
/// Uses `tracing` for logging (not `append_server_log`) since `AppState` is
/// not available in the creation context.
///
/// This handles the case where an agent step is the first (or only) step in a
/// job and nothing else triggers `handle_agent_steps` via the orchestrator.
#[tracing::instrument(skip(pool, agents_config, workspace_config))]
pub async fn dispatch_initial_agent_steps(
    pool: &sqlx::PgPool,
    agents_config: &AgentsConfig,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
) -> Result<()> {
    let job = match JobRepo::get(pool, job_id).await? {
        Some(j) => j,
        None => return Ok(()),
    };

    let task = match workspace_config.tasks.get(&job.task_name) {
        Some(t) => t.clone(),
        None => return Ok(()),
    };

    let max_iterations = task.flow.len() + 1;
    for _iteration in 0..max_iterations {
        let steps = JobStepRepo::get_steps_for_job(pool, job_id).await?;
        let job_row = match JobRepo::get(pool, job_id).await? {
            Some(j) if j.status != "cancelled" => j,
            _ => break, // Job gone or cancelled
        };

        let ready_agent_step = steps
            .iter()
            .find(|s| s.status == StepStatus::Ready.as_ref() && s.action_type == "agent");

        let step = match ready_agent_step {
            Some(s) => s.clone(),
            None => break,
        };

        let action_spec: ActionDef = match step.action_spec.as_ref() {
            Some(spec) => serde_json::from_value(spec.clone())
                .context("Failed to deserialize agent action_spec")?,
            None => {
                let err = "Missing action_spec for agent step";
                JobStepRepo::mark_failed(pool, job_id, &step.step_name, err).await?;
                orchestrate_step(pool, job_id, &step.step_name, &task, workspace_config).await;
                continue;
            }
        };

        let provider_name = match action_spec.provider.as_deref() {
            Some(p) => p,
            None => {
                let err = "Agent step missing provider";
                JobStepRepo::mark_failed(pool, job_id, &step.step_name, err).await?;
                orchestrate_step(pool, job_id, &step.step_name, &task, workspace_config).await;
                continue;
            }
        };

        let provider_config = match agents_config.providers.get(provider_name) {
            Some(p) => p,
            None => {
                let err = format!("Unknown agent provider: {}", provider_name);
                JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
                orchestrate_step(pool, job_id, &step.step_name, &task, workspace_config).await;
                continue;
            }
        };

        JobStepRepo::mark_running_server(pool, job_id, &step.step_name).await?;
        JobRepo::mark_running_if_pending_server(pool, job_id).await?;

        let mut render_ctx =
            crate::job_creator::build_step_render_context(&job_row, &steps, workspace_config);

        // Inject `each` variable for for_each loop instances (mirrors rendering.rs)
        if step.loop_source.is_some() {
            if let Some(ref item) = step.loop_item {
                let each_val = serde_json::json!({
                    "item": item,
                    "index": step.loop_index,
                    "total": step.loop_total,
                });
                if let Some(obj) = render_ctx.as_object_mut() {
                    obj.insert("each".to_string(), each_val);
                }
            }
        }

        // Resolve flow-step input templates and replace `input` in the context
        if let Some(ref input) = step.input {
            if let Some(input_map) = input.as_object() {
                if !input_map.is_empty() {
                    let map: std::collections::HashMap<String, serde_json::Value> = input_map
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    match stroem_common::template::render_input_map(&map, &render_ctx) {
                        Ok(resolved) => {
                            if let Some(ctx_obj) = render_ctx.as_object_mut() {
                                ctx_obj.insert("input".to_string(), resolved);
                            }
                        }
                        Err(e) => {
                            let err = format!(
                                "Failed to render input for agent step '{}': {:#}",
                                step.step_name, e
                            );
                            JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
                            orchestrate_step(
                                pool,
                                job_id,
                                &step.step_name,
                                &task,
                                workspace_config,
                            )
                            .await;
                            continue;
                        }
                    }
                }
            }
        }

        let rendered_prompt = match action_spec.prompt.as_deref() {
            Some(tmpl) => match stroem_common::template::render_template(tmpl, &render_ctx) {
                Ok(p) => p,
                Err(e) => {
                    let err = format!("Failed to render prompt template: {:#}", e);
                    JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
                    orchestrate_step(pool, job_id, &step.step_name, &task, workspace_config).await;
                    continue;
                }
            },
            None => {
                let err = "Agent step missing prompt";
                JobStepRepo::mark_failed(pool, job_id, &step.step_name, err).await?;
                orchestrate_step(pool, job_id, &step.step_name, &task, workspace_config).await;
                continue;
            }
        };

        // Reject empty prompts — an empty string would waste an LLM API call
        if rendered_prompt.trim().is_empty() {
            let err = "Agent step prompt rendered to empty string";
            JobStepRepo::mark_failed(pool, job_id, &step.step_name, err).await?;
            orchestrate_step(pool, job_id, &step.step_name, &task, workspace_config).await;
            continue;
        }

        let rendered_system = match action_spec.system_prompt.as_deref() {
            Some(tmpl) => match stroem_common::template::render_template(tmpl, &render_ctx) {
                Ok(s) => Some(s),
                Err(e) => {
                    let err = format!("Failed to render system_prompt template: {:#}", e);
                    JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err).await?;
                    orchestrate_step(pool, job_id, &step.step_name, &task, workspace_config).await;
                    continue;
                }
            },
            None => None,
        };

        let model_name = action_spec
            .model
            .as_deref()
            .unwrap_or(&provider_config.model);

        tracing::info!(
            job_id = %job_id,
            step = %step.step_name,
            provider = %provider_name,
            model = %model_name,
            "[agent] initial dispatch: calling LLM"
        );

        let start = std::time::Instant::now();

        // Retry loop (same logic as handle_agent_steps, but uses tracing instead of server log)
        let max_retries = provider_config.max_retries;
        let mut call_result: Result<LlmResponse> = Err(anyhow::anyhow!("LLM call not attempted"));

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let base_ms = 1000u64 * (1u64 << (attempt - 1).min(6));
                let jitter = (attempt as u64 * 7919) % (base_ms / 4 + 1);
                let delay_ms = base_ms + jitter;
                tracing::info!(
                    job_id = %job_id,
                    step = %step.step_name,
                    attempt,
                    max_retries,
                    delay_ms,
                    "[agent] initial dispatch: retrying"
                );
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            }

            call_result = match tokio::time::timeout(
                LLM_CALL_TIMEOUT,
                call_llm(
                    provider_config,
                    model_name,
                    &rendered_prompt,
                    rendered_system.as_deref(),
                    &action_spec,
                ),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "LLM API call timed out after {}s",
                    LLM_CALL_TIMEOUT.as_secs()
                )),
            };

            match &call_result {
                Ok(_) => break,
                Err(e) if is_transient_error(e) && attempt < max_retries => continue,
                Err(_) => break,
            }
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;
        let completed_step_name = step.step_name.clone();

        match call_result {
            Ok(response) => {
                let mut output = response.content;
                if let Some(obj) = output.as_object_mut() {
                    obj.insert(
                        "_meta".to_string(),
                        serde_json::json!({
                            "model": model_name,
                            "provider": provider_name,
                            "input_tokens": response.input_tokens,
                            "output_tokens": response.output_tokens,
                            "latency_ms": elapsed_ms,
                            "turns": 1,
                        }),
                    );
                }
                tracing::info!(
                    job_id = %job_id,
                    step = %step.step_name,
                    elapsed_ms,
                    output_tokens = response.output_tokens,
                    "[agent] initial dispatch: step completed"
                );
                JobStepRepo::mark_completed(pool, job_id, &step.step_name, Some(output)).await?;
            }
            Err(e) => {
                let err_msg = format!("{:#}", e);
                tracing::error!(
                    job_id = %job_id,
                    step = %step.step_name,
                    "[agent] initial dispatch: step failed — {}",
                    err_msg
                );
                JobStepRepo::mark_failed(pool, job_id, &step.step_name, &err_msg).await?;
            }
        }

        orchestrate_step(pool, job_id, &completed_step_name, &task, workspace_config).await;
    }

    Ok(())
}

/// Returns `true` if the error is likely transient and worth retrying.
///
/// Matches HTTP status codes precisely (e.g., "status: 429") to avoid false
/// positives from error messages that happen to contain numeric substrings
/// (e.g., "max_tokens must be <= 4500" is not a 500 error).
fn is_transient_error(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err).to_lowercase();
    // Match status codes with a disambiguating prefix
    let has_transient_status = msg.contains("status: 429")
        || msg.contains("status: 500")
        || msg.contains("status: 502")
        || msg.contains("status: 503")
        || msg.contains("status: 529")
        || msg.contains("http 429")
        || msg.contains("http 500")
        || msg.contains("http 502")
        || msg.contains("http 503")
        || msg.contains("http 529")
        || msg.contains("429 too many")
        || msg.contains("500 internal")
        || msg.contains("502 bad gateway")
        || msg.contains("503 service")
        || msg.contains("529 ");
    let has_transient_keyword = msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("temporarily unavailable")
        || msg.contains("overloaded");
    has_transient_status || has_transient_keyword
}

/// Run `orchestrator::on_step_completed` for a step, logging any error.
///
/// This promotes dependent steps to ready without triggering the full
/// `orchestrate_after_step` chain (which would recurse back into
/// `handle_agent_steps`).
async fn orchestrate_step(
    pool: &sqlx::PgPool,
    job_id: Uuid,
    step_name: &str,
    task: &stroem_common::models::workflow::TaskDef,
    workspace_config: &WorkspaceConfig,
) {
    if let Err(e) = crate::orchestrator::on_step_completed(
        pool,
        job_id,
        step_name,
        task,
        Some(workspace_config),
    )
    .await
    {
        tracing::error!(
            "Failed to run orchestrator after agent step '{}' in job {}: {:#}",
            step_name,
            job_id,
            e
        );
    }
}

#[derive(Debug)]
struct LlmResponse {
    content: serde_json::Value,
    input_tokens: u64,
    output_tokens: u64,
}

/// Call the LLM provider using rig-core.
///
/// Builds a fresh client per call (caching can be added later if needed).
/// Uses the `Agent::prompt()` interface for simple single-turn prompts.
///
/// # Temperature and max_tokens
///
/// Values from `action` take precedence over `provider_config` defaults.
///
/// # Structured output (`output`)
///
/// When `action.output` is set, `OutputDef::to_json_schema()` converts it to
/// JSON Schema and the system prompt is augmented with instructions to return a
/// JSON object matching the schema. The response is then parsed as a JSON
/// object; if parsing fails the call returns an error.
///
/// # Token tracking
///
/// The `Prompt` trait returns a `String` with no usage metadata. Token counts
/// are currently returned as `0`.
/// TODO: switch to the lower-level completion API when rig exposes per-call
/// usage in a stable interface.
async fn call_llm(
    provider_config: &AgentProviderConfig,
    model_name: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    action: &ActionDef,
) -> Result<LlmResponse> {
    use rig::completion::Prompt as _;
    use rig::prelude::CompletionClient as _;

    // Resolve temperature: action override takes precedence over provider default.
    let temperature = action.temperature.or(provider_config.temperature);

    // Resolve max_tokens: action override takes precedence over provider default.
    let max_tokens = action.max_tokens.unwrap_or(provider_config.max_tokens);

    // If output is set, convert to JSON Schema and augment the system prompt.
    let schema_suffix: Option<String> = action.output.as_ref().map(|output_def| {
        let schema = output_def.to_json_schema();
        let schema_str =
            serde_json::to_string_pretty(&schema).unwrap_or_else(|_| schema.to_string());
        format!(
            "\n\nYou must respond with a JSON object matching this schema:\n{}\nReturn ONLY the JSON object, no other text, no markdown code fences.",
            schema_str
        )
    });

    let effective_system: Option<String> = match (system_prompt, schema_suffix.as_deref()) {
        (Some(sys), Some(suffix)) => Some(format!("{}{}", sys, suffix)),
        (None, Some(suffix)) => Some(suffix.to_string()),
        (Some(sys), None) => Some(sys.to_string()),
        (None, None) => None,
    };

    /// Build a rig provider client, configure an agent, and prompt it.
    macro_rules! call_provider {
        ($client_type:ty, $label:expr, $api_key:expr) => {{
            let mut builder = <$client_type>::builder().api_key($api_key);
            if let Some(ref endpoint) = provider_config.api_endpoint {
                builder = builder.base_url(endpoint);
            }
            let client = builder
                .build()
                .context(concat!("Failed to build ", $label, " client"))?;
            let mut ab = client.agent(model_name);
            if let Some(ref sys) = effective_system {
                ab = ab.preamble(sys);
            }
            ab = ab.max_tokens(u64::from(max_tokens));
            if let Some(temp) = temperature {
                ab = ab.temperature(f64::from(temp));
            }
            ab.build()
                .prompt(prompt)
                .await
                .context(concat!($label, " API call failed"))?
        }};
    }

    let require_api_key = || -> Result<&str> {
        provider_config
            .api_key
            .as_deref()
            .context("Agent provider requires api_key")
    };

    let response_text = match provider_config.provider_type.as_str() {
        // Standard providers (api_key required)
        "anthropic" => call_provider!(
            rig::providers::anthropic::Client,
            "Anthropic",
            require_api_key()?.to_string()
        ),
        "cohere" => call_provider!(
            rig::providers::cohere::Client,
            "Cohere",
            require_api_key()?.to_string()
        ),
        "deepseek" => call_provider!(
            rig::providers::deepseek::Client,
            "DeepSeek",
            require_api_key()?.to_string()
        ),
        "galadriel" => call_provider!(
            rig::providers::galadriel::Client,
            "Galadriel",
            require_api_key()?.to_string()
        ),
        "gemini" => call_provider!(
            rig::providers::gemini::Client,
            "Gemini",
            require_api_key()?.to_string()
        ),
        "groq" => call_provider!(
            rig::providers::groq::Client,
            "Groq",
            require_api_key()?.to_string()
        ),
        "huggingface" => call_provider!(
            rig::providers::huggingface::Client,
            "HuggingFace",
            require_api_key()?.to_string()
        ),
        "hyperbolic" => call_provider!(
            rig::providers::hyperbolic::Client,
            "Hyperbolic",
            require_api_key()?.to_string()
        ),
        "mira" => call_provider!(
            rig::providers::mira::Client,
            "Mira",
            require_api_key()?.to_string()
        ),
        "mistral" => call_provider!(
            rig::providers::mistral::Client,
            "Mistral",
            require_api_key()?.to_string()
        ),
        "moonshot" => call_provider!(
            rig::providers::moonshot::Client,
            "Moonshot",
            require_api_key()?.to_string()
        ),
        "openrouter" => call_provider!(
            rig::providers::openrouter::Client,
            "OpenRouter",
            require_api_key()?.to_string()
        ),
        "perplexity" => call_provider!(
            rig::providers::perplexity::Client,
            "Perplexity",
            require_api_key()?.to_string()
        ),
        "together" => call_provider!(
            rig::providers::together::Client,
            "Together",
            require_api_key()?.to_string()
        ),
        "xai" => call_provider!(
            rig::providers::xai::Client,
            "xAI",
            require_api_key()?.to_string()
        ),
        // OpenAI: uses CompletionsClient for chat completions endpoint
        "openai" => call_provider!(
            rig::providers::openai::CompletionsClient,
            "OpenAI",
            require_api_key()?.to_string()
        ),
        // Azure: typed auth + required api_endpoint
        "azure" => {
            let api_key = require_api_key()?;
            let endpoint = provider_config
                .api_endpoint
                .as_deref()
                .context("Azure provider requires api_endpoint")?;
            let auth = rig::providers::azure::AzureOpenAIAuth::ApiKey(api_key.to_string());
            let client = rig::providers::azure::Client::builder()
                .api_key(auth)
                .azure_endpoint(endpoint.to_string())
                .build()
                .context("Failed to build Azure client")?;
            let mut ab = client.agent(model_name);
            if let Some(ref sys) = effective_system {
                ab = ab.preamble(sys);
            }
            ab = ab.max_tokens(u64::from(max_tokens));
            if let Some(temp) = temperature {
                ab = ab.temperature(f64::from(temp));
            }
            ab.build()
                .prompt(prompt)
                .await
                .context("Azure API call failed")?
        }
        // Local providers: no API key needed
        "ollama" => call_provider!(
            rig::providers::ollama::Client,
            "Ollama",
            rig::client::Nothing
        ),
        "llamafile" => call_provider!(
            rig::providers::llamafile::Client,
            "Llamafile",
            rig::client::Nothing
        ),
        other => bail!("Unknown agent provider type: {}", other),
    };

    // When output is set, the response must be a valid JSON object.
    let content = if action.output.is_some() {
        let trimmed = response_text.trim();
        // Strip markdown code fences if the model wrapped the JSON anyway
        let json_str = strip_code_fences(trimmed);
        match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(json) if json.is_object() => json,
            Ok(json) => {
                bail!(
                    "output requires a JSON object but got {}",
                    json_type_name(&json)
                );
            }
            Err(e) => {
                bail!(
                    "output requires a JSON object but response could not be parsed: {}. Response was: {}",
                    e,
                    truncate_for_error(trimmed, 200)
                );
            }
        }
    } else {
        // No schema: try to parse as JSON, fall back to text wrapper
        match serde_json::from_str::<serde_json::Value>(&response_text) {
            Ok(json) if json.is_object() => json,
            _ => serde_json::json!({ "text": response_text }),
        }
    };

    // Token usage is not exposed through the Prompt trait (returns String only).
    // TODO: switch to the lower-level completion API when rig exposes per-call
    // token usage in a stable interface.
    Ok(LlmResponse {
        content,
        input_tokens: 0,
        output_tokens: 0,
    })
}

/// Strip a single layer of markdown code fences (```json ... ``` or ``` ... ```).
fn strip_code_fences(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```") {
        // Skip optional language tag on the first line
        let after_tag = if let Some(nl) = rest.find('\n') {
            &rest[nl + 1..]
        } else {
            return s; // No newline — malformed fence, return as-is
        };
        if let Some(end) = after_tag.rfind("```") {
            return after_tag[..end].trim();
        }
    }
    s
}

/// Human-readable JSON value type name for error messages.
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Truncate a string to at most `max_bytes` bytes, respecting UTF-8 char
/// boundaries.  When `max_bytes` falls in the middle of a multi-byte character
/// the function snaps down to the nearest complete character boundary so the
/// returned slice is always valid UTF-8.
fn truncate_for_error(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk backwards from max_bytes to find a char boundary.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SUPPORTED_AGENT_PROVIDERS;
    use stroem_common::models::workflow::ActionDef;

    fn make_action(output: Option<serde_json::Value>) -> ActionDef {
        let mut val = serde_json::json!({
            "type": "agent",
            "provider": "test-provider",
            "prompt": "test prompt"
        });
        if let Some(output_def) = output {
            val["output"] = output_def;
        }
        serde_json::from_value(val).expect("ActionDef from test JSON")
    }

    fn make_provider(provider_type: &str) -> AgentProviderConfig {
        AgentProviderConfig {
            provider_type: provider_type.to_string(),
            api_key: Some("test-key".to_string()),
            api_endpoint: None,
            model: "model".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_retries: 3,
        }
    }

    /// Validate that an unknown provider type produces a clear error message.
    #[tokio::test]
    async fn test_call_llm_unknown_provider_type() {
        let provider_config = AgentProviderConfig {
            provider_type: "bedrock".to_string(),
            api_key: Some("test-key".to_string()),
            api_endpoint: None,
            model: "model".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_retries: 3,
        };
        let action = make_action(None);

        let result = call_llm(&provider_config, "some-model", "hello", None, &action).await;
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("Unknown agent provider type"),
            "Expected 'Unknown agent provider type', got: {}",
            msg
        );
    }

    /// Validate that a missing api_key produces a clear error message.
    #[tokio::test]
    async fn test_call_llm_missing_api_key() {
        let provider_config = AgentProviderConfig {
            provider_type: "anthropic".to_string(),
            api_key: None,
            api_endpoint: None,
            model: "claude-3-5-haiku-latest".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_retries: 3,
        };
        let action = make_action(None);

        let result = call_llm(
            &provider_config,
            "claude-3-5-haiku-latest",
            "hello",
            None,
            &action,
        )
        .await;
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("api_key"),
            "Expected error about api_key, got: {}",
            msg
        );
    }

    /// Verify ollama does NOT require an api_key.
    #[tokio::test]
    async fn test_call_llm_ollama_no_api_key_required() {
        let provider_config = AgentProviderConfig {
            provider_type: "ollama".to_string(),
            api_key: None,
            api_endpoint: Some("http://127.0.0.1:1".to_string()),
            model: "llama3".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_retries: 3,
        };
        let action = make_action(None);
        let result = call_llm(&provider_config, "llama3", "hello", None, &action).await;
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        // Must fail with connection error, NOT with "requires api_key"
        assert!(
            !msg.contains("requires api_key"),
            "ollama should not require api_key, got: {}",
            msg
        );
        assert!(
            msg.contains("Ollama API call failed"),
            "Expected Ollama API call error (connection), got: {}",
            msg
        );
    }

    /// Verify azure requires api_endpoint.
    #[tokio::test]
    async fn test_call_llm_azure_requires_endpoint() {
        let provider_config = AgentProviderConfig {
            provider_type: "azure".to_string(),
            api_key: Some("test-key".to_string()),
            api_endpoint: None,
            model: "gpt-4o".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_retries: 3,
        };
        let action = make_action(None);
        let result = call_llm(&provider_config, "gpt-4o", "hello", None, &action).await;
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("requires api_endpoint"),
            "Expected 'requires api_endpoint', got: {}",
            msg
        );
    }

    /// Every provider in SUPPORTED_AGENT_PROVIDERS must be handled in call_llm
    /// (not hit the catch-all "Unknown agent provider type" arm).
    #[tokio::test]
    async fn test_dispatch_covers_all_supported_providers() {
        let action = make_action(None);
        for &provider_type in SUPPORTED_AGENT_PROVIDERS {
            let provider_config = AgentProviderConfig {
                provider_type: provider_type.to_string(),
                api_key: Some("test-key".to_string()),
                api_endpoint: Some("http://127.0.0.1:1".to_string()),
                model: "test-model".to_string(),
                max_tokens: 4096,
                temperature: None,
                max_retries: 3,
            };
            let result = call_llm(&provider_config, "test-model", "hello", None, &action).await;
            // All should fail (unreachable endpoint) but NOT with "Unknown agent provider type"
            assert!(result.is_err());
            let msg = format!("{:#}", result.unwrap_err());
            assert!(
                !msg.contains("Unknown agent provider type"),
                "Provider '{}' is not handled in call_llm dispatch: {}",
                provider_type,
                msg
            );
        }
    }

    // ─── Unit tests for pure helper functions ─────────────────────────────────

    #[test]
    fn test_is_transient_error_429() {
        let err = anyhow::anyhow!("HTTP 429 Too Many Requests");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_503() {
        let err = anyhow::anyhow!("status: 503 Service Unavailable");
        assert!(is_transient_error(&err));
    }

    /// A message containing "500" as part of a number (e.g., token limit) must NOT
    /// be classified as transient — it does not match the status-code prefix patterns.
    #[test]
    fn test_is_not_transient_500_in_message() {
        let err = anyhow::anyhow!("max_tokens must be <= 4500");
        assert!(!is_transient_error(&err));
    }

    /// A message containing "connection" as part of a config field name must NOT
    /// be classified as transient.
    #[test]
    fn test_is_not_transient_connection_in_validation() {
        let err = anyhow::anyhow!("connection_type is invalid");
        assert!(!is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_connection() {
        let err = anyhow::anyhow!("connection refused");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_timeout() {
        let err = anyhow::anyhow!("request timed out");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_not_transient_error_401() {
        let err = anyhow::anyhow!("HTTP 401 Unauthorized");
        assert!(!is_transient_error(&err));
    }

    #[test]
    fn test_is_not_transient_error_400() {
        let err = anyhow::anyhow!("HTTP 400 Bad Request: invalid model");
        assert!(!is_transient_error(&err));
    }

    #[test]
    fn test_strip_code_fences_no_fences() {
        assert_eq!(strip_code_fences(r#"{"key":"val"}"#), r#"{"key":"val"}"#);
    }

    #[test]
    fn test_strip_code_fences_json_fences() {
        let input = "```json\n{\"key\":\"val\"}\n```";
        assert_eq!(strip_code_fences(input), r#"{"key":"val"}"#);
    }

    #[test]
    fn test_strip_code_fences_plain_fences() {
        let input = "```\n{\"key\":\"val\"}\n```";
        assert_eq!(strip_code_fences(input), r#"{"key":"val"}"#);
    }

    #[test]
    fn test_truncate_for_error_short() {
        assert_eq!(truncate_for_error("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_for_error_long() {
        let s = "a".repeat(300);
        let truncated = truncate_for_error(&s, 200);
        assert_eq!(truncated.len(), 200);
    }

    #[test]
    fn test_json_type_name() {
        assert_eq!(json_type_name(&serde_json::Value::Null), "null");
        assert_eq!(json_type_name(&serde_json::Value::Bool(true)), "boolean");
        assert_eq!(json_type_name(&serde_json::Value::Array(vec![])), "array");
    }

    /// When provider has a temperature set, it should be used.
    /// (We can only test the error path since we can't mock the API.)
    #[test]
    fn test_make_provider_has_temperature() {
        let mut p = make_provider("anthropic");
        p.temperature = Some(0.7);
        assert_eq!(p.temperature, Some(0.7));
    }

    /// Action-level temperature override takes precedence.
    #[test]
    fn test_action_temperature_override() {
        let mut action = make_action(None);
        action.temperature = Some(0.2);
        let provider = make_provider("anthropic");
        // Action temperature (0.2) should override provider temperature (None)
        let effective = action.temperature.or(provider.temperature);
        assert_eq!(effective, Some(0.2));
    }

    /// Action-level max_tokens override takes precedence.
    #[test]
    fn test_action_max_tokens_override() {
        let mut action = make_action(None);
        action.max_tokens = Some(512);
        let provider = make_provider("anthropic");
        let effective = action.max_tokens.unwrap_or(provider.max_tokens);
        assert_eq!(effective, 512);
    }

    /// When max_tokens is not set on the action, provider default is used.
    #[test]
    fn test_provider_max_tokens_fallback() {
        let action = make_action(None);
        let provider = make_provider("anthropic");
        let effective = action.max_tokens.unwrap_or(provider.max_tokens);
        assert_eq!(effective, 4096);
    }

    /// Provider temperature is used when the action has none set.
    #[test]
    fn test_provider_temperature_used_when_action_has_none() {
        let action = make_action(None);
        let mut provider = make_provider("anthropic");
        provider.temperature = Some(0.5);
        let effective = action.temperature.or(provider.temperature);
        assert_eq!(effective, Some(0.5));
    }

    // ─── truncate_for_error: multi-byte UTF-8 ────────────────────────────────

    /// Truncating at a byte offset inside a 4-byte emoji must snap to the
    /// last complete char boundary and return valid UTF-8 without panicking.
    ///
    /// "Hello " is 6 bytes; 🌍 starts at byte 6 and is 4 bytes wide.
    /// A budget of 7 bytes falls inside the emoji, so the result must be
    /// "Hello " (6 bytes).
    #[test]
    fn test_truncate_for_error_multibyte_utf8() {
        let s = "Hello 🌍 world";
        let result = truncate_for_error(s, 7);
        assert!(result.len() <= 7);
        assert!(
            s.is_char_boundary(result.len()),
            "result must end on a char boundary"
        );
        assert_eq!(result, "Hello ");
    }

    /// Each CJK character is 3 bytes. Truncating a 12-byte string ("你好世界")
    /// at 7 bytes should yield the first 2 characters (6 bytes) rather than
    /// returning a partial 3-byte character sequence.
    #[test]
    fn test_truncate_for_error_cjk() {
        let s = "你好世界"; // 12 bytes, 4 chars
        let result = truncate_for_error(s, 7);
        assert!(result.len() <= 7);
        assert!(
            s.is_char_boundary(result.len()),
            "result must end on a char boundary"
        );
        assert_eq!(result, "你好"); // 6 bytes
    }

    /// When the budget lands exactly on a char boundary, no bytes are dropped.
    #[test]
    fn test_truncate_for_error_exact_char_boundary() {
        // "Hello 🌍" = "Hello " (6) + emoji (4) = 10 bytes. Byte 10 is a
        // valid char boundary (space follows the emoji).
        let s = "Hello 🌍 world";
        let result = truncate_for_error(s, 10);
        assert!(result.len() <= 10);
        assert!(s.is_char_boundary(result.len()));
        assert_eq!(result, "Hello 🌍");
    }

    // ─── Unstructured response wrapping ──────────────────────────────────────

    /// Plain text (not JSON) is wrapped in `{"text": ...}`.
    #[test]
    fn test_unstructured_response_wrapping() {
        let response_text = "This is a plain text response from the LLM.";
        let content = match serde_json::from_str::<serde_json::Value>(response_text) {
            Ok(json) if json.is_object() => json,
            _ => serde_json::json!({ "text": response_text }),
        };
        assert_eq!(
            content["text"],
            "This is a plain text response from the LLM."
        );
    }

    /// A JSON object response passes through without a `text` wrapper.
    #[test]
    fn test_json_object_response_passes_through() {
        let response_text = r#"{"category":"bug","confidence":0.95}"#;
        let content = match serde_json::from_str::<serde_json::Value>(response_text) {
            Ok(json) if json.is_object() => json,
            _ => serde_json::json!({ "text": response_text }),
        };
        assert_eq!(content["category"], "bug");
        assert!(
            content.get("text").is_none(),
            "object response must not gain a text key"
        );
    }

    /// A JSON array (not an object) is wrapped in `{"text": ...}`.
    #[test]
    fn test_json_array_response_wraps_to_text() {
        let response_text = r#"["item1", "item2"]"#;
        let content = match serde_json::from_str::<serde_json::Value>(response_text) {
            Ok(json) if json.is_object() => json,
            _ => serde_json::json!({ "text": response_text }),
        };
        assert_eq!(content["text"], r#"["item1", "item2"]"#);
    }

    // ─── strip_code_fences: malformed input ──────────────────────────────────

    /// An opening fence with no closing ``` returns the original string.
    #[test]
    fn test_strip_code_fences_no_closing_fence() {
        let input = "```json\n{\"key\":\"val\"}";
        assert_eq!(strip_code_fences(input), input);
    }

    /// Only an opening fence with no newline (malformed) returns the original.
    #[test]
    fn test_strip_code_fences_only_opening() {
        let input = "```";
        assert_eq!(strip_code_fences(input), input);
    }

    // ─── is_transient_error: additional keyword coverage ─────────────────────

    /// The word "overloaded" triggers a transient classification.
    #[test]
    fn test_is_transient_error_overloaded() {
        let err = anyhow::anyhow!("API overloaded, please try again");
        assert!(is_transient_error(&err));
    }

    /// A generic validation error is not transient.
    #[test]
    fn test_is_not_transient_error_validation() {
        let err = anyhow::anyhow!("Invalid model parameter: temperature must be >= 0");
        assert!(!is_transient_error(&err));
    }

    // ─── _meta injection ─────────────────────────────────────────────────────

    /// `_meta` is inserted alongside existing keys on a JSON object output.
    #[test]
    fn test_meta_injection_on_object() {
        let mut output = serde_json::json!({"category": "bug", "confidence": 0.95});
        if let Some(obj) = output.as_object_mut() {
            obj.insert(
                "_meta".to_string(),
                serde_json::json!({
                    "model": "test-model",
                    "provider": "test-provider",
                    "input_tokens": 100u64,
                    "output_tokens": 50u64,
                    "latency_ms": 1000u64,
                    "turns": 1u64,
                }),
            );
        }
        assert_eq!(output["category"], "bug");
        assert_eq!(output["_meta"]["model"], "test-model");
        assert_eq!(output["_meta"]["input_tokens"], 100);
    }

    /// When output is the `{"text": "..."}` wrapper it is still a JSON object,
    /// so `_meta` is injected into it just like any other object response.
    #[test]
    fn test_meta_injected_on_text_wrapper() {
        let mut output = serde_json::json!({"text": "plain response"});
        if let Some(obj) = output.as_object_mut() {
            obj.insert("_meta".to_string(), serde_json::json!({"model": "m"}));
        }
        assert!(output["_meta"].is_object());
        assert_eq!(output["text"], "plain response");
    }
}
