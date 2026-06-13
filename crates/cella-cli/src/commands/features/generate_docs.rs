//! `cella features generate-docs` — generate README.md for each feature in a collection.

use std::path::PathBuf;

use clap::Args;

use cella_features::docs::{GenerateDocsInput, generate_docs};

use crate::commands::LogLevel;

/// Generate documentation (`README.md`) for every feature in a collection.
///
/// Matches the output format of the official `@devcontainers/cli` `features
/// generate-docs` command: same section headers, options table columns, notes
/// file inclusion, and footer note.
#[derive(Args)]
pub struct GenerateDocsArgs {
    /// Path to the folder whose subdirectories contain feature manifests
    /// (`devcontainer-feature.json`).
    #[arg(short = 'p', long, default_value = ".")]
    pub project_folder: PathBuf,

    /// OCI registry name.
    #[arg(short = 'r', long, default_value = "ghcr.io")]
    pub registry: String,

    /// Unique identifier for the feature collection (e.g. `owner/repo`).
    #[arg(short = 'n', long)]
    pub namespace: String,

    /// GitHub owner for the footer link.
    #[arg(long, default_value = "")]
    pub github_owner: String,

    /// GitHub repo for the footer link.
    #[arg(long, default_value = "")]
    pub github_repo: String,

    /// Log verbosity level.
    #[arg(long, default_value = "info")]
    pub log_level: LogLevel,
}

impl GenerateDocsArgs {
    /// Execute the generate-docs command.
    ///
    /// # Errors
    ///
    /// Returns an error when `--project-folder` cannot be read.
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let input = GenerateDocsInput {
            project_folder: &self.project_folder,
            registry: &self.registry,
            namespace: &self.namespace,
            github_owner: &self.github_owner,
            github_repo: &self.github_repo,
        };

        let results = generate_docs(&input)?;

        for r in &results {
            tracing::info!("Generated: {} ({})", r.readme_path.display(), r.feature_id);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// Thin `Parser` wrapper so we can call `try_parse_from` on `GenerateDocsArgs`,
    /// which only derives `Args`.
    #[derive(Parser)]
    struct Cmd {
        #[command(flatten)]
        inner: GenerateDocsArgs,
    }

    fn parse(args: &[&str]) -> Result<GenerateDocsArgs, clap::Error> {
        Cmd::try_parse_from(args).map(|c| c.inner)
    }

    #[test]
    fn generate_docs_log_level_default_is_info() {
        let gd = parse(&["cmd", "--namespace", "owner/repo"]).expect("parse failed");
        assert!(
            matches!(gd.log_level, LogLevel::Info),
            "default log-level should be Info"
        );
    }

    #[test]
    fn generate_docs_log_level_accepts_debug() {
        let gd = parse(&["cmd", "--namespace", "owner/repo", "--log-level", "debug"])
            .expect("parse failed");
        assert!(
            matches!(gd.log_level, LogLevel::Debug),
            "should parse --log-level debug"
        );
    }

    #[test]
    fn generate_docs_log_level_accepts_trace() {
        let gd = parse(&["cmd", "--namespace", "owner/repo", "--log-level", "trace"])
            .expect("parse failed");
        assert!(
            matches!(gd.log_level, LogLevel::Trace),
            "should parse --log-level trace"
        );
    }

    #[test]
    fn generate_docs_rejects_invalid_log_level() {
        let result = parse(&["cmd", "--namespace", "owner/repo", "--log-level", "verbose"]);
        assert!(result.is_err(), "invalid log-level should fail to parse");
    }

    #[test]
    fn generate_docs_args_has_log_level_flag() {
        use clap::CommandFactory;
        let cmd = Cmd::command();
        let has_log_level = cmd
            .get_arguments()
            .any(|a| a.get_long() == Some("log-level"));
        assert!(has_log_level, "generate-docs must expose --log-level");
    }
}
