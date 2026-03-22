//! Custom multi-turn agent dispatch loop.
//!
//! Uses rig-core's `CompletionModel::completion()` for raw LLM calls while
//! handling tool routing, state persistence, and suspension ourselves.
//! This is necessary because rig's `Agent::prompt()` handles multi-turn
//! internally but assumes all tools are synchronous — Strøm has async task
//! tools (child jobs) and ask_user (suspends step).

use anyhow::{bail, Context, Result};
use rig::completion::{
    AssistantContent, CompletionModel as _, CompletionRequest, Message, ToolDefinition, Usage,
};
use rig::message::{Text, ToolCall, ToolResult, ToolResultContent, UserContent};
use rig::OneOrMany;
use stroem_common::models::workflow::{ActionDef, AgentToolRef, WorkspaceConfig};
use stroem_db::JobRepo;
use uuid::Uuid;

use super::mcp_client::McpClientManager;
use super::state::{AgentConversationState, AskUserCall, PendingToolCall};
use super::tools;
use crate::config::AgentProviderConfig;

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

/// Run the multi-turn agent dispatch loop.
///
/// If `resume_state` is provided, the loop resumes from a previous suspension.
/// Otherwise, it starts a fresh conversation.
///
/// The loop calls the LLM, routes tool calls (MCP sync, task async, ask_user),
/// and continues until the LLM produces a final text response, calls an async
/// tool, or reaches the max turns limit.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_agent_loop(
    pool: &sqlx::PgPool,
    workspace_config: &WorkspaceConfig,
    workspace_name: &str,
    job_id: Uuid,
    step_name: &str,
    action_spec: &ActionDef,
    provider_config: &AgentProviderConfig,
    model_name: &str,
    rendered_prompt: &str,
    rendered_system: Option<&str>,
    resume_state: Option<AgentConversationState>,
    mcp_client: Option<&McpClientManager>,
    tool_results: Vec<(String, String)>, // (tool_call_id, result_text) for resolved tools
    agents_config: Option<&crate::config::AgentsConfig>,
    revision: Option<&str>,
) -> Result<DispatchOutcome> {
    let max_turns = action_spec.max_turns.unwrap_or(DEFAULT_MAX_TURNS).min(100);

    // Build tool definitions
    let tool_defs = build_tool_definitions(action_spec, workspace_config, mcp_client);

    // Initialize or restore conversation state
    let mut conv_state = resume_state.unwrap_or_default();

    // If resuming with tool results, inject them as user messages
    if !tool_results.is_empty() {
        let tool_result_contents: Vec<UserContent> = tool_results
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
        conv_state.messages.push(serde_json::to_value(&tool_msg)?);
    }

    // Build effective system prompt with schema suffix if output is set
    let schema_suffix: Option<String> = action_spec.output.as_ref().map(|output_def| {
        let schema = output_def.to_json_schema();
        let schema_str =
            serde_json::to_string_pretty(&schema).unwrap_or_else(|_| schema.to_string());
        format!(
            "\n\nYou must respond with a JSON object matching this schema:\n{}\nReturn ONLY the JSON object, no other text, no markdown code fences.",
            schema_str
        )
    });

    let effective_system: Option<String> = match (rendered_system, schema_suffix.as_deref()) {
        (Some(sys), Some(suffix)) => Some(format!("{}{}", sys, suffix)),
        (None, Some(suffix)) => Some(suffix.to_string()),
        (Some(sys), None) => Some(sys.to_string()),
        (None, None) => None,
    };

    // Resolve temperature and max_tokens
    let temperature = action_spec.temperature.or(provider_config.temperature);
    let max_tokens = action_spec.max_tokens.unwrap_or(provider_config.max_tokens);

    // Main dispatch loop
    loop {
        conv_state.turn += 1;

        // Check if job has been cancelled
        if let Ok(Some(job)) = JobRepo::get(pool, job_id).await {
            if job.status == "cancelled" {
                return Ok(DispatchOutcome::Failed {
                    error: "Job was cancelled".to_string(),
                });
            }
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

        // Add conversation history from state (skip system messages — handled via preamble)
        for msg_value in &conv_state.messages {
            if let Ok(msg) = serde_json::from_value::<Message>(msg_value.clone()) {
                chat_history.push(msg);
            }
        }

        // The prompt is the initial user message (only on first turn)
        let prompt_msg = if conv_state.turn == 1 {
            Message::User {
                content: OneOrMany::one(UserContent::Text(Text {
                    text: rendered_prompt.to_string(),
                })),
            }
        } else {
            // On subsequent turns, the last message in chat_history is the tool result
            // which should be used as the "prompt" for rig's CompletionRequest
            chat_history.pop().unwrap_or_else(|| Message::User {
                content: OneOrMany::one(UserContent::Text(Text {
                    text: "Continue.".to_string(),
                })),
            })
        };

        // Build the CompletionRequest
        let request = CompletionRequest {
            model: None,
            preamble: effective_system.clone(), // Use preamble for system prompt
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
            call_completion(provider_config, model_name, request),
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
                _ => {} // Reasoning, Image — skip
            }
        }

        // No tool calls → final response
        if tool_calls.is_empty() {
            let response_text = text_parts.join("\n");
            let output = build_final_output(action_spec, &response_text)?;
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
            } else if mcp_client.is_some_and(|c| c.is_mcp_tool(&tc.function.name)) {
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

        // Handle ask_user (takes priority over task calls)
        if let Some(tc) = ask_user_call {
            if !action_spec.interactive {
                // ask_user not allowed — return error as tool result
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
                continue; // Re-enter loop with error feedback
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
                // (tool names use underscores but task names may use hyphens)
                let task_name_hyphen = task_name.replace('_', "-");
                let resolved_task_name = if workspace_config.tasks.contains_key(&task_name) {
                    task_name.clone()
                } else if workspace_config.tasks.contains_key(&task_name_hyphen) {
                    task_name_hyphen
                } else {
                    // Task not found — return error to LLM
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

                // Create child job for this task tool call
                let input = if tc.function.arguments.is_object() {
                    tc.function.arguments.clone()
                } else {
                    serde_json::json!({})
                };

                let child_job_id = crate::job_creator::create_job_for_task(
                    pool,
                    workspace_config,
                    workspace_name,
                    &resolved_task_name,
                    input,
                    "agent_tool",
                    Some(&format!("{}/{}", job_id, step_name)),
                    revision,
                    agents_config,
                )
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

            // Wait for task tool child jobs to complete
            return Ok(DispatchOutcome::WaitingForTools { state: conv_state });
        }

        // If we only had MCP calls (all resolved synchronously), loop continues
    }
}

/// Build tool definitions from action spec, workspace config, and MCP client.
fn build_tool_definitions(
    action_spec: &ActionDef,
    workspace_config: &WorkspaceConfig,
    mcp_client: Option<&McpClientManager>,
) -> Vec<ToolDefinition> {
    let mut defs = Vec::new();

    for tool_ref in &action_spec.tools {
        match tool_ref {
            AgentToolRef::Task { task } => {
                if let Some(task_def) = workspace_config.tasks.get(task) {
                    defs.push(tools::task_to_tool_definition(task, task_def));
                }
            }
            AgentToolRef::Mcp { .. } => {
                // MCP tools are added from the client below
            }
        }
    }

    // Add MCP tool definitions
    if let Some(client) = mcp_client {
        defs.extend(client.tool_definitions());
    }

    // Add ask_user if interactive
    if action_spec.interactive {
        defs.push(tools::ask_user_tool_definition());
    }

    defs
}

/// Build the final output JSON from the agent's response text.
fn build_final_output(action_spec: &ActionDef, response_text: &str) -> Result<serde_json::Value> {
    if action_spec.output.is_some() {
        // Structured output — parse as JSON
        let trimmed = response_text.trim();
        let json_str = super::dispatch::strip_code_fences(trimmed);
        match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(json) if json.is_object() => Ok(json),
            Ok(json) => {
                bail!(
                    "output requires a JSON object but got {}",
                    super::dispatch::json_type_name(&json)
                );
            }
            Err(e) => {
                bail!(
                    "output requires a JSON object but response could not be parsed: {}. Response was: {}",
                    e,
                    super::dispatch::truncate_for_error(trimmed, 200)
                );
            }
        }
    } else {
        // Unstructured — try JSON, fall back to text wrapper
        match serde_json::from_str::<serde_json::Value>(response_text) {
            Ok(json) if json.is_object() => Ok(json),
            _ => Ok(serde_json::json!({ "text": response_text })),
        }
    }
}

/// Call the LLM completion endpoint using rig-core.
///
/// Uses the same provider dispatch as single-turn, but via the lower-level
/// `CompletionModel::completion()` interface instead of `Agent::prompt()`.
async fn call_completion(
    provider_config: &AgentProviderConfig,
    model_name: &str,
    request: CompletionRequest,
) -> Result<CompletionResponse> {
    use rig::prelude::CompletionClient as _;

    /// Build a provider client, call completion, and erase the raw response type.
    macro_rules! call_model {
        ($client_type:ty, $label:expr, $api_key:expr) => {{
            let mut builder = <$client_type>::builder().api_key($api_key);
            if let Some(ref endpoint) = provider_config.api_endpoint {
                builder = builder.base_url(endpoint);
            }
            let client = builder
                .build()
                .context(concat!("Failed to build ", $label, " client"))?;
            let model = client.completion_model(model_name);
            let resp = model
                .completion(request)
                .await
                .context(concat!($label, " completion call failed"))?;
            return Ok(CompletionResponse {
                choice: resp.choice,
                usage: resp.usage,
                message_id: resp.message_id,
            });
        }};
    }

    let require_api_key = || -> Result<&str> {
        provider_config
            .api_key
            .as_deref()
            .context("Agent provider requires api_key")
    };

    // Each macro arm returns early via `return Ok(...)` to erase the
    // provider-specific raw response type. The match itself is `!` (never).
    match provider_config.provider_type.as_str() {
        "anthropic" => call_model!(
            rig::providers::anthropic::Client,
            "Anthropic",
            require_api_key()?.to_string()
        ),
        "openai" => call_model!(
            rig::providers::openai::CompletionsClient,
            "OpenAI",
            require_api_key()?.to_string()
        ),
        "cohere" => call_model!(
            rig::providers::cohere::Client,
            "Cohere",
            require_api_key()?.to_string()
        ),
        "deepseek" => call_model!(
            rig::providers::deepseek::Client,
            "DeepSeek",
            require_api_key()?.to_string()
        ),
        "gemini" => call_model!(
            rig::providers::gemini::Client,
            "Gemini",
            require_api_key()?.to_string()
        ),
        "groq" => call_model!(
            rig::providers::groq::Client,
            "Groq",
            require_api_key()?.to_string()
        ),
        "mistral" => call_model!(
            rig::providers::mistral::Client,
            "Mistral",
            require_api_key()?.to_string()
        ),
        "openrouter" => call_model!(
            rig::providers::openrouter::Client,
            "OpenRouter",
            require_api_key()?.to_string()
        ),
        "together" => call_model!(
            rig::providers::together::Client,
            "Together",
            require_api_key()?.to_string()
        ),
        "xai" => call_model!(
            rig::providers::xai::Client,
            "xAI",
            require_api_key()?.to_string()
        ),
        "perplexity" => call_model!(
            rig::providers::perplexity::Client,
            "Perplexity",
            require_api_key()?.to_string()
        ),
        "galadriel" => call_model!(
            rig::providers::galadriel::Client,
            "Galadriel",
            require_api_key()?.to_string()
        ),
        "huggingface" => call_model!(
            rig::providers::huggingface::Client,
            "HuggingFace",
            require_api_key()?.to_string()
        ),
        "hyperbolic" => call_model!(
            rig::providers::hyperbolic::Client,
            "Hyperbolic",
            require_api_key()?.to_string()
        ),
        "mira" => call_model!(
            rig::providers::mira::Client,
            "Mira",
            require_api_key()?.to_string()
        ),
        "moonshot" => call_model!(
            rig::providers::moonshot::Client,
            "Moonshot",
            require_api_key()?.to_string()
        ),
        "ollama" => call_model!(
            rig::providers::ollama::Client,
            "Ollama",
            rig::client::Nothing
        ),
        "llamafile" => call_model!(
            rig::providers::llamafile::Client,
            "Llamafile",
            rig::client::Nothing
        ),
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
            let model = client.completion_model(model_name);
            let resp = model
                .completion(request)
                .await
                .context("Azure completion call failed")?;
            #[allow(clippy::needless_return)]
            return Ok(CompletionResponse {
                choice: resp.choice,
                usage: resp.usage,
                message_id: resp.message_id,
            });
        }
        other => bail!("Unknown agent provider type: {}", other),
    }
}

/// Simplified completion response (provider-independent).
struct CompletionResponse {
    choice: OneOrMany<AssistantContent>,
    usage: Usage,
    message_id: Option<String>,
}
