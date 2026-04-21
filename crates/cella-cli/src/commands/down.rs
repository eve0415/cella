use std::path::PathBuf;

use clap::Args;
use serde_json::json;
use tracing::{debug, info};

use super::OutputFormat;
use cella_backend::ContainerTarget;
use cella_backend::{ContainerInfo, ContainerState};
use cella_compose::discovery;
use cella_daemon::daemon;
use cella_daemon::shared::running_cella_container_count;
use cella_env::paths::{cella_data_dir, daemon_socket_path};

use crate::picker;

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

    #[command(flatten)]
    backend: crate::backend::BackendArgs,

    /// Force stop even when shutdownAction is "none".
    #[arg(long)]
    force: bool,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

/// Resolve a branch name to its worktree path.
fn resolve_branch_to_path(
    branch_name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
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
fn remove_worktree(branch_name: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let client = self.backend.resolve_client().await?;

        let workspace_folder = if let Some(ref branch_name) = self.branch {
            Some(resolve_branch_to_path(branch_name)?)
        } else {
            self.workspace_folder
        };

        let no_explicit_target = self.container_id.is_none()
            && self.container_name.is_none()
            && self.branch.is_none()
            && workspace_folder.is_none();

        let target = ContainerTarget {
            container_id: self.container_id,
            container_name: self.container_name,
            id_label: None,
            workspace_folder,
        };

        let container = match target.resolve(client.as_ref(), false).await {
            Ok(c) => c,
            Err(_) if no_explicit_target => {
                let containers = client.as_ref().list_cella_containers(false).await?;
                picker::resolve_container_interactive(
                    &containers,
                    None,
                    "Select a container to stop:",
                    None,
                )?
            }
            Err(e) => return Err(e.into()),
        };
        super::warn_if_missing_backend_label(&container);

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
                        println!("{}", serde_json::to_string(&output).unwrap_or_default());
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
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("docker compose down failed: {e}").into()
                })?;

            // Clean up override file
            if let Some(data_dir) = cella_data_dir() {
                let override_path = data_dir
                    .join("compose")
                    .join(project_name)
                    .join("docker-compose.cella.yml");
                cella_compose::override_file::cleanup_override_file(&override_path);
            }

            print_outcome(&self.output, "stopped", &container.id);
            cleanup_daemon();
            return Ok(());
        }

        if container.state == ContainerState::Running {
            client.as_ref().stop_container(&container.id).await?;
            info!("Container stopped");
        } else {
            info!("Container already stopped");
        }

        let outcome = if self.rm {
            client
                .as_ref()
                .remove_container(&container.id, self.volumes)
                .await?;

            cleanup_workspace_network(client.as_ref(), &container).await;

            if let Some(ref branch_name) = self.branch {
                remove_worktree(branch_name)?;
            }

            "removed"
        } else {
            "stopped"
        };

        print_outcome(&self.output, outcome, &container.id);
        cleanup_daemon();

        Ok(())
    }
}

/// Remove the per-workspace `cella-net-*` network if no containers are
/// attached. Best-effort: errors are logged at debug and never surfaced
/// to the caller, because the container is already gone and the
/// enclosing `cella down --rm` has succeeded.
///
/// The `dev.cella.workspace_path` label is set from the canonicalized
/// workspace path at container creation, matching the input
/// `ensure_repo_network` hashes — so the derived network name lines up.
async fn cleanup_workspace_network(
    client: &dyn cella_backend::ContainerBackend,
    container: &ContainerInfo,
) {
    let Some(workspace) = container.labels.get("dev.cella.workspace_path") else {
        return;
    };
    let workspace_path = PathBuf::from(workspace);
    match client.remove_workspace_network(&workspace_path).await {
        Ok(outcome) => debug!(?outcome, "workspace network cleanup"),
        Err(e) => debug!("workspace network cleanup failed (non-fatal): {e}"),
    }
}

/// Deregister a container from the daemon (before stop so proxy teardown is clean).
pub(super) async fn deregister_container(container: &ContainerInfo) {
    let Some(mgmt_sock) = daemon_socket_path() else {
        return;
    };
    if !mgmt_sock.exists() {
        return;
    }
    let req = cella_protocol::ManagementRequest::DeregisterContainer {
        container_name: container.name.clone(),
    };
    if let Err(e) = cella_daemon::management::send_management_request(&mgmt_sock, &req).await {
        debug!("Failed to deregister container with daemon: {e}");
    }

    // Release the per-workspace SSH-agent proxy refcount. The daemon
    // refcounts internally — the listener is torn down only when the
    // count hits zero, so this is safe to call once per `cella down`
    // even when other containers in the same workspace are still up.
    if let Some(workspace) = container.labels.get("dev.cella.workspace_path") {
        let release = cella_protocol::ManagementRequest::ReleaseSshAgentProxy {
            workspace: workspace.clone(),
        };
        if let Err(e) =
            cella_daemon::management::send_management_request(&mgmt_sock, &release).await
        {
            debug!("Failed to release ssh-agent proxy with daemon: {e}");
        }
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
            println!("{}", serde_json::to_string(&output).unwrap_or_default());
        }
    }
}

/// Stop the daemon if no cella containers remain.
pub(super) fn cleanup_daemon() {
    if running_cella_container_count() == 0
        && let Some(data_dir) = cella_data_dir()
        && daemon::stop_daemon(&data_dir.join("daemon.pid"), &data_dir.join("daemon.sock")).is_ok()
    {
        debug!("Cella daemon stopped (no containers remain)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_outcome_text_stopped() {
        // Should not panic when printing "stopped" outcome
        print_outcome(&OutputFormat::Text, "stopped", "abc123");
    }

    #[test]
    fn print_outcome_text_removed() {
        // Should not panic when printing "removed" outcome
        print_outcome(&OutputFormat::Text, "removed", "abc123");
    }

    #[test]
    fn print_outcome_json_stopped() {
        // Should not panic when printing JSON "stopped" outcome
        print_outcome(&OutputFormat::Json, "stopped", "abc123456789");
    }

    #[test]
    fn print_outcome_json_removed() {
        // Should not panic when printing JSON "removed" outcome
        print_outcome(&OutputFormat::Json, "removed", "abc123456789");
    }

    #[test]
    fn down_args_is_text_output_default() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "down"]).unwrap();
        if let crate::commands::Command::Down(args) = &cli.command {
            assert!(args.is_text_output());
        }
    }

    #[test]
    fn down_args_is_json_output() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "down", "--output", "json"]).unwrap();
        if let crate::commands::Command::Down(args) = &cli.command {
            assert!(!args.is_text_output());
        }
    }
}
