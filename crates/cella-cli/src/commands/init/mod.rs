mod noninteractive;
mod summary;
mod wizard;

use std::path::PathBuf;

use clap::{Args, ValueEnum};

use crate::progress::Progress;
use crate::style;

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
    pub async fn execute(
        self,
        progress: Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.template.is_some() {
            noninteractive::run(self).await
        } else {
            wizard::run(self, progress).await
        }
    }
}

/// Verify that the generated devcontainer.json can be parsed by our config
/// parser. Logs a warning on parse failure but does not abort — the template
/// may use fields that our schema does not yet recognize.
fn verify_generated_config(config_path: &std::path::Path) {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return;
    };
    let source_name = config_path.display().to_string();
    if let Err(diags) =
        cella_config::devcontainer::parse::devcontainer(&source_name, &content, false)
    {
        for d in diags.diagnostics() {
            if d.severity == cella_config::devcontainer::diagnostic::Severity::Error {
                eprintln!(
                    "  {} generated config warning: {}",
                    style::dim("(note)"),
                    d.message,
                );
            }
        }
    }
}
