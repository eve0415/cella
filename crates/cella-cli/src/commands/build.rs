use std::path::PathBuf;

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::{info, warn};

use cella_config::resolve;
use cella_docker::DockerClient;

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
    pub const fn is_text_output(&self) -> bool {
        matches!(self.output, OutputFormat::Text)
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;

        // 1. Resolve config
        info!("Resolving devcontainer config...");
        let resolved = resolve::config(&cwd, self.file.as_deref())?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());

        // 2. Connect to Docker
        let client = super::connect_docker(self.docker_host.as_deref())?;
        client.ping().await?;

        // Docker Compose: build all services + feature-enriched primary image
        if config.get("dockerComposeFile").is_some() {
            return execute_compose_build(
                config,
                &resolved,
                config_name,
                &client,
                self.no_cache,
                &self.output,
                &progress,
            )
            .await;
        }

        // 3. Build image via shared ensure_image logic
        let (img_name, _resolved_features, _image_details) = ensure_image(
            &client,
            config,
            &resolved.workspace_root,
            config_name,
            &resolved.config_path,
            self.no_cache,
            &progress,
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

/// Execute the Docker Compose build path: build feature image, write override, compose build.
async fn execute_compose_build(
    config: &serde_json::Value,
    resolved: &resolve::ResolvedConfig,
    config_name: Option<&str>,
    client: &DockerClient,
    no_cache: bool,
    output: &OutputFormat,
    progress: &crate::progress::Progress,
) -> Result<(), Box<dyn std::error::Error>> {
    let project = cella_compose::ComposeProject::from_resolved(
        config,
        &resolved.config_path,
        &resolved.workspace_root,
    )?;

    // Build feature-enriched image if features configured
    let image_override = if config
        .get("features")
        .is_some_and(|v| v.is_object() && !v.as_object().unwrap().is_empty())
    {
        let (img_name, _, _) = ensure_image(
            client,
            config,
            &resolved.workspace_root,
            config_name,
            &resolved.config_path,
            no_cache,
            progress,
        )
        .await?;
        Some(img_name)
    } else {
        None
    };

    // Write override file (needed for image swap)
    if image_override.is_some() {
        let (agent_vol_name, agent_vol_target, _) = cella_docker::volume::agent_volume_mount();
        let override_config = cella_compose::OverrideConfig {
            primary_service: project.primary_service.clone(),
            image_override: image_override.clone(),
            override_command: project.override_command,
            agent_volume_name: agent_vol_name.to_string(),
            agent_volume_target: agent_vol_target.to_string(),
            extra_env: Vec::new(),
            extra_labels: std::collections::BTreeMap::new(),
        };
        let yaml = cella_compose::override_file::generate_override_yaml(&override_config);
        cella_compose::override_file::write_override_file(&project.override_file, &yaml)?;
    }

    // Run docker compose build
    let compose_cmd = cella_compose::ComposeCommand::new(&project);
    compose_cmd
        .build(None)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!("docker compose build failed: {e}").into()
        })?;

    let img_name = image_override.unwrap_or_else(|| "(compose)".to_string());
    match output {
        OutputFormat::Text => {
            eprintln!("Compose services built. Primary image: {img_name}");
        }
        OutputFormat::Json => {
            let result = json!({
                "outcome": "built",
                "imageName": img_name,
                "compose": true,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
        }
    }
    Ok(())
}
