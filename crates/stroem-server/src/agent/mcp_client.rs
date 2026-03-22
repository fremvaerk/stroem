//! MCP client manager for connecting to external MCP servers.
//!
//! Agent steps with MCP tool references connect to the configured servers,
//! discover their tools, and execute tool calls synchronously during the
//! dispatch loop.

use anyhow::{bail, Context, Result};
use rmcp::model::{CallToolRequestParams, Content, RawContent};
use std::collections::HashMap;
use stroem_common::models::workflow::McpServerDef;

/// A live connection to an MCP server.
struct McpConnection {
    /// The running MCP client service (type-erased via Box).
    service: Box<dyn McpService>,
    /// Tools advertised by this server (name → description, schema).
    tools: Vec<McpToolInfo>,
    /// Server name (for logging / prefixing).
    #[allow(dead_code)]
    name: String,
}

/// Minimal tool info extracted from the MCP server's tool listing.
struct McpToolInfo {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

/// Type-erased trait for MCP service operations we need.
#[async_trait::async_trait]
trait McpService: Send + Sync {
    async fn call_tool(&self, params: CallToolRequestParams) -> Result<Vec<Content>>;
    async fn cancel(self: Box<Self>) -> Result<()>;
}

/// Concrete implementation for stdio transport.
struct StdioMcpService {
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
}

#[async_trait::async_trait]
impl McpService for StdioMcpService {
    async fn call_tool(&self, params: CallToolRequestParams) -> Result<Vec<Content>> {
        let result = self
            .service
            .call_tool(params)
            .await
            .context("MCP tool call failed")?;
        Ok(result.content)
    }

    async fn cancel(self: Box<Self>) -> Result<()> {
        self.service
            .cancel()
            .await
            .map_err(|e| anyhow::anyhow!("MCP shutdown failed: {}", e))?;
        Ok(())
    }
}

/// Manages connections to multiple MCP servers.
///
/// Created per agent step dispatch — connects to all referenced MCP servers,
/// discovers their tools, and provides methods to call tools and shut down.
pub struct McpClientManager {
    connections: HashMap<String, McpConnection>,
}

impl McpClientManager {
    /// Connect to the given MCP servers and discover their tools.
    ///
    /// Currently only supports `stdio` transport. SSE transport support
    /// can be added when the rmcp crate's SSE client features are stable.
    pub async fn connect(
        mcp_servers: &HashMap<String, McpServerDef>,
        server_names: &[&str],
    ) -> Result<Self> {
        let mut connections = HashMap::new();

        for &name in server_names {
            let server_def = match mcp_servers.get(name) {
                Some(def) => def,
                None => {
                    tracing::warn!("MCP server '{}' not found in workspace config", name);
                    continue;
                }
            };

            match server_def.transport.as_str() {
                "stdio" => {
                    let command = server_def
                        .command
                        .as_deref()
                        .context(format!("MCP server '{}' missing command", name))?;

                    let args: Vec<String> = server_def.args.clone().unwrap_or_default();

                    // Build the child process command
                    // SECURITY: env_clear() prevents inheriting parent process secrets.
                    // Only explicitly configured env vars are passed.
                    let mut cmd = tokio::process::Command::new(command);
                    cmd.env_clear();
                    cmd.args(&args);

                    // Set only the explicitly configured environment variables
                    for (key, value) in &server_def.env {
                        cmd.env(key, value);
                    }

                    tracing::info!(
                        server = name,
                        command = command,
                        "Spawning MCP server process"
                    );

                    let transport =
                        rmcp::transport::TokioChildProcess::new(cmd).context(format!(
                            "Failed to spawn MCP server '{}' (command: {})",
                            name, command
                        ))?;

                    let timeout = std::time::Duration::from_secs(u64::from(
                        server_def.timeout_secs.unwrap_or(30),
                    ));
                    let service = match tokio::time::timeout(
                        timeout,
                        rmcp::serve_client((), transport),
                    )
                    .await
                    {
                        Ok(result) => {
                            result.context(format!("Failed to initialize MCP server '{}'", name))?
                        }
                        Err(_) => {
                            bail!(
                                "MCP server '{}' initialization timed out after {}s",
                                name,
                                timeout.as_secs()
                            );
                        }
                    };

                    // Discover tools
                    let tools_result =
                        match tokio::time::timeout(timeout, service.list_tools(Default::default()))
                            .await
                        {
                            Ok(result) => result.context(format!(
                                "Failed to list tools from MCP server '{}'",
                                name
                            ))?,
                            Err(_) => {
                                bail!(
                                    "MCP server '{}' tool discovery timed out after {}s",
                                    name,
                                    timeout.as_secs()
                                );
                            }
                        };

                    let tools: Vec<McpToolInfo> = tools_result
                        .tools
                        .into_iter()
                        .map(|t| {
                            let input_schema =
                                serde_json::to_value(&*t.input_schema).unwrap_or_default();
                            McpToolInfo {
                                name: t.name.to_string(),
                                description: t.description.as_deref().unwrap_or("").to_string(),
                                input_schema,
                            }
                        })
                        .collect();

                    tracing::info!(
                        server = name,
                        tool_count = tools.len(),
                        "Connected to MCP server"
                    );

                    connections.insert(
                        name.to_string(),
                        McpConnection {
                            service: Box::new(StdioMcpService { service }),
                            tools,
                            name: name.to_string(),
                        },
                    );
                }
                "sse" => {
                    // SSE transport not yet supported in agent dispatch.
                    tracing::warn!(
                        "MCP server '{}' uses SSE transport which is not yet supported for agent tools",
                        name
                    );
                }
                other => {
                    bail!(
                        "MCP server '{}' has unknown transport type '{}'",
                        name,
                        other
                    );
                }
            }
        }

        Ok(Self { connections })
    }

