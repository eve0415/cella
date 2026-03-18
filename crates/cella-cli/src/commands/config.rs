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
        /// Path to the devcontainer.json file. Auto-discovered if not specified.
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Treat warnings (e.g. unknown fields) as errors.
        #[arg(long)]
        strict: bool,
    },
}

impl ConfigArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            ConfigCommand::Validate { file, strict } => validate(file, strict),
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
    let path = if let Some(p) = file {
        p
    } else {
        let cwd = std::env::current_dir()?;
        cella_config::discover::discover_config(&cwd)?
    };

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
                eprintln!(
                    "✓ {source_name} is valid ({} warning(s))",
                    warnings.len()
                );
            }
            Ok(())
        }
        Err(diagnostics) => {
            eprint!("{}", diagnostics.render());

            if diagnostics.has_errors() {
                Err(format!(
                    "validation failed: {} error(s)",
                    diagnostics.error_count()
                )
                .into())
            } else if strict {
                Err(format!(
                    "strict mode: {} warning(s) treated as errors",
                    diagnostics.warning_count()
                )
                .into())
            } else {
                // Warnings only, non-strict → success
                eprintln!(
                    "✓ {source_name} is valid ({} warning(s))",
                    diagnostics.warning_count()
                );
                Ok(())
            }
        }
    }
}
