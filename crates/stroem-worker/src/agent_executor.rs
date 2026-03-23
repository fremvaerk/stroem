//! Worker-side agent step executor.
//!
//! Looks up provider config from local worker config, connects MCP clients,
//! and runs the agent dispatch loop from `stroem-agent`.

use anyhow::Result;
use stroem_agent::config::AgentProviderConfig;
use stroem_agent::loop_dispatch::{AgentContext, DispatchOutcome, TaskToolInfo};
use stroem_agent::state::AgentConversationState;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::client::{ClaimedStep, ServerClient};

/// Result of executing an agent step on the worker.
pub enum AgentResult {
    /// Step completed successfully with output.
    Completed { output: serde_json::Value },
    /// Step is waiting for task-tool child jobs — worker is done, step stays running.
    WaitingForTools,
    /// Step is suspended for `ask_user` — worker is done, step is suspended.
    Suspended,
    /// Step failed with an error.
    Failed { error: String },
}

/// [`AgentContext`] implementation that calls server endpoints via HTTP.
struct WorkerAgentContext<'a> {
    client: &'a ServerClient,
    step_name: String,
}

#[async_trait::async_trait]
impl AgentContext for WorkerAgentContext<'_> {
    async fn is_job_cancelled(&self, job_id: Uuid) -> bool {
        self.client
            .check_job_cancelled(job_id)
            .await
            .unwrap_or(false)
    }

    async fn create_task_tool_job(
        &self,
        job_id: Uuid,
        step_name: &str,
        task_name: &str,
        input: serde_json::Value,
    ) -> Result<Uuid> {
        self.client
            .agent_task_tool(job_id, step_name, task_name, input)
            .await
    }

    async fn save_agent_state(
        &self,
        job_id: Uuid,
        step_name: &str,
        state: &AgentConversationState,
    ) -> Result<()> {
        let state_value = serde_json::to_value(state)?;
        self.client
            .agent_save_state(job_id, step_name, state_value)
            .await
    }

    async fn suspend_for_ask_user(
        &self,
        job_id: Uuid,
        step_name: &str,
        state: &AgentConversationState,
        message: &str,
    ) -> Result<()> {
        let state_value = serde_json::to_value(state)?;
        self.client
            .agent_suspend_step(job_id, step_name, state_value, message)
            .await
    }

    async fn log(&self, job_id: Uuid, message: &str) {
        tracing::info!(job_id = %job_id, "{}", message);
        // Best-effort push to server log stream
        let ts = chrono::Utc::now().to_rfc3339();
        let line = serde_json::json!({
            "timestamp": ts,
            "stream": "stderr",
            "line": message,
        });
        if let Err(e) = self
            .client
            .push_logs(job_id, &self.step_name, vec![line])
            .await
        {
            tracing::debug!("Failed to push agent log to server: {:#}", e);
        }
    }
}

