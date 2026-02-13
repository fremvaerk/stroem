use anyhow::Result;
use stroem_worker::config::load_config;
use stroem_worker::executor::StepExecutor;
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
        executor = executor.with_kube_runner(kube_runner);
    }

    // Run the worker
    run_worker(config, executor).await
}
