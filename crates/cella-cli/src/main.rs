mod commands;
pub mod progress;

use std::io::IsTerminal;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use progress::{IndicatifMakeWriter, Progress, Verbosity};

/// cella — Dev containers reinvented for the AI age
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Show expanded step details (container names, feature resolution, etc.).
    #[arg(short, long, global = true)]
    verbose: bool,

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

    let verbosity = if cli.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    };

    // Spinners are active when: text output mode AND no RUST_LOG AND stderr is a TTY.
    let rust_log_set = std::env::var_os("RUST_LOG").is_some();
    let is_tty = std::io::stderr().is_terminal();
    let spinners_enabled = cli.command.is_text_output() && !rust_log_set && is_tty;

    let progress = Progress::new(spinners_enabled, verbosity);

    // The daemon subprocess initializes its own file-based tracing.
    // Skip the normal indicatif-based tracing for daemon start.
    if !cli.command.is_daemon_start() {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_writer(IndicatifMakeWriter::new(progress.multi().clone()))
            .init();
    }

    cli.command.execute(progress).await
}
