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
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

fn validate(
    file: Option<PathBuf>,
    strict: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = if let Some(p) = file {
        p
    } else {
        let cwd = std::env::current_dir()?;
        cella_config::devcontainer::discover::config(&cwd)?
    };

    let raw_text = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let source_name = path.display().to_string();

    match cella_config::devcontainer::parse::devcontainer(&source_name, &raw_text, strict) {
        Ok((_config, warnings)) => {
            if warnings.is_empty() {
                eprintln!("✓ {source_name} is valid");
            } else {
                for w in &warnings {
                    eprintln!("warning: {} ({})", w.message, w.path);
                }
                eprintln!("✓ {source_name} is valid ({} warning(s))", warnings.len());
            }
            Ok(())
        }
        Err(diagnostics) => {
            eprint!("{}", diagnostics.render());

            if diagnostics.has_errors() {
                Err(format!("validation failed: {} error(s)", diagnostics.error_count()).into())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cella-config-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn validate_accepts_minimal_devcontainer_file() {
        let path = temp_config(
            r#"{
                "image": "mcr.microsoft.com/devcontainers/base:ubuntu"
            }"#,
        );

        let result = validate(Some(path.clone()), false);

        let _ = std::fs::remove_file(path);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_reports_missing_file_path() {
        let path = std::env::temp_dir().join("cella-config-test-missing-devcontainer.json");

        let err = validate(Some(path.clone()), false).expect_err("missing file should fail");

        assert!(err.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn validate_rejects_invalid_json() {
        let path = temp_config("{ invalid json");

        let err = validate(Some(path.clone()), false).expect_err("invalid json should fail");

        let _ = std::fs::remove_file(path);
        assert!(err.to_string().contains("validation failed"));
    }

    #[test]
    fn unimplemented_config_commands_return_error() {
        for command in [
            ConfigCommand::Show,
            ConfigCommand::Global,
            ConfigCommand::Dotfiles,
            ConfigCommand::Agent,
        ] {
            let result = ConfigArgs { command }.execute();
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().to_string(), "not yet implemented");
        }
    }
}
