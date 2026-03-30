use std::path::PathBuf;

use clap::Args;
use serde_json::json;
use tracing::{debug, info};

use super::up::OutputFormat;
use cella_compose::discovery;
use cella_credential_proxy::daemon::stop_daemon;
use cella_daemon::shared::running_cella_container_count;
use cella_docker::{ContainerInfo, ContainerState, ContainerTarget};
use cella_env::git_credential::{
    credential_proxy_pid_path, credential_proxy_port_path, credential_proxy_socket_path,
    daemon_management_socket_path,
};

/// Stop the dev container for the current workspace.
#[derive(Args)]
pub struct DownArgs {
    #[command(flatten)]
    pub verbose: super::VerboseArgs,

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
    #[arg(long, conflicts_with = "branch")]
    container_id: Option<String>,

    /// Target container by name.
    #[arg(long, conflicts_with = "branch")]
    container_name: Option<String>,

    /// Target a worktree branch's container by branch name.
    #[arg(long)]
    branch: Option<String>,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// Force stop even when shutdownAction is "none".
    #[arg(long)]
    force: bool,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

/// Resolve a branch name to its worktree path.
fn resolve_branch_to_path(branch_name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let repo_info = cella_git::discover(&cwd)?;
    let worktrees = cella_git::list(&repo_info.root)?;
    let wt = worktrees
        .iter()
        .find(|wt| wt.branch.as_deref() == Some(branch_name))
        .ok_or_else(|| format!("No worktree found for branch '{branch_name}'"))?;
    Ok(wt.path.clone())
}

/// Remove the worktree directory and clean up stale git records.
fn remove_worktree(branch_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let repo_info = cella_git::discover(&cwd)?;
    let worktrees = cella_git::list(&repo_info.root)?;
    if let Some(wt) = worktrees
        .iter()
        .find(|wt| wt.branch.as_deref() == Some(branch_name))
    {
        cella_git::remove(&repo_info.root, &wt.path)?;
        let _ = std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&repo_info.root)
            .output();
        info!(branch = %branch_name, "removed worktree");
    }
    Ok(())
}

impl DownArgs {
    pub const fn is_text_output(&self) -> bool {
        matches!(self.output, OutputFormat::Text)
    }

    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let client = super::connect_docker(self.docker_host.as_deref())?;

        let workspace_folder = if let Some(ref branch_name) = self.branch {
            Some(resolve_branch_to_path(branch_name)?)
        } else {
            self.workspace_folder
        };

        let target = ContainerTarget {
            container_id: self.container_id,
            container_name: self.container_name,
            id_label: None,
            workspace_folder,
        };

        let container = target.resolve(&client, false).await?;

        // For non-compose containers, honour shutdownAction from label
        if !discovery::is_compose_container(&container.labels) {
            let shutdown_action = container
                .labels
                .get("dev.cella.shutdown_action")
                .map_or("stopContainer", String::as_str);

            if shutdown_action == "none" && !self.force {
                match &self.output {
                    OutputFormat::Text => {
                        eprintln!("Container has shutdownAction=\"none\". Use --force to stop it.");
                    }
                    OutputFormat::Json => {
                        let output = json!({
                            "outcome": "refused",
                            "reason": "shutdownAction is none",
                            "containerId": container.id,
                        });
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&output).unwrap_or_default()
                        );
                    }
                }
                return Ok(());
            }
        }

        deregister_container(&container).await;

        // Docker Compose branch: use `docker compose down` for compose containers
        if discovery::is_compose_container(&container.labels)
            && let Some(project_name) = discovery::compose_project_from_labels(&container.labels)
        {
            info!("Stopping compose project: {project_name}");
            let compose_cmd = cella_compose::ComposeCommand::from_project_name(project_name);
            compose_cmd
                .down()
                .await
                .map_err(|e| -> Box<dyn std::error::Error> {
                    format!("docker compose down failed: {e}").into()
                })?;

            // Clean up override file
            if let Some(data_dir) = cella_env::git_credential::cella_data_dir() {
                let override_path = data_dir
                    .join("compose")
                    .join(project_name)
                    .join("docker-compose.cella.yml");
                cella_compose::override_file::cleanup_override_file(&override_path);
            }

            print_outcome(&self.output, "stopped", &container.id);
            cleanup_credential_proxy();
            return Ok(());
        }

        if container.state == ContainerState::Running {
            client.stop_container(&container.id).await?;
            info!("Container stopped");
        } else {
            info!("Container already stopped");
        }

        let outcome = if self.rm {
            client.remove_container(&container.id, self.volumes).await?;

            if let Some(ref branch_name) = self.branch {
                remove_worktree(branch_name)?;
            }

            "removed"
        } else {
            "stopped"
        };

        print_outcome(&self.output, outcome, &container.id);
        cleanup_credential_proxy();

        Ok(())
    }
}

/// Deregister a container from the daemon (before stop so proxy teardown is clean).
async fn deregister_container(container: &ContainerInfo) {
    let Some(mgmt_sock) = daemon_management_socket_path() else {
        return;
    };
    if !mgmt_sock.exists() {
        return;
    }
    let req = cella_port::protocol::ManagementRequest::DeregisterContainer {
        container_name: container.name.clone(),
    };
    if let Err(e) = cella_daemon::management::send_management_request(&mgmt_sock, &req).await {
        debug!("Failed to deregister container with daemon: {e}");
    }
}

/// Print the outcome in the requested output format.
fn print_outcome(output: &OutputFormat, outcome: &str, container_id: &str) {
    match output {
        OutputFormat::Text => {
            if outcome == "removed" {
                eprintln!("Container stopped and removed.");
            } else {
                eprintln!("Container stopped.");
            }
        }
        OutputFormat::Json => {
            let output = json!({
                "outcome": outcome,
                "containerId": container_id,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&output).unwrap_or_default()
            );
        }
    }
}

/// Stop the credential proxy if no cella containers remain.
fn cleanup_credential_proxy() {
    if running_cella_container_count() == 0
        && let (Some(pid_path), Some(socket_path), Some(port_path)) = (
            credential_proxy_pid_path(),
            credential_proxy_socket_path(),
            credential_proxy_port_path(),
        )
        && stop_daemon(&pid_path, &socket_path, &port_path).is_ok()
    {
        debug!("Credential proxy daemon stopped (no containers remain)");
    }
}
