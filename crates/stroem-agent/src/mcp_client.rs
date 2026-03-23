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

/// Concrete McpService implementation wrapping rmcp's RunningService.
struct RmcpService {
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
}

#[async_trait::async_trait]
impl McpService for RmcpService {
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
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("MCP shutdown failed: {}", e))
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
    /// Initialize an MCP service, discover its tools, and register the connection.
    async fn discover_and_register(
        connections: &mut HashMap<String, McpConnection>,
        name: &str,
        service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let tools_result =
            match tokio::time::timeout(timeout, service.list_tools(Default::default())).await {
                Ok(result) => {
                    result.context(format!("Failed to list tools from MCP server '{}'", name))?
                }
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
                let input_schema = serde_json::to_value(&*t.input_schema).unwrap_or_default();
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
                service: Box::new(RmcpService { service }),
                tools,
                name: name.to_string(),
            },
        );

        Ok(())
    }

    /// Connect to the given MCP servers and discover their tools.
    ///
    /// Supports `stdio` (child process) and `sse` (Streamable HTTP) transports.
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

                    // SECURITY: env_clear() prevents inheriting parent process secrets.
                    let mut cmd = tokio::process::Command::new(command);
                    cmd.env_clear();
                    cmd.args(&args);

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

                    Self::discover_and_register(&mut connections, name, service, timeout).await?;
                }
                "sse" => {
                    let url = server_def
                        .url
                        .as_deref()
                        .context(format!("MCP server '{}' missing url", name))?;

                    tracing::info!(server = name, url = url, "Connecting to MCP server via SSE");

                    let mut config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url);

                    if let Some(ref token) = server_def.auth_token {
                        config = config.auth_header(token);
                    }

                    let transport = rmcp::transport::StreamableHttpClientTransport::with_client(
                        reqwest::Client::default(),
                        config,
                    );

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

                    Self::discover_and_register(&mut connections, name, service, timeout).await?;
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
    pub async fn call_tool(&self, tool_name: &str, arguments: serde_json::Value) -> Result<String> {
        let rest = tool_name
            .strip_prefix("mcp_")
            .context("Tool name does not start with mcp_ prefix")?;

        for (server_name, conn) in &self.connections {
            let server_prefix = format!("{}_", server_name.replace('-', "_"));
            if let Some(original_tool) = rest.strip_prefix(&server_prefix) {
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

#[cfg(test)]
mod tests {
    use super::*;

    struct MockMcpService {
        response_text: String,
    }

    #[async_trait::async_trait]
    impl McpService for MockMcpService {
        async fn call_tool(&self, _params: CallToolRequestParams) -> Result<Vec<Content>> {
            use rmcp::model::RawTextContent;
            Ok(vec![Content {
                raw: RawContent::Text(RawTextContent {
                    text: self.response_text.clone(),
                    meta: None,
                }),
                annotations: None,
            }])
        }

        async fn cancel(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    fn make_tool(name: &str, description: &str) -> McpToolInfo {
        McpToolInfo {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn make_manager(connections: Vec<(&str, Vec<McpToolInfo>)>) -> McpClientManager {
        let mut map = HashMap::new();
        for (name, tools) in connections {
            map.insert(
                name.to_string(),
                McpConnection {
                    service: Box::new(MockMcpService {
                        response_text: format!("mock response from {}", name),
                    }),
                    tools,
                    name: name.to_string(),
                },
            );
        }
        McpClientManager { connections: map }
    }

    #[test]
    fn test_is_mcp_tool_matching_server() {
        let mgr = make_manager(vec![("github", vec![make_tool("search", "Search repos")])]);
        assert!(mgr.is_mcp_tool("mcp_github_search"));
    }

    #[test]
    fn test_is_mcp_tool_no_prefix() {
        let mgr = make_manager(vec![("github", vec![make_tool("search", "")])]);
        assert!(!mgr.is_mcp_tool("strom_task_deploy"));
        assert!(!mgr.is_mcp_tool("ask_user"));
    }

    #[test]
    fn test_is_mcp_tool_unknown_server() {
        let mgr = make_manager(vec![("github", vec![make_tool("search", "")])]);
        assert!(!mgr.is_mcp_tool("mcp_slack_post"));
    }

    #[test]
    fn test_tool_definitions_single_server() {
        let mgr = make_manager(vec![(
            "github",
            vec![
                make_tool("search", "Search repositories"),
                make_tool("create-issue", "Create an issue"),
            ],
        )]);
        let defs = mgr.tool_definitions();
        assert_eq!(defs.len(), 2);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"mcp_github_search"));
        assert!(names.contains(&"mcp_github_create_issue"));
    }

    #[test]
    fn test_tool_definitions_empty() {
        let mgr = make_manager(vec![]);
        assert!(mgr.tool_definitions().is_empty());
    }

    #[tokio::test]
    async fn test_call_tool_routes_correctly() {
        let mgr = make_manager(vec![
            ("github", vec![make_tool("search", "")]),
            ("slack", vec![make_tool("post", "")]),
        ]);
        let result = mgr
            .call_tool("mcp_github_search", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result, "mock response from github");
    }

    #[tokio::test]
    async fn test_call_tool_no_mcp_prefix() {
        let mgr = make_manager(vec![("github", vec![make_tool("search", "")])]);
        let result = mgr
            .call_tool("strom_task_deploy", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_call_tool_unknown_server() {
        let mgr = make_manager(vec![("github", vec![make_tool("search", "")])]);
        let result = mgr.call_tool("mcp_slack_post", serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