/// Execute an agent step on the worker.
///
/// Looks up the provider config from the worker's local `agents_config`,
/// connects MCP clients if needed, and runs the dispatch loop from
/// `stroem_agent`.
///
/// Note: `cancel_token` is not currently wired into the LLM call timeout.
/// Cancellation is handled by polling `ctx.is_job_cancelled()` each turn
/// (up to 120s delay for an in-flight LLM call).
pub async fn execute_agent_step(
    client: &ServerClient,
    step: &ClaimedStep,
    agents_config: Option<&stroem_agent::config::AgentsConfig>,
    _cancel_token: CancellationToken,
) -> AgentResult {
    let job_id = step.job_id;
    let step_name = &step.step_name;

    // Resolve provider name from claim response
    let provider_name = match &step.agent_provider_name {
        Some(name) => name.clone(),
        None => {
            return AgentResult::Failed {
                error: "Agent step missing provider name in claim response".to_string(),
            };
        }
    };

    // Require local agents config
    let agents = match agents_config {
        Some(c) => c,
        None => {
            return AgentResult::Failed {
                error: "Worker has no agents config — cannot execute agent steps. \
                        Add an 'agents:' section to worker-config.yaml."
                    .to_string(),
            };
        }
    };

    // Look up the specific provider
    let provider_config: AgentProviderConfig = match agents.providers.get(&provider_name) {
        Some(p) => p.clone(),
        None => {
            return AgentResult::Failed {
                error: format!(
                    "Worker does not have provider '{}' configured. \
                     Add it to agents.providers in worker-config.yaml.",
                    provider_name
                ),
            };
        }
    };

    // Require a rendered prompt
    let prompt = match &step.agent_prompt {
        Some(p) if !p.trim().is_empty() => p.clone(),
        _ => {
            return AgentResult::Failed {
                error: "Agent step has no rendered prompt".to_string(),
            };
        }
    };

    let system_prompt = step.agent_system_prompt.as_deref();

    // Deserialise the action spec for agent-specific fields
    let action_spec: stroem_common::models::workflow::ActionDef = match &step.action_spec {
        Some(spec) => match serde_json::from_value(spec.clone()) {
            Ok(a) => a,
            Err(e) => {
                return AgentResult::Failed {
                    error: format!("Failed to deserialize agent action_spec: {}", e),
                };
            }
        },
        None => {
            return AgentResult::Failed {
                error: "Agent step missing action_spec".to_string(),
            };
        }
    };

    // Determine effective model name (action override → provider default)
    let model_name = action_spec
        .model
        .as_deref()
        .unwrap_or(&provider_config.model)
        .to_string();

    // Restore conversation state for resumed steps
    let resume_state: Option<AgentConversationState> = step
        .agent_state
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // Build task tool metadata from claim response
    let task_tool_infos = build_task_tool_infos(step.agent_tool_tasks.as_ref());

    // Connect MCP clients if any MCP tool references are present.
    // The `mcp` feature on this crate forwards to stroem-agent/mcp, so
    // McpClientManager is always available here.
    #[cfg(feature = "mcp")]
    let mcp_client = {
        use stroem_common::models::workflow::AgentToolRef;

        let mcp_server_names: Vec<&str> = action_spec
            .tools
            .iter()
            .filter_map(|t| match t {
                AgentToolRef::Mcp { mcp } => Some(mcp.as_str()),
                _ => None,
            })
            .collect();

        if !mcp_server_names.is_empty() {
            let mcp_servers = step.mcp_servers.as_ref().cloned().unwrap_or_default();
            match stroem_agent::mcp_client::McpClientManager::connect(
                &mcp_servers,
                &mcp_server_names,
            )
            .await
            {
                Ok(mgr) => Some(mgr),
                Err(e) => {
                    return AgentResult::Failed {
                        error: format!("Failed to connect MCP servers: {:#}", e),
                    };
                }
            }
        } else {
            None
        }
    };

    let ctx = WorkerAgentContext {
        client,
        step_name: step_name.to_string(),
    };

    let start = std::time::Instant::now();

    // Dispatch: multi-turn when tools or interactive flag are present; otherwise single-turn.
    let is_multi_turn = !action_spec.tools.is_empty() || action_spec.interactive;

    let result = if is_multi_turn {
        stroem_agent::loop_dispatch::dispatch_agent_loop(
            &ctx,
            job_id,
            step_name,
            &action_spec,
            &provider_config,
            &model_name,
            &prompt,
            system_prompt,
            resume_state,
            #[cfg(feature = "mcp")]
            mcp_client.as_ref(),
            #[cfg(not(feature = "mcp"))]
            None,
            vec![],
            &task_tool_infos,
        )
        .await
    } else {
        // Single-turn — wrap into DispatchOutcome for uniform handling below
        match stroem_agent::dispatch::execute_single_turn_with_retry(
            &provider_config,
            &model_name,
            &prompt,
            system_prompt,
            &action_spec,
        )
        .await
        {
            Ok(resp) => {
                let mut output = resp.content;
                if let Some(obj) = output.as_object_mut() {
                    obj.insert(
                        "_meta".to_string(),
                        serde_json::json!({
                            "model": model_name,
                            "provider": provider_name,
                            "input_tokens": resp.input_tokens,
                            "output_tokens": resp.output_tokens,
                            "latency_ms": start.elapsed().as_millis() as u64,
                            "turns": 1,
                        }),
                    );
                }
                Ok(DispatchOutcome::Completed {
                    output,
                    usage: rig::completion::Usage {
                        input_tokens: resp.input_tokens,
                        output_tokens: resp.output_tokens,
                        total_tokens: resp.input_tokens + resp.output_tokens,
                        cached_input_tokens: 0,
                    },
                    turns: 1,
                })
            }
            Err(e) => Ok(DispatchOutcome::Failed {
                error: format!("{:#}", e),
            }),
        }
    };

    // Shut down MCP clients after the dispatch loop finishes
    #[cfg(feature = "mcp")]
    if let Some(mgr) = mcp_client {
        mgr.shutdown().await;
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(DispatchOutcome::Completed {
            mut output,
            usage,
            turns,
        }) => {
            // Multi-turn adds _meta here; single-turn already added it above
            if is_multi_turn {
                if let Some(obj) = output.as_object_mut() {
                    obj.insert(
                        "_meta".to_string(),
                        serde_json::json!({
                            "model": model_name,
                            "provider": provider_name,
                            "input_tokens": usage.input_tokens,
                            "output_tokens": usage.output_tokens,
                            "latency_ms": elapsed_ms,
                            "turns": turns,
                        }),
                    );
                }
            }
            tracing::info!(
                job_id = %job_id,
                step = %step_name,
                elapsed_ms,
                turns,
                "[agent] Step completed"
            );
            AgentResult::Completed { output }
        }
        Ok(DispatchOutcome::WaitingForTools { .. }) => {
            tracing::info!(
                job_id = %job_id,
                step = %step_name,
                "[agent] Waiting for task-tool child jobs"
            );
            AgentResult::WaitingForTools
        }
        Ok(DispatchOutcome::WaitingForUser { .. }) => {
            tracing::info!(
                job_id = %job_id,
                step = %step_name,
                "[agent] Suspended for ask_user"
            );
            AgentResult::Suspended
        }
        Ok(DispatchOutcome::Failed { error }) => {
            tracing::error!(
                job_id = %job_id,
                step = %step_name,
                "[agent] Step failed: {}",
                error
            );
            AgentResult::Failed { error }
        }
        Err(e) => {
            let error = format!("{:#}", e);
            tracing::error!(
                job_id = %job_id,
                step = %step_name,
                "[agent] Step failed: {}",
                error
            );
            AgentResult::Failed { error }
        }
    }
}

