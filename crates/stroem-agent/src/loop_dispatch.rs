//! Custom multi-turn agent dispatch loop.
//!
//! Uses rig-core's `CompletionModel::completion()` for raw LLM calls while
//! handling tool routing, state persistence, and suspension ourselves.
//!
//! This module is environment-agnostic: it takes an `AgentContext` trait
//! for operations that differ between server-side and worker-side execution
//! (e.g., creating child jobs, saving state, checking cancellation).

use anyhow::{bail, Context, Result};
use rig_core::completion::{AssistantContent, Message, ToolDefinition, Usage};
use rig_core::message::{CallId, ToolCall, ToolName, ToolResult, ToolResultContent, UserContent};
use stroem_common::models::workflow::{ActionDef, AgentToolRef};
use uuid::Uuid;

use crate::config::AgentProviderConfig;
#[cfg(feature = "mcp")]
use crate::mcp_client::McpClientManager;
use crate::state::{AgentConversationState, AskUserCall, PendingToolCall};
use crate::tools;

/// Trait for environment-specific operations needed by the dispatch loop.
///
/// Server implements this via direct DB calls.
/// Worker implements this via HTTP calls to server endpoints.
// async_trait desugars this to a function returning a `Pin<Box<dyn Future>>`,
// already `#[must_use]`, and redundantly marks the method itself `#[must_use]`
// too — newer clippy's `double_must_use` flags that redundancy.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait AgentContext: Send + Sync {
    /// Check if the job has been cancelled.
    async fn is_job_cancelled(&self, job_id: Uuid) -> bool;

    /// Create a child job for a task tool call. Returns child_job_id.
    async fn create_task_tool_job(
        &self,
        job_id: Uuid,
        step_name: &str,
        task_name: &str,
        input: serde_json::Value,
    ) -> Result<Uuid>;

    /// Save intermediate agent state (pending tool calls).
    async fn save_agent_state(
        &self,
        job_id: Uuid,
        step_name: &str,
        state: &AgentConversationState,
    ) -> Result<()>;

    /// Suspend step for ask_user (save state + mark suspended).
    async fn suspend_for_ask_user(
        &self,
        job_id: Uuid,
        step_name: &str,
        state: &AgentConversationState,
        message: &str,
    ) -> Result<()>;

    /// Append a log message to the job's server event stream.
    async fn log(&self, job_id: Uuid, message: &str);
}

/// Outcome of a dispatch loop iteration.
pub enum DispatchOutcome {
    /// Agent produced a final text response — step is complete.
    Completed {
        output: serde_json::Value,
        usage: Usage,
        turns: u32,
    },
    /// Agent called task tools that created child jobs — waiting for them.
    WaitingForTools { state: AgentConversationState },
    /// Agent called ask_user — step is suspended waiting for human input.
    WaitingForUser {
        state: AgentConversationState,
        message: String,
    },
    /// Agent failed with an error.
    Failed { error: String },
}

/// Default max turns if not specified on the action.
const DEFAULT_MAX_TURNS: u32 = 25;

/// Task tool info for building tool definitions.
pub struct TaskToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input: std::collections::HashMap<String, stroem_common::models::workflow::InputFieldDef>,
    /// Pre-built JSON Schema for tool parameters (skips rebuilding from `input`).
    /// When set, `build_tool_definitions` uses this directly instead of calling
    /// `input_schema_to_json_schema` on the `input` map.
    pub parameters_schema: Option<serde_json::Value>,
}

