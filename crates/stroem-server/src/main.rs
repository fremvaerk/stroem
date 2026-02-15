use anyhow::{Context, Result};
use stroem_db::{create_pool, run_migrations, UserRepo};
use stroem_server::auth::hash_password;
use stroem_server::config::ServerConfig;
use stroem_server::log_storage::LogStorage;
use stroem_server::state::AppState;
use stroem_server::workspace::WorkspaceManager;
use tokio_util::sync::CancellationToken;

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

    // Initialize OIDC providers
    let oidc_providers = if let Some(auth) = &config.auth {
        if auth.providers.values().any(|p| p.provider_type == "oidc") {
            let base_url = auth
                .base_url
                .as_ref()
                .context("base_url required when OIDC providers are configured")?;
            stroem_server::oidc::init_providers(&auth.providers, base_url).await?
        } else {
            std::collections::HashMap::new()
        }
    } else {
        std::collections::HashMap::new()
    };

    // Initialize log storage
    let log_storage = LogStorage::new(&config.log_storage.local_dir);

    #[cfg(feature = "s3")]
    let log_storage = if let Some(ref s3_config) = config.log_storage.s3 {
        log_storage
            .with_s3(s3_config)
            .await
            .context("Failed to initialize S3 log backend")?
    } else {
        log_storage
    };

    // Build application state
    let state = AppState::new(
        pool,
        workspace_manager,
        config.clone(),
        log_storage,
        oidc_providers,
    );

    // Start background tasks
    let cancel_token = CancellationToken::new();
    let _scheduler = stroem_server::scheduler::start(
        state.pool.clone(),
        state.workspaces.clone(),
        cancel_token.clone(),
    );
    let _recovery = stroem_server::recovery::start(state.clone(), cancel_token.clone());

    // Build router
    let app = stroem_server::web::build_router(state);

    // Start server with graceful shutdown
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("Failed to bind to {}", config.listen))?;

    tracing::info!("Server listening on {}", config.listen);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(cancel_token))
        .await
        .context("Server error")?;

    Ok(())
}

async fn shutdown_signal(cancel_token: CancellationToken) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received, stopping...");
    cancel_token.cancel();
}
