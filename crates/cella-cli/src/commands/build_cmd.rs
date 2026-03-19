use std::path::PathBuf;

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::{info, warn};

use cella_config::resolve::resolve_config;
use cella_docker::DockerClient;

use super::image::ensure_image;

/// Build the dev container image without starting it.
#[derive(Args)]
pub struct BuildArgs {
    /// Do not use cache when building the image.
    #[arg(long)]
    no_cache: bool,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    file: Option<PathBuf>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

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
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = if let Some(ref wf) = self.workspace_folder {
            wf.canonicalize().unwrap_or_else(|_| wf.clone())
        } else {
            std::env::current_dir()?
        };

        // 1. Resolve config
        info!("Resolving devcontainer config...");
        let resolved = resolve_config(&cwd, self.file.as_deref())?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());

        // 2. Connect to Docker
        let client = match &self.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };
        client.ping().await?;

        // 3. Build image via shared ensure_image logic
        let is_text = matches!(self.output, OutputFormat::Text);
        let (img_name, _resolved_features) = ensure_image(
            &client,
            config,
            &resolved.workspace_root,
            config_name,
            &resolved.config_path,
            self.no_cache,
            is_text,
        )
        .await?;

        // 4. Check if a container exists for this workspace with stale config
        if let Some(container) = client.find_container(&resolved.workspace_root).await?
            && let Some(old_hash) = &container.config_hash
            && *old_hash != resolved.config_hash
        {
            eprintln!(
                "\x1b[33mWARNING:\x1b[0m Config has changed since this container was created."
            );
            eprintln!("  Run `cella up --rebuild` to recreate with the updated config.");
        }

        // 5. Output result
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
