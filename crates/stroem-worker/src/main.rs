use anyhow::Result;
use clap::Parser;
use stroem_worker::config::load_config;
use stroem_worker::executor::StepExecutor;
use stroem_worker::poller::run_worker;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "stroem-worker", about = "Strøm workflow worker")]
struct Cli {
    /// Path to worker configuration file
    #[arg(
        short,
        long,
        env = "STROEM_CONFIG",
        default_value = "worker-config.yaml"
    )]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider before any TLS usage (kube client)
    #[cfg(feature = "kubernetes")]
    {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load configuration
    let cli = Cli::parse();
    let config = load_config(&cli.config)?;

    tracing::info!("Starting worker '{}'", config.worker_name);

    // Build executor with optional runners
    #[allow(unused_mut)]
    let mut executor = StepExecutor::new();

    if let Some(ref runner_image) = config.runner_image {
        tracing::info!("Default runner image: {}", runner_image);
        executor = executor.with_runner_image(runner_image.clone());
    }

    #[cfg(feature = "docker")]
    if let Some(ref docker_config) = config.docker {
        tracing::info!("Docker runner enabled");
        let docker_runner = if let Some(ref _host) = docker_config.host {
            stroem_runner::DockerRunner::new()?
        } else {
            stroem_runner::DockerRunner::new()?
        };
        executor = executor.with_docker_runner(docker_runner);
    }

    #[cfg(feature = "kubernetes")]
    if let Some(ref kube_config) = config.kubernetes {
        tracing::info!(
            "Kubernetes runner enabled (namespace: {})",
            kube_config.namespace
        );
        let mut kube_runner = stroem_runner::KubeRunner::new(
            kube_config.namespace.clone(),
            config.server_url.clone(),
            config.worker_token.clone(),
        );
        if let Some(ref init_image) = kube_config.init_image {
            kube_runner = kube_runner.with_init_image(init_image.clone());
        }
        if let Some(ref startup_cm) = kube_config.runner_startup_configmap {
            kube_runner = kube_runner.with_startup_configmap(startup_cm.clone());
        }
        executor = executor.with_kube_runner(kube_runner);
    }

    // Set up graceful shutdown
    let cancel_token = CancellationToken::new();
    let shutdown_token = cancel_token.clone();
    tokio::spawn(async move {
        shutdown_signal(shutdown_token).await;
    });

    // Run the worker
    run_worker(config, executor, cancel_token).await
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

    tracing::info!("Shutdown signal received, stopping worker...");
    cancel_token.cancel();
}
