use anyhow::Result;
use stroem_worker::config::load_config;
use stroem_worker::poller::run_worker;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load configuration
    let config_path =
        std::env::var("STROEM_CONFIG").unwrap_or_else(|_| "worker-config.yaml".to_string());
    let config = load_config(&config_path)?;

    tracing::info!("Starting worker '{}'", config.worker_name);

    // Run the worker
    run_worker(config).await
}
