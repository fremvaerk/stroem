use crate::config::ServerConfig;
use crate::log_broadcast::LogBroadcast;
use crate::log_storage::LogStorage;
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
    pub log_storage: Arc<LogStorage>,
    pub oidc_providers: Arc<HashMap<String, OidcProvider>>,
}

impl AppState {
    /// Create a new app state
    pub fn new(
        pool: PgPool,
        workspaces: WorkspaceManager,
        config: ServerConfig,
        log_storage: LogStorage,
        oidc_providers: HashMap<String, OidcProvider>,
    ) -> Self {
        Self {
            pool,
            workspaces: Arc::new(workspaces),
            config: Arc::new(config),
            log_broadcast: Arc::new(LogBroadcast::new()),
            log_storage: Arc::new(log_storage),
            oidc_providers: Arc::new(oidc_providers),
        }
    }

    /// Get the workspace config for a given workspace name
    pub async fn get_workspace(&self, name: &str) -> Option<WorkspaceConfig> {
        self.workspaces.get_config_async(name).await
    }
}