/// Run the multi-turn agent dispatch loop.
///
/// If `resume_state` is provided, the loop resumes from a previous suspension.
/// Otherwise, it starts a fresh conversation.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_agent_loop(
    ctx: &dyn AgentContext,
    job_id: Uuid,
    step_name: &str,
    action_spec: &ActionDef,
    provider_config: &AgentProviderConfig,
    model_name: &str,
    rendered_prompt: &str,
    rendered_system: Option<&str>,
    resume_state: Option<AgentConversationState>,
    #[cfg(feature = "mcp")] mcp_client: Option<&McpClientManager>,
    #[cfg(not(feature = "mcp"))] _mcp_client: Option<&()>,
    tool_results: Vec<(String, String)>,
    task_tool_infos: &[TaskToolInfo],
) -> Result<DispatchOutcome> {
    let max_turns = action_spec.max_turns.unwrap_or(DEFAULT_MAX_TURNS).min(100);

    // Build tool definitions
    let tool_defs = build_tool_definitions(
        action_spec,
        task_tool_infos,
        #[cfg(feature = "mcp")]
        mcp_client,
    );

    // Initialize or restore conversation state
    let mut conv_state = resume_state.unwrap_or_default();

    // History saved by a rig-core 0.36 worker predates the current message
    // schema; lift it so a step suspended across the upgrade resumes intact.
    let reasoning_issuer = crate::provider::reasoning_issuer(provider_config, model_name)
        .unwrap_or_else(|| provider_config.provider_type.clone());
    crate::legacy_history::upgrade(&mut conv_state.messages, &reasoning_issuer);

    // Collect tool results: from explicit parameter or from resolved state populated by
    // the server when agent_tool child jobs complete.
    //
    // We borrow from `conv_state.resolved_tool_results` without draining first so
    // that the data is never lost if a subsequent fallible operation returns `Err`.
    // The vec is cleared only after the results have been successfully serialised
    // into `conv_state.messages`.
    let from_state = tool_results.is_empty() && !conv_state.resolved_tool_results.is_empty();
    let effective_tool_results: Vec<(String, String)> = if !tool_results.is_empty() {
        tool_results
    } else {
        conv_state
            .resolved_tool_results
            .iter()
            .map(|r| (r.tool_call_id.clone(), r.result_text.clone()))
            .collect()
    };

    // If resuming with tool results, inject them as user messages
    if !effective_tool_results.is_empty() {
        let history = parse_history(&conv_state.messages);
        let tool_result_contents: Vec<UserContent> = effective_tool_results
            .iter()
            .map(|(call_id, result_text)| {
                UserContent::ToolResult(resumed_tool_result(&history, call_id, result_text))
            })
            .collect();

        let tool_msg = Message::User {
            content: tool_result_contents,
        };
        // Serialise first (fallible); only clear the source vec on success.
        conv_state.messages.push(serde_json::to_value(&tool_msg)?);
        if from_state {
            conv_state.resolved_tool_results.clear();
        }
    }

    // Build effective system prompt
    let effective_system = crate::dispatch::build_effective_system(rendered_system, action_spec);

    // Resolve temperature and max_tokens
    let temperature = action_spec.temperature.or(provider_config.temperature);
    let max_tokens = action_spec.max_tokens.unwrap_or(provider_config.max_tokens);

    // Main dispatch loop
    loop {
        conv_state.turn += 1;

        // Check if job has been cancelled
        if ctx.is_job_cancelled(job_id).await {
            return Ok(DispatchOutcome::Failed {
                error: "Job was cancelled".to_string(),
            });
        }

        if conv_state.turn > max_turns {
            return Ok(DispatchOutcome::Failed {
                error: format!(
                    "Agent exceeded maximum turns ({}) without producing a final answer",
                    max_turns
                ),
            });
        }

        // Build the completion request
        let mut chat_history = parse_history(&conv_state.messages);

        let prompt_msg = if conv_state.turn == 1 {
            Message::user(rendered_prompt)
        } else {
            chat_history
                .pop()
                .unwrap_or_else(|| Message::user("Continue."))
        };

        let request = crate::dispatch::completion_request(
            effective_system.as_deref(),
            chat_history,
            prompt_msg,
            tool_defs.clone(),
            temperature,
            max_tokens,
        );

        // Make the LLM call with timeout
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            crate::provider::call_completion(provider_config, model_name, request),
        )
        .await
        {
            Ok(r) => r.context("LLM completion call failed")?,
            Err(_) => {
                bail!("LLM API call timed out after 120s");
            }
        };

        // Track token usage (a counter the provider did not report adds 0)
        let turn_input_tokens = response.usage.input_tokens.unwrap_or(0);
        let turn_output_tokens = response.usage.output_tokens.unwrap_or(0);
        conv_state.total_input_tokens += turn_input_tokens;
        conv_state.total_output_tokens += turn_output_tokens;

        // Save the prompt to conversation history (only on first turn)
        if conv_state.turn == 1 {
            let user_msg = Message::user(rendered_prompt);
            conv_state.messages.push(serde_json::to_value(&user_msg)?);
        }

        // Save assistant response to conversation history
        let assistant_msg = Message::Assistant {
            id: response.message_id.clone(),
            content: response.choice.clone(),
        };
        conv_state
            .messages
            .push(serde_json::to_value(&assistant_msg)?);

        ctx.log(
            job_id,
            &format!(
                "[agent] Turn {}: LLM responded (in={}, out={} tokens)",
                conv_state.turn, turn_input_tokens, turn_output_tokens
            ),
        )
        .await;

        // Classify the response content
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for content in response.choice.iter() {
            match content {
                AssistantContent::Text(t) => {
                    text_parts.push(t.text.clone());
                    ctx.log(job_id, &format!("[agent] {}", t.text)).await;
                }
                AssistantContent::ToolCall(tc) => {
                    let args_str = tc.function.arguments.to_string();
                    let args_preview = crate::dispatch::truncate_for_error(&args_str, 200);
                    ctx.log(
                        job_id,
                        &format!("[agent] Tool call: {}({})", tc.function.name, args_preview),
                    )
                    .await;
                    tool_calls.push(tc.clone());
                }
                AssistantContent::Reasoning(sealed) => {
                    // Surface thinking/reasoning blocks in the log stream
                    let thinking_text: String = sealed
                        .open(sealed.issuer())
                        .into_iter()
                        .flat_map(|r| r.content.iter())
                        .filter_map(|c| match c {
                            rig_core::message::ReasoningContent::Text { text, .. } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !thinking_text.is_empty() {
                        ctx.log(job_id, &format!("[agent] Thinking: {}", thinking_text))
                            .await;
                    }
                }
                _ => {}
            }
        }

        // No tool calls → final response
        if tool_calls.is_empty() {
            let response_text = text_parts.join("\n");
            let output = crate::dispatch::build_final_output(action_spec, &response_text)?;
            return Ok(DispatchOutcome::Completed {
                output,
                usage: Usage {
                    input_tokens: Some(conv_state.total_input_tokens),
                    output_tokens: Some(conv_state.total_output_tokens),
                    total_tokens: Some(
                        conv_state.total_input_tokens + conv_state.total_output_tokens,
                    ),
                    cached_input_tokens: Some(0),
                    cache_creation_input_tokens: Some(0),
                    ..Usage::default()
                },
                turns: conv_state.turn,
            });
        }

        // Partition tool calls by type
        let mut mcp_calls = Vec::new();
        let mut task_calls = Vec::new();
        let mut ask_user_call: Option<&ToolCall> = None;

        for tc in &tool_calls {
            if tc.function.name == "ask_user" {
                ask_user_call = Some(tc);
            } else if is_mcp_tool_call(
                &tc.function.name,
                #[cfg(feature = "mcp")]
                mcp_client,
            ) {
                mcp_calls.push(tc);
            } else if tc.function.name.starts_with("strom_task_") {
                task_calls.push(tc);
            } else {
                // Unknown tool — return error as tool result
                let err_result = UserContent::ToolResult(tool_result_text(
                    tc,
                    format!("Error: unknown tool '{}'", tc.function.name),
                ));
                let msg = Message::User {
                    content: vec![err_result],
                };
                conv_state.messages.push(serde_json::to_value(&msg)?);
            }
        }

        // Execute MCP tool calls synchronously
        #[cfg(feature = "mcp")]
        for tc in &mcp_calls {
            let result = if let Some(client) = mcp_client {
                match client
                    .call_tool(&tc.function.name, tc.function.arguments.clone())
                    .await
                {
                    Ok(text) => text,
                    Err(e) => format!("Error calling MCP tool '{}': {:#}", tc.function.name, e),
                }
            } else {
                format!(
                    "Error: MCP tool '{}' called but no MCP client available",
                    tc.function.name
                )
            };

            let result_preview = crate::dispatch::truncate_for_error(&result, 500);
            ctx.log(
                job_id,
                &format!(
                    "[agent] MCP result {}: {}",
                    tc.function.name, result_preview
                ),
            )
            .await;

            let tool_result = UserContent::ToolResult(tool_result_text(tc, result));
            let msg = Message::User {
                content: vec![tool_result],
            };
            conv_state.messages.push(serde_json::to_value(&msg)?);
        }
        #[cfg(not(feature = "mcp"))]
        let _ = &mcp_calls; // suppress unused warning

        // Handle ask_user (takes priority over task calls)
        if let Some(tc) = ask_user_call {
            if !action_spec.interactive {
                let err_result = UserContent::ToolResult(tool_result_text(
                    tc,
                    "Error: ask_user is not available. The 'interactive' flag is not enabled on this action.".to_string(),
                ));
                let msg = Message::User {
                    content: vec![err_result],
                };
                conv_state.messages.push(serde_json::to_value(&msg)?);
                continue;
            }

            let message = tc
                .function
                .arguments
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Please provide input")
                .to_string();

            conv_state.suspended_for_ask_user = true;
            conv_state.ask_user_call = Some(AskUserCall {
                tool_call_id: tool_call_handle(&tc.id),
                message: message.clone(),
            });

            // Save state and suspend via context
            ctx.suspend_for_ask_user(job_id, step_name, &conv_state, &message)
                .await?;

            return Ok(DispatchOutcome::WaitingForUser {
                state: conv_state,
                message,
            });
        }

        // Handle task tool calls — create child jobs
        if !task_calls.is_empty() {
            for tc in &task_calls {
                let task_name = tools::task_name_from_tool_name(&tc.function.name)
                    .unwrap_or_else(|| tc.function.name.to_string());

                // Convert underscores back to hyphens for task lookup
                let task_name_hyphen = task_name.replace('_', "-");

                // Check if the task exists in our tool set
                let resolved_task_name = if task_tool_infos.iter().any(|t| t.name == task_name) {
                    task_name.clone()
                } else if task_tool_infos.iter().any(|t| t.name == task_name_hyphen) {
                    task_name_hyphen.clone()
                } else {
                    let err_result = UserContent::ToolResult(tool_result_text(
                        tc,
                        format!("Error: task '{}' not found", task_name),
                    ));
                    let msg = Message::User {
                        content: vec![err_result],
                    };
                    conv_state.messages.push(serde_json::to_value(&msg)?);
                    continue;
                };

                // Verify the task is in the allowed tool set
                let allowed = action_spec.tools.iter().any(|t| match t {
                    AgentToolRef::Task { git_ref: _, task } => {
                        task == &resolved_task_name || task.replace('-', "_") == task_name
                    }
                    _ => false,
                });
                if !allowed {
                    let err_result = UserContent::ToolResult(tool_result_text(
                        tc,
                        format!(
                            "Error: task '{}' is not in the allowed tool set",
                            resolved_task_name
                        ),
                    ));
                    let msg = Message::User {
                        content: vec![err_result],
                    };
                    conv_state.messages.push(serde_json::to_value(&msg)?);
                    continue;
                }

                let input = if tc.function.arguments.is_object() {
                    tc.function.arguments.clone()
                } else {
                    serde_json::json!({})
                };

                let child_job_id = ctx
                    .create_task_tool_job(job_id, step_name, &resolved_task_name, input)
                    .await
                    .context(format!(
                        "Failed to create child job for task tool '{}'",
                        resolved_task_name
                    ))?;

                conv_state.pending_tool_calls.push(PendingToolCall {
                    tool_call_id: tool_call_handle(&tc.id),
                    tool_name: tc.function.name.to_string(),
                    child_job_id,
                });
            }

            // Save state via context
            ctx.save_agent_state(job_id, step_name, &conv_state).await?;

            return Ok(DispatchOutcome::WaitingForTools { state: conv_state });
        }

        // If we only had MCP calls (all resolved synchronously), loop continues
    }
}

/// Build tool definitions from action spec and task tool infos.
fn build_tool_definitions(
    action_spec: &ActionDef,
    task_tool_infos: &[TaskToolInfo],
    #[cfg(feature = "mcp")] mcp_client: Option<&McpClientManager>,
) -> Vec<ToolDefinition> {
    let mut defs = Vec::new();

    for tool_ref in &action_spec.tools {
        match tool_ref {
            AgentToolRef::Task { git_ref: _, task } => {
                if let Some(info) = task_tool_infos.iter().find(|t| &t.name == task) {
                    if let Some(ref schema) = info.parameters_schema {
                        // Use pre-built schema from server — avoids re-running
                        // `input_schema_to_json_schema` on an empty input map.
                        defs.push(ToolDefinition {
                            name: format!("strom_task_{}", task.replace('-', "_")),
                            description: info
                                .description
                                .clone()
                                .unwrap_or_else(|| format!("Execute the '{}' task", task)),
                            parameters: schema.clone(),
                        });
                    } else {
                        // Fallback: build from the input map (original path).
                        let task_def = stroem_common::models::workflow::TaskDef {
                            description: info.description.clone(),
                            input: info.input.clone(),
                            ..serde_yaml::from_str("flow:\n  _:\n    action: _")
                                .expect("valid minimal TaskDef YAML")
                        };
                        defs.push(tools::task_to_tool_definition(task, &task_def));
                    }
                }
            }
            AgentToolRef::Mcp { .. } => {
                // MCP tools are added from the client below
            }
        }
    }

    // Add MCP tool definitions
    #[cfg(feature = "mcp")]
    if let Some(client) = mcp_client {
        defs.extend(client.tool_definitions());
    }

    // Add ask_user if interactive
    if action_spec.interactive {
        defs.push(tools::ask_user_tool_definition());
    }

    defs
}

/// Check if a tool call is an MCP tool.
fn is_mcp_tool_call(
    tool_name: &str,
    #[cfg(feature = "mcp")] mcp_client: Option<&McpClientManager>,
) -> bool {
    #[cfg(feature = "mcp")]
    {
        mcp_client.is_some_and(|c| c.is_mcp_tool(tool_name))
    }
    #[cfg(not(feature = "mcp"))]
    {
        let _ = tool_name;
        false
    }
}

/// The persisted conversation as rig messages. A message that does not parse
/// is skipped (legacy history is lifted first, see [`crate::legacy_history`]).
fn parse_history(messages: &[serde_json::Value]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|value| serde_json::from_value::<Message>(value.clone()).ok())
        .collect()
}