/// Build [`TaskToolInfo`] list from the claim response's `agent_tool_tasks` JSON.
///
/// The server encodes task tool metadata as:
/// ```json
/// {
///   "my-task": { "description": "Does X", "input": { "type": "object", ... } },
///   "other-task": {}
/// }
/// ```
///
/// The `input` field, when present, is a pre-built JSON Schema produced by
/// `input_schema_to_json_schema` on the server. We carry it through as
/// `parameters_schema` so that `build_tool_definitions` can pass it directly to
/// the LLM rather than reconstructing it from an empty `input` map.
fn build_task_tool_infos(agent_tool_tasks: Option<&serde_json::Value>) -> Vec<TaskToolInfo> {
    let Some(tasks) = agent_tool_tasks.and_then(|v| v.as_object()) else {
        return vec![];
    };

    tasks
        .iter()
        .map(|(name, info)| TaskToolInfo {
            name: name.clone(),
            description: info
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from),
            input: std::collections::HashMap::new(),
            parameters_schema: info.get("input").cloned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_step() -> ClaimedStep {
        ClaimedStep {
            job_id: uuid::Uuid::new_v4(),
            workspace: "default".to_string(),
            task_name: "test".to_string(),
            step_name: "agent-step".to_string(),
            action_name: "my-agent".to_string(),
            action_type: "agent".to_string(),
            action_image: None,
            action_spec: Some(serde_json::json!({
                "type": "agent",
                "provider": "anthropic",
                "prompt": "Hello"
            })),
            input: None,
            runner: None,
            timeout_secs: None,
            revision: None,
            agent_provider_name: Some("anthropic".to_string()),
            agent_prompt: Some("Hello world".to_string()),
            agent_system_prompt: None,
            mcp_servers: None,
            agent_state: None,
            agent_tool_tasks: None,
        }
    }

    fn make_providers(
    ) -> std::collections::HashMap<String, stroem_agent::config::AgentProviderConfig> {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "anthropic".to_string(),
            stroem_agent::config::AgentProviderConfig {
                provider_type: "anthropic".to_string(),
                api_key: Some("test-key".to_string()),
                api_endpoint: None,
                model: "claude-3-haiku-20240307".to_string(),
                max_tokens: 100,
                temperature: None,
                max_retries: 0,
            },
        );
        providers
    }

    #[tokio::test]
    async fn test_execute_agent_step_missing_provider_name() {
        let client = ServerClient::new("http://localhost:1", "token", None, None);
        let mut step = make_test_step();
        step.agent_provider_name = None;
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = execute_agent_step(&client, &step, None, cancel).await;
        match result {
            AgentResult::Failed { error } => {
                assert!(
                    error.contains("provider name"),
                    "expected 'provider name' in error: {error}"
                );
            }
            _ => panic!("Expected AgentResult::Failed"),
        }
    }

    #[tokio::test]
    async fn test_execute_agent_step_missing_agents_config() {
        let client = ServerClient::new("http://localhost:1", "token", None, None);
        let step = make_test_step();
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = execute_agent_step(&client, &step, None, cancel).await;
        match result {
            AgentResult::Failed { error } => {
                assert!(
                    error.contains("no agents config"),
                    "expected 'no agents config' in error: {error}"
                );
            }
            _ => panic!("Expected AgentResult::Failed"),
        }
    }

    #[tokio::test]
    async fn test_execute_agent_step_unknown_provider() {
        let client = ServerClient::new("http://localhost:1", "token", None, None);
        let step = make_test_step();
        let cancel = tokio_util::sync::CancellationToken::new();
        // Empty providers map — "anthropic" is not configured
        let agents = stroem_agent::config::AgentsConfig {
            providers: std::collections::HashMap::new(),
        };
        let result = execute_agent_step(&client, &step, Some(&agents), cancel).await;
        match result {
            AgentResult::Failed { error } => {
                assert!(
                    error.contains("anthropic"),
                    "expected provider name 'anthropic' in error: {error}"
                );
            }
            _ => panic!("Expected AgentResult::Failed"),
        }
    }

    #[tokio::test]
    async fn test_execute_agent_step_missing_prompt() {
        let client = ServerClient::new("http://localhost:1", "token", None, None);
        let mut step = make_test_step();
        step.agent_prompt = None;
        let cancel = tokio_util::sync::CancellationToken::new();
        let agents = stroem_agent::config::AgentsConfig {
            providers: make_providers(),
        };
        let result = execute_agent_step(&client, &step, Some(&agents), cancel).await;
        match result {
            AgentResult::Failed { error } => {
                assert!(
                    error.contains("prompt"),
                    "expected 'prompt' in error: {error}"
                );
            }
            _ => panic!("Expected AgentResult::Failed"),
        }
    }

    #[tokio::test]
    async fn test_execute_agent_step_empty_prompt() {
        let client = ServerClient::new("http://localhost:1", "token", None, None);
        let mut step = make_test_step();
        step.agent_prompt = Some("   ".to_string()); // whitespace-only is also rejected
        let cancel = tokio_util::sync::CancellationToken::new();
        let agents = stroem_agent::config::AgentsConfig {
            providers: make_providers(),
        };
        let result = execute_agent_step(&client, &step, Some(&agents), cancel).await;
        match result {
            AgentResult::Failed { error } => {
                assert!(
                    error.contains("prompt"),
                    "expected 'prompt' in error: {error}"
                );
            }
            _ => panic!("Expected AgentResult::Failed"),
        }
    }

    #[tokio::test]
    async fn test_execute_agent_step_missing_action_spec() {
        let client = ServerClient::new("http://localhost:1", "token", None, None);
        let mut step = make_test_step();
        step.action_spec = None;
        let cancel = tokio_util::sync::CancellationToken::new();
        let agents = stroem_agent::config::AgentsConfig {
            providers: make_providers(),
        };
        let result = execute_agent_step(&client, &step, Some(&agents), cancel).await;
        match result {
            AgentResult::Failed { error } => {
                assert!(
                    error.contains("action_spec"),
                    "expected 'action_spec' in error: {error}"
                );
            }
            _ => panic!("Expected AgentResult::Failed"),
        }
    }

    #[test]
    fn test_build_task_tool_infos_empty() {
        let result = build_task_tool_infos(None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_task_tool_infos_null() {
        let result = build_task_tool_infos(Some(&serde_json::Value::Null));
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_task_tool_infos_with_tasks() {
        let tasks = serde_json::json!({
            "my-task": { "description": "Does something" },
            "other-task": {}
        });
        let result = build_task_tool_infos(Some(&tasks));
        assert_eq!(result.len(), 2);

        let my_task = result.iter().find(|t| t.name == "my-task").unwrap();
        assert_eq!(my_task.description, Some("Does something".to_string()));
        // No `input` key in JSON → parameters_schema is None
        assert!(my_task.parameters_schema.is_none());

        let other = result.iter().find(|t| t.name == "other-task").unwrap();
        assert!(other.description.is_none());
        assert!(other.parameters_schema.is_none());
    }

    #[test]
    fn test_build_task_tool_infos_with_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The message" }
            },
            "required": ["message"]
        });
        let tasks = serde_json::json!({
            "send-message": {
                "description": "Sends a message",
                "input": schema
            }
        });
        let result = build_task_tool_infos(Some(&tasks));
        assert_eq!(result.len(), 1);

        let task = &result[0];
        assert_eq!(task.name, "send-message");
        assert_eq!(task.description, Some("Sends a message".to_string()));
        assert!(task.input.is_empty(), "input map must always be empty");

        let ps = task
            .parameters_schema
            .as_ref()
            .expect("parameters_schema must be set");
        assert_eq!(ps["type"], "object");
        assert!(ps["properties"]["message"]["type"] == "string");
    }

    #[test]
    fn test_build_task_tool_infos_input_is_empty() {
        let tasks = serde_json::json!({
            "task-a": { "description": "Task A" }
        });
        let result = build_task_tool_infos(Some(&tasks));
        assert_eq!(result.len(), 1);
        assert!(result[0].input.is_empty(), "input map must be empty");
        assert!(
            result[0].parameters_schema.is_none(),
            "parameters_schema must be None when no input key in JSON"
        );
    }
}
