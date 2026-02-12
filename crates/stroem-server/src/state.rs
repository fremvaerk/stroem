use crate::config::ServerConfig;
use crate::log_broadcast::LogBroadcast;
use sqlx::PgPool;
use std::sync::Arc;
use stroem_common::models::workflow::WorkspaceConfig;
use tokio::sync::RwLock;

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub workspace: Arc<RwLock<WorkspaceConfig>>,
    pub config: Arc<ServerConfig>,
    pub log_broadcast: Arc<LogBroadcast>,
}

impl AppState {
    /// Create a new app state
    pub fn new(pool: PgPool, workspace: WorkspaceConfig, config: ServerConfig) -> Self {
        Self {
            pool,
            workspace: Arc::new(RwLock::new(workspace)),
            config: Arc::new(config),
            log_broadcast: Arc::new(LogBroadcast::new()),
        }
    }
}