/// The string a tool call is recorded under in `AgentConversationState`
/// (`PendingToolCall` / `AskUserCall` / `ResolvedToolResult::tool_call_id`):
/// the id the provider issued, or rig's minted handle when it issued none.
fn tool_call_handle(id: &CallId) -> String {
    id.wire().into_owned()
}

/// A text tool result answering `tc`.
fn tool_result_text(tc: &ToolCall, text: impl Into<String>) -> ToolResult {
    tc.result(vec![ToolResultContent::text(text)])
}

/// The result for a tool call recorded as `tool_call_id`, answered after a
/// suspension (task-tool child job or `ask_user`).
///
/// A tool result carries its call's id and the tool's name, so the call is
/// looked up in the history: by [`tool_call_handle`], or — for a call saved
/// by a rig-core 0.36 worker, which recorded OpenAI Responses calls under
/// their `fc_…` item id — by the provider item id.
fn resumed_tool_result(history: &[Message], tool_call_id: &str, text: &str) -> ToolResult {
    let call = history
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .find_map(|content| match content {
            AssistantContent::ToolCall(tc)
                if tc.id.wire() == tool_call_id
                    || tc.id.provider().and_then(|p| p.item_id.as_deref())
                        == Some(tool_call_id) =>
            {
                Some(tc)
            }
            _ => None,
        });
    match call {
        Some(tc) => tool_result_text(tc, text),
        // Not in the history (it never is for a well-formed state): answer
        // the id as the provider's, under a placeholder tool name.
        None => ToolResult {
            call: CallId::from_wire(tool_call_id),
            name: ToolName::new("unknown_tool").expect("non-empty tool name"),
            content: vec![ToolResultContent::text(text)],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ResolvedToolResult;
    use crate::test_support::capture_one_request;

    /// A context with no side effects: never cancelled, records nothing.
    struct NoopContext;

    #[async_trait::async_trait]
    impl AgentContext for NoopContext {
        async fn is_job_cancelled(&self, _job_id: Uuid) -> bool {
            false
        }
        async fn create_task_tool_job(
            &self,
            _job_id: Uuid,
            _step_name: &str,
            _task_name: &str,
            _input: serde_json::Value,
        ) -> Result<Uuid> {
            bail!("not used")
        }
        async fn save_agent_state(
            &self,
            _job_id: Uuid,
            _step_name: &str,
            _state: &AgentConversationState,
        ) -> Result<()> {
            Ok(())
        }
        async fn suspend_for_ask_user(
            &self,
            _job_id: Uuid,
            _step_name: &str,
            _state: &AgentConversationState,
            _message: &str,
        ) -> Result<()> {
            Ok(())
        }
        async fn log(&self, _job_id: Uuid, _message: &str) {}
    }

    fn openai_provider(api_endpoint: String) -> AgentProviderConfig {
        AgentProviderConfig {
            provider_type: "openai".to_string(),
            api_key: Some("test-key".to_string()),
            api_endpoint: Some(api_endpoint),
            model: "test-model".to_string(),
            max_tokens: 256,
            temperature: None,
            max_retries: 0,
        }
    }

    fn interactive_action() -> ActionDef {
        serde_json::from_value(serde_json::json!({"type": "agent", "interactive": true})).unwrap()
    }

    /// An `ask_user` suspension saved by a rig-core 0.36 worker (JSON as rig
    /// 0.36.0 serialized it): prompt; reasoning + text + task-tool call; its
    /// result; the `ask_user` call. The server has since recorded the user's
    /// answer under the call's id.
    fn legacy_ask_user_state() -> AgentConversationState {
        let messages: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
              {"content": [{"text": "Deploy it", "type": "text"}], "role": "user"},
              {"content": [
                 {"content": [{"content": {"signature": "sig", "text": "thinking..."}, "type": "text"}], "id": null},
                 {"text": "On it"},
                 {"additional_params": null, "call_id": null,
                  "function": {"arguments": {"env": "prod"}, "name": "strom_task_deploy"},
                  "id": "call_abc", "signature": null}
               ], "id": "msg_1", "role": "assistant"},
              {"content": [{"call_id": "call_abc", "content": [{"text": "{\"ok\":true}", "type": "text"}],
                            "id": "call_abc", "type": "toolresult"}], "role": "user"},
              {"content": [{"additional_params": null, "call_id": null,
                            "function": {"arguments": {"message": "Sure?"}, "name": "ask_user"},
                            "id": "call_ask", "signature": null}], "id": null, "role": "assistant"}
            ]"#,
        )
        .unwrap();
        AgentConversationState {
            messages,
            turn: 2,
            total_input_tokens: 10,
            total_output_tokens: 5,
            resolved_tool_results: vec![ResolvedToolResult {
                tool_call_id: "call_ask".to_string(),
                result_text: "yes".to_string(),
            }],
            ..AgentConversationState::new()
        }
    }

    /// A step suspended by a rig-core 0.36 worker resumes on this one with
    /// its whole conversation: both assistant tool calls and both answers
    /// reach the provider, each answer paired with its call.
    #[tokio::test]
    async fn resume_of_legacy_ask_user_state_replays_calls_and_answers() {
        let (base_url, server) = capture_one_request().await;
        let provider = openai_provider(format!("{base_url}/v1"));
        let result = dispatch_agent_loop(
            &NoopContext,
            Uuid::new_v4(),
            "agent",
            &interactive_action(),
            &provider,
            "test-model",
            "Deploy it",
            None,
            Some(legacy_ask_user_state()),
            None,
            Vec::new(),
            &[],
        )
        .await;
        assert!(result.is_err(), "the capture server answers 400");

        let messages = server.await.unwrap().body["messages"].clone();
        let messages = messages.as_array().unwrap();
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant", "tool"]);
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_abc");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"],
            "strom_task_deploy"
        );
        assert_eq!(messages[2]["tool_call_id"], "call_abc");
        assert_eq!(messages[3]["tool_calls"][0]["id"], "call_ask");
        assert_eq!(messages[4]["tool_call_id"], "call_ask");
        assert!(messages[4]["content"].to_string().contains("yes"));
    }

    fn assistant_with_calls(calls: Vec<ToolCall>) -> Message {
        Message::Assistant {
            id: None,
            content: calls.into_iter().map(AssistantContent::ToolCall).collect(),
        }
    }

    fn function(name: &str) -> rig_core::message::ToolFunction {
        rig_core::message::ToolFunction::new(ToolName::new(name).unwrap(), serde_json::json!({}))
    }

    #[test]
    fn resumed_tool_result_pairs_with_the_recorded_call() {
        let provider_call = ToolCall::from_wire("call_1", function("strom_task_a"));
        // A call the provider issued no id for (Gemini, Ollama): rig mints one.
        let minted_call = ToolCall::new(
            CallId::Local(rig_core::message::LocalCallId::new()),
            function("ask_user"),
        );
        // OpenAI Responses-style dual id; a rig-core 0.36 worker recorded it
        // under its `fc_…` item id.
        let dual_call = ToolCall::from_dual_wire("fc_9", "call_9", function("strom_task_b"));
        let history = vec![
            Message::user("go"),
            assistant_with_calls(vec![provider_call.clone(), minted_call.clone()]),
            assistant_with_calls(vec![dual_call.clone()]),
        ];

        for (call, recorded_as) in [
            (&provider_call, tool_call_handle(&provider_call.id)),
            (&minted_call, tool_call_handle(&minted_call.id)),
            (&dual_call, tool_call_handle(&dual_call.id)),
            (&dual_call, "fc_9".to_string()),
        ] {
            let result = resumed_tool_result(&history, &recorded_as, "done");
            assert_eq!(result.call, call.id, "recorded as {recorded_as}");
            assert_eq!(result.name, call.function.name);
            assert_eq!(result.content, vec![ToolResultContent::text("done")]);
        }
    }

    #[test]
    fn resumed_tool_result_without_recorded_call_still_serializes() {
        let result = resumed_tool_result(&[Message::user("go")], "call_x", "done");
        assert_eq!(result.call, CallId::from_wire("call_x"));
        assert_eq!(result.name, "unknown_tool");
        let message = Message::User {
            content: vec![UserContent::ToolResult(result)],
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(serde_json::from_value::<Message>(json).unwrap(), message);
    }

    #[test]
    fn tool_call_handle_is_the_wire_id() {
        assert_eq!(tool_call_handle(&CallId::from_wire("call_1")), "call_1");
        assert_eq!(
            tool_call_handle(&CallId::from_dual_wire("fc_1", "call_1")),
            "call_1"
        );
    }
}
