use crate::config::ServerConfig;
use crate::log_broadcast::LogBroadcast;
use crate::oidc::OidcProvider;
use crate::workspace::WorkspaceManager;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub workspaces: Arc<WorkspaceManager>,
    pub config: Arc<ServerConfig>,
    pub log_broadcast: Arc<LogBroadcast>,
    pub oidc_providers: Arc<HashMap<String, OidcProvider>>,
}

impl AppState {
    /// Create a new app state
    pub fn new(
        pool: PgPool,
        workspaces: WorkspaceManager,
        config: ServerConfig,
        oidc_providers: HashMap<String, OidcProvider>,
    ) -> Self {
        Self {
            pool,
            workspaces: Arc::new(workspaces),
            config: Arc::new(config),
            log_broadcast: Arc::new(LogBroadcast::new()),
            oidc_providers: Arc::new(oidc_providers),
        }
    }

    /// Get the workspace config for a given workspace name
    pub async fn get_workspace(&self, name: &str) -> Option<WorkspaceConfig> {
        self.workspaces.get_config_async(name).await
    }
}
