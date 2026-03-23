mod commands;
pub mod progress;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use progress::{IndicatifMakeWriter, Progress};

/// cella — Dev containers reinvented for the AI age
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: commands::Command,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install miette's graphical error handler for pretty diagnostics
    miette::set_hook(Box::new(|_| {
        Box::new(miette::GraphicalReportHandler::new_themed(
            miette::GraphicalTheme::unicode(),
        ))
    }))
    .ok();

    // Parse CLI first to determine output mode before creating progress.
    let cli = Cli::parse();
    let progress = Progress::new(cli.command.is_text_output());

    // Route tracing through indicatif so log lines never corrupt spinners.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(IndicatifMakeWriter::new(progress.multi().clone()))
        .init();

    cli.command.execute(progress).await
}
