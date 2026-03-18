use clap::Args;
use serde_json::json;
use tracing::info;

use super::up::OutputFormat;
use cella_docker::{ContainerState, DockerClient};

/// Stop the dev container for the current workspace.
#[derive(Args)]
pub struct DownArgs {
    /// Remove the container after stopping.
    #[arg(long)]
    rm: bool,

    /// Remove associated volumes (only with --rm).
    #[arg(long, requires = "rm")]
    volumes: bool,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

impl DownArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = std::env::current_dir()?;

        // Connect to Docker
        let client = match &self.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };

        // Find container
        let container = client
            .find_container(&cwd)
            .await?
            .ok_or_else(|| format!("no cella container found for workspace: {}", cwd.display()))?;

        // Stop if running
        if container.state == ContainerState::Running {
            client.stop_container(&container.id).await?;
            info!("Container stopped");
        } else {
            info!("Container already stopped");
        }

        // Remove if requested
        if self.rm {
            client.remove_container(&container.id, self.volumes).await?;

            match self.output {
                OutputFormat::Text => {
                    eprintln!("Container stopped and removed.");
                }
                OutputFormat::Json => {
                    let output = json!({
                        "outcome": "removed",
                        "containerId": container.id,
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&output).unwrap_or_default()
                    );
                }
            }
        } else {
            match self.output {
                OutputFormat::Text => {
                    eprintln!("Container stopped.");
                }
                OutputFormat::Json => {
                    let output = json!({
                        "outcome": "stopped",
                        "containerId": container.id,
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&output).unwrap_or_default()
                    );
                }
            }
        }

        Ok(())
    }
}
