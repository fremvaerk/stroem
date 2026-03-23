//! Custom multi-turn agent dispatch loop.
//!
//! Uses rig-core's `CompletionModel::completion()` for raw LLM calls while
//! handling tool routing, state persistence, and suspension ourselves.
//!
//! This module is environment-agnostic: it takes an `AgentContext` trait
//! for operations that differ between server-side and worker-side execution
//! (e.g., creating child jobs, saving state, checking cancellation).

use anyhow::{bail, Context, Result};
use rig::completion::{AssistantContent, CompletionRequest, Message, ToolDefinition, Usage};
use rig::message::{Text, ToolCall, ToolResult, ToolResultContent, UserContent};
use rig::OneOrMany;
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
        let tool_result_contents: Vec<UserContent> = effective_tool_results
            .iter()
            .map(|(call_id, result_text)| {
                UserContent::ToolResult(ToolResult {
                    id: call_id.clone(),
                    call_id: Some(call_id.clone()),
                    content: OneOrMany::one(ToolResultContent::Text(Text {
                        text: result_text.clone(),
                    })),
                })
            })
            .collect();

        let tool_msg = Message::User {
            content: OneOrMany::many(tool_result_contents).unwrap_or_else(|_| {
                OneOrMany::one(UserContent::Text(Text {
                    text: "No tool results".to_string(),
                }))
            }),
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
        let mut chat_history: Vec<Message> = Vec::new();

        for msg_value in &conv_state.messages {
            if let Ok(msg) = serde_json::from_value::<Message>(msg_value.clone()) {
                chat_history.push(msg);
            }
        }

        let prompt_msg = if conv_state.turn == 1 {
            Message::User {
                content: OneOrMany::one(UserContent::Text(Text {
                    text: rendered_prompt.to_string(),
                })),
            }
        } else {
            chat_history.pop().unwrap_or_else(|| Message::User {
                content: OneOrMany::one(UserContent::Text(Text {
                    text: "Continue.".to_string(),
                })),
            })
        };

        let request = CompletionRequest {
            model: None,
            preamble: effective_system.clone(),
            chat_history: if chat_history.is_empty() {
                OneOrMany::one(prompt_msg.clone())
            } else {
                let mut all = chat_history;
                all.push(prompt_msg.clone());
                OneOrMany::many(all).unwrap_or_else(|_| OneOrMany::one(prompt_msg.clone()))
            },
            documents: vec![],
            tools: tool_defs.clone(),
            temperature: temperature.map(f64::from),
            max_tokens: Some(u64::from(max_tokens)),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };

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

        // Track token usage
        conv_state.total_input_tokens += response.usage.input_tokens;
        conv_state.total_output_tokens += response.usage.output_tokens;

        // Save the prompt to conversation history (only on first turn)
        if conv_state.turn == 1 {
            let user_msg = Message::User {
                content: OneOrMany::one(UserContent::Text(Text {
                    text: rendered_prompt.to_string(),
                })),
            };
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

        // Classify the response content
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for content in response.choice.iter() {
            match content {
                AssistantContent::Text(t) => text_parts.push(t.text.clone()),
                AssistantContent::ToolCall(tc) => tool_calls.push(tc.clone()),
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
                    input_tokens: conv_state.total_input_tokens,
                    output_tokens: conv_state.total_output_tokens,
                    total_tokens: conv_state.total_input_tokens + conv_state.total_output_tokens,
                    cached_input_tokens: 0,
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
                let err_result = UserContent::ToolResult(ToolResult {
                    id: tc.id.clone(),
                    call_id: tc.call_id.clone(),
                    content: OneOrMany::one(ToolResultContent::Text(Text {
                        text: format!("Error: unknown tool '{}'", tc.function.name),
                    })),
                });
                let msg = Message::User {
                    content: OneOrMany::one(err_result),
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

            let tool_result = UserContent::ToolResult(ToolResult {
                id: tc.id.clone(),
                call_id: tc.call_id.clone(),
                content: OneOrMany::one(ToolResultContent::Text(Text { text: result })),
            });
            let msg = Message::User {
                content: OneOrMany::one(tool_result),
            };
            conv_state.messages.push(serde_json::to_value(&msg)?);
        }
        #[cfg(not(feature = "mcp"))]
        let _ = &mcp_calls; // suppress unused warning

        // Handle ask_user (takes priority over task calls)
        if let Some(tc) = ask_user_call {
            if !action_spec.interactive {
                let err_result = UserContent::ToolResult(ToolResult {
                    id: tc.id.clone(),
                    call_id: tc.call_id.clone(),
                    content: OneOrMany::one(ToolResultContent::Text(Text {
                        text: "Error: ask_user is not available. The 'interactive' flag is not enabled on this action.".to_string(),
                    })),
                });
                let msg = Message::User {
                    content: OneOrMany::one(err_result),
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
                tool_call_id: tc.id.clone(),
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
                    .unwrap_or_else(|| tc.function.name.clone());

                // Convert underscores back to hyphens for task lookup
                let task_name_hyphen = task_name.replace('_', "-");

                // Check if the task exists in our tool set
                let resolved_task_name = if task_tool_infos.iter().any(|t| t.name == task_name) {
                    task_name.clone()
                } else if task_tool_infos.iter().any(|t| t.name == task_name_hyphen) {
                    task_name_hyphen.clone()
                } else {
                    let err_result = UserContent::ToolResult(ToolResult {
                        id: tc.id.clone(),
                        call_id: tc.call_id.clone(),
                        content: OneOrMany::one(ToolResultContent::Text(Text {
                            text: format!("Error: task '{}' not found", task_name),
                        })),
                    });
                    let msg = Message::User {
                        content: OneOrMany::one(err_result),
                    };
                    conv_state.messages.push(serde_json::to_value(&msg)?);
                    continue;
                };

                // Verify the task is in the allowed tool set
                let allowed = action_spec.tools.iter().any(|t| match t {
                    AgentToolRef::Task { task } => {
                        task == &resolved_task_name || task.replace('-', "_") == task_name
                    }
                    _ => false,
                });
                if !allowed {
                    let err_result = UserContent::ToolResult(ToolResult {
                        id: tc.id.clone(),
                        call_id: tc.call_id.clone(),
                        content: OneOrMany::one(ToolResultContent::Text(Text {
                            text: format!(
                                "Error: task '{}' is not in the allowed tool set",
                                resolved_task_name
                            ),
                        })),
                    });
                    let msg = Message::User {
                        content: OneOrMany::one(err_result),
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
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
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
            AgentToolRef::Task { task } => {
                if let Some(info) = task_tool_infos.iter().find(|t| &t.name == task) {
                    if let Some(ref schema) = info.parameters_schema {
                        // Use pre-built schema from server — avoids re-running
                        // `input_schema_to_json_schema` on an empty input map.
                        defs.push(rig::completion::ToolDefinition {
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
