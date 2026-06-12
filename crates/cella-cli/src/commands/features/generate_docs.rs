//! `cella features generate-docs` — generate README.md for each feature in a collection.

use std::path::PathBuf;

use clap::Args;

use cella_features::docs::{GenerateDocsInput, generate_docs};

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
            eprintln!("Generated: {} ({})", r.readme_path.display(), r.feature_id);
        }

        Ok(())
    }
}
