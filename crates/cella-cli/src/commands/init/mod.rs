mod noninteractive;
mod summary;
mod wizard;

use std::path::PathBuf;

use clap::{Args, ValueEnum};

use crate::progress::Progress;

/// Output format for generated devcontainer configuration.
#[derive(Clone, ValueEnum)]
pub enum ConfigFormat {
    /// JSONC with section comments (default).
    Jsonc,
    /// Plain JSON.
    Json,
}

impl ConfigFormat {
    pub const fn to_template_format(&self) -> cella_templates::types::OutputFormat {
        match self {
            Self::Jsonc => cella_templates::types::OutputFormat::Jsonc,
            Self::Json => cella_templates::types::OutputFormat::Json,
        }
    }
}

/// Initialize a devcontainer configuration for the current workspace.
#[derive(Args)]
pub struct InitArgs {
    /// Template OCI reference (e.g. ghcr.io/devcontainers/templates/rust:latest).
    /// When provided, runs in non-interactive mode.
    #[arg(long)]
    pub template: Option<String>,

    /// Workspace folder path (defaults to current directory).
    #[arg(short = 'w', long)]
    pub workspace_folder: Option<PathBuf>,

    /// Template option: KEY=VALUE (repeatable).
    #[arg(long = "template-option", value_name = "KEY=VALUE")]
    pub template_options: Vec<String>,

    /// Feature OCI reference to include (repeatable).
    #[arg(long, value_name = "OCI_REF")]
    pub feature: Vec<String>,

    /// Feature option: `FEATURE_ID=KEY=VALUE` (repeatable).
    #[arg(long, value_name = "FEATURE_ID=KEY=VALUE")]
    pub option: Vec<String>,

    /// Output format for generated devcontainer configuration (non-interactive).
    #[arg(long, value_enum, default_value = "jsonc")]
    pub output_format: ConfigFormat,

    /// Overwrite existing configuration without prompting.
    #[arg(long)]
    pub force: bool,

    /// OCI registry for template/feature collection
    /// (default: ghcr.io/devcontainers/templates).
    #[arg(long)]
    pub registry: Option<String>,

    /// Force re-fetch collection index (ignore cache).
    #[arg(long)]
    pub refresh: bool,

    /// Start dev container after generating config.
    #[arg(long)]
    pub up: bool,
}

impl InitArgs {
    pub async fn execute(self, progress: Progress) -> Result<(), Box<dyn std::error::Error>> {
        if self.template.is_some() {
            noninteractive::run(self).await
        } else {
            wizard::run(self, progress).await
        }
    }
}
