mod remote;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "stroem-api", version, about = "Strøm — remote server client")]
struct Cli {
    /// Server URL
    #[arg(long, env = "STROEM_URL", default_value = "http://localhost:8080")]
    server: String,

    /// Authentication token (API key or JWT). Can also be set via STROEM_TOKEN env var.
    #[arg(long, env = "STROEM_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: remote::Commands,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    remote::dispatch(cli.command, &cli.server, cli.token.as_deref()).await
}
