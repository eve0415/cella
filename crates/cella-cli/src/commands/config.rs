use std::path::PathBuf;

use clap::{Args, Subcommand};

/// View and manage cella configuration.
#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show the resolved configuration.
    Show,
    /// Edit global cella settings.
    Global,
    /// Manage dotfile symlinks.
    Dotfiles,
    /// Configure agent presets.
    Agent,
    /// Validate a devcontainer.json file.
    Validate {
        /// Path to the devcontainer.json file. Defaults to .devcontainer/devcontainer.json.
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
}

impl ConfigArgs {
    pub async fn execute(self, strict: bool) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            ConfigCommand::Validate { file } => validate(file, strict),
            ConfigCommand::Show
            | ConfigCommand::Global
            | ConfigCommand::Dotfiles
            | ConfigCommand::Agent => {
                eprintln!("cella config: not yet implemented");
                Err("not yet implemented".into())
            }
        }
    }
}

fn validate(file: Option<PathBuf>, strict: bool) -> Result<(), Box<dyn std::error::Error>> {
    let path = file.unwrap_or_else(|| PathBuf::from(".devcontainer/devcontainer.json"));

    let raw_text = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

    let source_name = path.display().to_string();

    match cella_config::parse::parse_devcontainer(&source_name, &raw_text, strict) {
        Ok((_config, warnings)) => {
            if warnings.is_empty() {
                eprintln!("✓ {source_name} is valid");
            } else {
                for w in &warnings {
                    eprintln!("warning: {} ({})", w.message, w.path);
                }
                eprintln!("✓ {} is valid ({} warning(s))", source_name, warnings.len());
            }
            Ok(())
        }
        Err(diagnostics) => {
            eprint!("{}", diagnostics.render());
            Err(format!("validation failed: {} error(s)", diagnostics.error_count()).into())
        }
    }
}
