use super::auth::McpAuthContext;
use crate::state::AppState;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo, ToolsCapability};
use rmcp::{tool_handler, ServerHandler};
use std::sync::Arc;

/// MCP server handler for Strøm.
///
/// Each instance is created per-request (stateless mode) and holds
/// a reference to the shared application state plus the authenticated
/// user context (if auth is configured).
pub struct StromMcpHandler {
    pub(crate) state: Arc<AppState>,
    pub(crate) auth: Option<McpAuthContext>,
    tool_router: ToolRouter<Self>,
}

impl StromMcpHandler {
    pub fn new(state: Arc<AppState>, auth: Option<McpAuthContext>) -> Self {
        Self {
            state,
            auth,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_handler]
impl ServerHandler for StromMcpHandler {
    fn get_info(&self) -> ServerInfo {
        let mut caps = ServerCapabilities::default();
        caps.tools = Some(ToolsCapability::default());

        let mut impl_info = Implementation::default();
        impl_info.name = "stroem".into();
        impl_info.version = env!("CARGO_PKG_VERSION").into();

        let mut info = ServerInfo::default();
        info.capabilities = caps;
        info.server_info = impl_info;
        info.instructions = Some(
            "Strøm workflow orchestration server. Use tools to list tasks, \
             execute workflows, check job status, and read logs."
                .into(),
        );
        info
    }
}
