mod commands;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// cella — Dev containers reinvented for the AI age
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: commands::Command,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    cli.command.execute().await
}