    /// Get tool definitions from all connected MCP servers.
    ///
    /// Tool names are prefixed with `mcp_{server_name}_` to avoid collisions.
    pub fn tool_definitions(&self) -> Vec<rig::completion::ToolDefinition> {
        let mut defs = Vec::new();

        for (server_name, conn) in &self.connections {
            for tool in &conn.tools {
                let prefixed_name = format!(
                    "mcp_{}_{}",
                    server_name.replace('-', "_"),
                    tool.name.replace('-', "_")
                );

                defs.push(rig::completion::ToolDefinition {
                    name: prefixed_name,
                    description: tool.description.clone(),
                    parameters: tool.input_schema.clone(),
                });
            }
        }

        defs
    }

    /// Call a tool on a connected MCP server.
    ///
    /// The `tool_name` should be the prefixed name (e.g., `mcp_github_search`).
    /// This method unprefixes it to find the server and original tool name.
    pub async fn call_tool(&self, tool_name: &str, arguments: serde_json::Value) -> Result<String> {
        // Parse the prefixed tool name: mcp_{server}_{tool}
        let rest = tool_name
            .strip_prefix("mcp_")
            .context("Tool name does not start with mcp_ prefix")?;

        // Find the matching server by trying each connection
        for (server_name, conn) in &self.connections {
            let server_prefix = format!("{}_", server_name.replace('-', "_"));
            if let Some(original_tool) = rest.strip_prefix(&server_prefix) {
                // Find the original tool name (may have had hyphens)
                let matching_tool = conn
                    .tools
                    .iter()
                    .find(|t| t.name.replace('-', "_") == original_tool)
                    .map(|t| t.name.as_str())
                    .unwrap_or(original_tool);

                let mut params = CallToolRequestParams::new(matching_tool.to_string());
                if let Some(obj) = arguments.as_object() {
                    params.arguments = Some(obj.clone());
                }

                let content = conn.service.call_tool(params).await.context(format!(
                    "MCP tool call '{}' on server '{}' failed",
                    matching_tool, server_name
                ))?;

                // Concatenate text content from the result
                let text = content
                    .iter()
                    .filter_map(|c| match &c.raw {
                        RawContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                return Ok(text);
            }
        }

        bail!("No MCP server found for tool '{}'", tool_name)
    }

    /// Check if a tool name belongs to a connected MCP server.
    pub fn is_mcp_tool(&self, tool_name: &str) -> bool {
        if let Some(rest) = tool_name.strip_prefix("mcp_") {
            self.connections.keys().any(|server_name| {
                let prefix = format!("{}_", server_name.replace('-', "_"));
                rest.starts_with(&prefix)
            })
        } else {
            false
        }
    }

    /// Gracefully shut down all MCP server connections.
    pub async fn shutdown(self) {
        for (name, conn) in self.connections {
            if let Err(e) = conn.service.cancel().await {
                tracing::warn!("Failed to shut down MCP server '{}': {:#}", name, e);
            }
        }
    }
}
