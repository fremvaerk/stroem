use anyhow::{Context, Result};
use stroem_db::{create_pool, run_migrations, UserRepo};
use stroem_server::auth::hash_password;
use stroem_server::config::ServerConfig;
use stroem_server::state::AppState;
use stroem_server::workspace::WorkspaceManager;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Starting Strøm server");

    // Load configuration
    let config_path =
        std::env::var("STROEM_CONFIG").unwrap_or_else(|_| "server-config.yaml".to_string());

    tracing::info!("Loading config from: {}", config_path);

    let config_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path))?;

    let config: ServerConfig = serde_yml::from_str(&config_content)
        .with_context(|| format!("Failed to parse config file: {}", config_path))?;

    tracing::info!("Config loaded successfully");

    // Create database pool
    tracing::info!("Connecting to database...");
    let pool = create_pool(&config.db.url)
        .await
        .context("Failed to create database pool")?;

    // Run migrations
    tracing::info!("Running database migrations...");
    run_migrations(&pool)
        .await
        .context("Failed to run migrations")?;

    // Seed initial user if configured
    if let Some(auth_config) = &config.auth {
        if let Some(initial_user) = &auth_config.initial_user {
            match UserRepo::get_by_email(&pool, &initial_user.email).await {
                Ok(Some(_)) => {
                    tracing::info!(
                        "Initial user '{}' already exists, skipping seed",
                        initial_user.email
                    );
                }
                Ok(None) => {
                    let password_hash = hash_password(&initial_user.password)
                        .context("Failed to hash initial user password")?;
                    UserRepo::create(
                        &pool,
                        uuid::Uuid::new_v4(),
                        &initial_user.email,
                        Some(&password_hash),
                        None,
                    )
                    .await
                    .context("Failed to create initial user")?;
                    tracing::info!("Created initial user: {}", initial_user.email);
                }
                Err(e) => {
                    tracing::warn!("Failed to check for initial user: {}", e);
                }
            }
        }
    }

    // Load workspaces
    tracing::info!("Loading {} workspace(s)...", config.workspaces.len());
    let workspace_manager = WorkspaceManager::new(config.workspaces.clone())
        .await
        .context("Failed to load workspaces")?;

    for info in workspace_manager.list_workspace_info().await {
        tracing::info!(
            "Workspace '{}': {} actions, {} tasks",
            info.name,
            info.actions_count,
            info.tasks_count
        );
    }

    // Start background watchers for hot-reload
    workspace_manager.start_watchers();

    // Build application state
    let state = AppState::new(pool, workspace_manager, config.clone());

    // Build router
    let app = stroem_server::web::build_router(state);

    // Start server
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("Failed to bind to {}", config.listen))?;

    tracing::info!("Server listening on {}", config.listen);

    axum::serve(listener, app).await.context("Server error")?;

    Ok(())
}
