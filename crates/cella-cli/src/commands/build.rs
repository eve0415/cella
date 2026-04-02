use std::path::PathBuf;

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::{info, warn};

use cella_config::devcontainer::resolve;

use super::image::ensure_image;

/// Build the dev container image without starting it.
#[derive(Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub verbose: super::VerboseArgs,

    /// Do not use cache when building the image.
    #[arg(long)]
    no_cache: bool,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    file: Option<PathBuf>,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

/// Output format for build command.
#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

impl BuildArgs {
    pub const fn is_text_output(&self) -> bool {
        matches!(self.output, OutputFormat::Text)
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;

        info!("Resolving devcontainer config...");
        let resolved = resolve::config(&cwd, self.file.as_deref())?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());

        let client = self.backend.resolve_client().await?;
        client.ping().await?;

        // Docker Compose path: delegate to orchestrator
        if config.get("dockerComposeFile").is_some() {
            let (sender, renderer) = crate::progress::bridge(&progress);
            let result = cella_orchestrator::compose_build::compose_build(
                client.as_ref(),
                config,
                &resolved.config_path,
                &resolved.workspace_root,
                &sender,
            )
            .await
            .map_err(|e| e.to_string());
            drop(sender);
            let _ = renderer.await;
            let result = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            match self.output {
                OutputFormat::Text => {
                    eprintln!(
                        "Compose services built. Primary image: {}",
                        result.image_name
                    );
                }
                OutputFormat::Json => {
                    let output = json!({
                        "outcome": "built",
                        "imageName": result.image_name,
                        "compose": true,
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&output).unwrap_or_default()
                    );
                }
            }
            return Ok(());
        }

        // Non-compose path
        let (img_name, _resolved_features, _image_details) = ensure_image(
            client.as_ref(),
            config,
            &resolved.workspace_root,
            config_name,
            &resolved.config_path,
            self.no_cache,
            &progress,
        )
        .await?;

        if let Some(container) = client.find_container(&resolved.workspace_root).await?
            && let Some(old_hash) = &container.config_hash
            && *old_hash != resolved.config_hash
        {
            eprintln!(
                "\x1b[33mWARNING:\x1b[0m Config has changed since this container was created."
            );
            eprintln!("  Run `cella up --rebuild` to recreate with the updated config.");
        }

        match self.output {
            OutputFormat::Text => {
                eprintln!("Image built: {img_name}");
            }
            OutputFormat::Json => {
                let output = json!({
                    "outcome": "built",
                    "imageName": img_name,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&output).unwrap_or_default()
                );
            }
        }

        Ok(())
    }
}
