use std::path::PathBuf;

use clap::Args;
use serde_json::json;
use tracing::info;

use super::tunnel::tunnel_socket_path;
use super::up::OutputFormat;
use cella_docker::{ContainerState, ContainerTarget, DockerClient};

/// Stop the dev container for the current workspace.
#[derive(Args)]
pub struct DownArgs {
    /// Remove the container after stopping.
    #[arg(long)]
    rm: bool,

    /// Remove associated volumes (only with --rm).
    #[arg(long, requires = "rm")]
    volumes: bool,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Target container by ID.
    #[arg(long)]
    container_id: Option<String>,

    /// Target container by name.
    #[arg(long)]
    container_name: Option<String>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

impl DownArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let client = match &self.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };

        let target = ContainerTarget {
            container_id: self.container_id,
            container_name: self.container_name,
            id_label: None,
            workspace_folder: self.workspace_folder,
        };

        let container = target.resolve(&client, false).await?;

        // Best-effort tunnel disconnect
        if let Some(socket_path) = tunnel_socket_path()
            && let Err(e) = cella_tunnel::client::disconnect_container(&socket_path, &container.id)
        {
            tracing::debug!("Tunnel disconnect (best-effort): {e}");
        }

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
