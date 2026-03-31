mod branch;
mod build;
mod code;
pub mod compose_features;
mod compose_up;
mod config;
mod credential;
mod daemon;
mod doctor;
mod down;
mod env_cache;
mod exec;
pub mod image;
mod init;
mod list;
mod logs;
mod network;
mod nvim;
mod ports;
mod prune;
mod read_configuration;
mod shell;
pub mod shell_detect;
mod tmux;

mod switch;
mod template;
mod up;

use clap::{Args, Subcommand};
use tracing::warn;

use crate::progress::{Progress, Verbosity};

/// Common flags for commands that support verbose output.
#[derive(Args, Clone)]
pub struct VerboseArgs {
    /// Show expanded step details (container names, feature resolution, etc.).
    #[arg(short, long)]
    pub verbose: bool,
}

/// Top-level CLI commands.
#[derive(Subcommand)]
pub enum Command {
    /// Start a dev container for the current workspace.
    Up(up::UpArgs),
    /// Stop and remove the dev container.
    Down(down::DownArgs),
    /// Open a shell inside the running dev container.
    Shell(shell::ShellArgs),
    /// Execute a command inside the running dev container.
    Exec(exec::ExecArgs),
    /// Build the dev container image without starting it.
    Build(build::BuildArgs),
    /// List all dev containers managed by cella.
    List(list::ListArgs),
    /// View logs from the dev container.
    Logs(logs::LogsArgs),
    /// Check system dependencies and configuration.
    Doctor(doctor::DoctorArgs),
    /// Create a new worktree-backed branch with its own dev container.
    Branch(branch::BranchArgs),
    /// Switch to a different worktree-backed branch.
    Switch(switch::SwitchArgs),
    /// Remove stale worktrees and their associated containers.
    Prune(prune::PruneArgs),
    /// View and manage cella configuration.
    Config(config::ConfigArgs),
    /// Manage dev container templates.
    Template(template::TemplateArgs),
    /// Initialize cella in the current repository.
    Init(init::InitArgs),
    /// Open VS Code connected to the dev container.
    Code(code::CodeArgs),
    /// Open neovim inside the dev container.
    Nvim(nvim::NvimArgs),
    /// Open a persistent tmux session inside the dev container.
    Tmux(tmux::TmuxArgs),
    /// Inspect network proxy and blocking configuration.
    Network(network::NetworkArgs),
    /// View port forwarding status for dev containers.
    Ports(ports::PortsArgs),
    /// Manage credential forwarding for dev containers.
    Credential(credential::CredentialArgs),
    /// Read and output the resolved devcontainer configuration.
    #[command(name = "read-configuration")]
    ReadConfiguration(read_configuration::ReadConfigurationArgs),
    /// Manage the cella daemon.
    #[command(name = "daemon", hide = true)]
    Daemon(daemon::DaemonArgs),
}

impl Command {
    /// Whether this command uses text (non-JSON) output, i.e. spinners should be active.
    pub const fn is_text_output(&self) -> bool {
        match self {
            Self::Up(args) => args.is_text_output(),
            Self::Code(args) => args.is_text_output(),
            Self::Nvim(args) => args.is_text_output(),
            Self::Tmux(args) => args.is_text_output(),
            Self::Build(args) => args.is_text_output(),
            Self::Down(args) => args.is_text_output(),
            Self::ReadConfiguration(_) => false,
            _ => true,
        }
    }

    /// Extract verbosity from subcommands that support `--verbose`.
    pub const fn verbosity(&self) -> Verbosity {
        let verbose = match self {
            Self::Up(args) => args.verbose.verbose,
            Self::Code(args) => args.up.verbose.verbose,
            Self::Nvim(args) => args.up.verbose.verbose,
            Self::Tmux(args) => args.up.verbose.verbose,
            Self::Build(args) => args.verbose.verbose,
            Self::Branch(args) => args.verbose.verbose,
            Self::Down(args) => args.verbose.verbose,
            _ => false,
        };
        if verbose {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        }
    }

    /// Whether this is the `daemon start` subcommand, which initializes
    /// its own file-based tracing instead of the normal indicatif writer.
    pub const fn is_daemon_start(&self) -> bool {
        matches!(self, Self::Daemon(_))
    }

    pub async fn execute(self, progress: Progress) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Up(args) => args.execute(progress).await,
            Self::Code(args) => args.execute(progress).await,
            Self::Down(args) => args.execute().await,
            Self::Shell(args) => args.execute().await,
            Self::Exec(args) => args.execute().await,
            Self::Build(args) => args.execute(progress).await,
            Self::List(args) => args.execute().await,
            Self::Logs(args) => args.execute().await,
            Self::Doctor(args) => args.execute().await,
            Self::Branch(args) => args.execute(progress).await,

            Self::Switch(args) => args.execute().await,
            Self::Prune(args) => args.execute().await,
            Self::ReadConfiguration(args) => args.execute(),
            Self::Config(args) => args.execute(),
            Self::Template(args) => args.execute(),
            Self::Init(args) => args.execute(),
            Self::Nvim(args) => args.execute(progress).await,
            Self::Tmux(args) => args.execute(progress).await,
            Self::Credential(args) => args.execute().await,
            Self::Network(args) => args.execute(),
            Self::Ports(args) => args.execute().await,
            Self::Daemon(args) => args.execute().await,
        }
    }
}

/// Connect to the Docker daemon, optionally using an explicit host URL.
///
/// # Errors
///
/// Returns error if the Docker client cannot connect.
pub fn connect_docker(
    docker_host: Option<&str>,
) -> Result<cella_docker::DockerClient, cella_docker::CellaDockerError> {
    docker_host.map_or_else(cella_docker::DockerClient::connect, |host| {
        cella_docker::DockerClient::connect_with_host(host)
    })
}

/// Resolve the workspace folder from an optional argument or the current directory.
///
/// # Errors
///
/// Returns error if the current directory cannot be determined.
pub fn resolve_workspace_folder(
    opt: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    if let Some(wf) = opt {
        Ok(wf.canonicalize().unwrap_or_else(|_| wf.to_path_buf()))
    } else {
        Ok(std::env::current_dir()?)
    }
}

/// Resolve a specific compose service container from a base container.
///
/// If `service` is `Some`, looks up the compose project from the container's
/// labels and finds the matching service container.
///
/// # Errors
///
/// Returns error if the container is not compose-based or the service is not found.
pub async fn resolve_service_container(
    client: &cella_docker::DockerClient,
    container: cella_docker::ContainerInfo,
    service: Option<&str>,
) -> Result<cella_docker::ContainerInfo, Box<dyn std::error::Error>> {
    let Some(svc) = service else {
        return Ok(container);
    };

    let project = cella_compose::discovery::compose_project_from_labels(&container.labels)
        .ok_or_else(|| {
            format!(
                "--service flag requires a compose-based devcontainer, but '{}' is not",
                container.name
            )
        })?;

    client
        .find_compose_container(project, svc)
        .await?
        .ok_or_else(|| format!("Service '{svc}' not found in compose project '{project}'").into())
}

/// Terminal environment variables to forward into the container.
pub const TERMINAL_ENV_VARS: &[&str] = &[
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "LANG",
    "COLUMNS",
    "LINES",
];

/// Ensure the cella daemon is running and version-compatible.
///
/// Starts it as a background process if not already running.
/// If running but stale (binary rebuilt in debug, or version mismatch in release),
/// shuts it down gracefully and restarts.
pub async fn ensure_cella_daemon() {
    use cella_daemon::daemon;
    use cella_env::paths::cella_data_dir;

    let Some(data_dir) = cella_data_dir() else {
        warn!("Cannot determine cella data directory, skipping daemon start");
        return;
    };

    let pid_path = data_dir.join("daemon.pid");
    let socket_path = data_dir.join("daemon.sock");

    if daemon::is_daemon_running(&pid_path, &socket_path) {
        check_and_restart_if_stale(&pid_path, &socket_path).await;
        return;
    }

    if let Err(e) = daemon::ensure_daemon_running(&socket_path, &pid_path) {
        warn!("Failed to start cella daemon: {e}");
    }
}

/// Check if the running daemon is stale and restart it if necessary.
async fn check_and_restart_if_stale(pid_path: &std::path::Path, socket_path: &std::path::Path) {
    if check_daemon_needs_restart(socket_path).await == Some(true) {
        tracing::info!("Daemon version mismatch detected, restarting");
        restart_daemon(pid_path, socket_path).await;
    }
}

/// Check if the running daemon needs a restart due to version mismatch.
/// Returns `Some(true)` if restart needed, `Some(false)` if ok, `None` if check failed.
async fn check_daemon_needs_restart(control_socket_path: &std::path::Path) -> Option<bool> {
    use cella_port::protocol::{ManagementRequest, ManagementResponse};

    if !control_socket_path.exists() {
        return None;
    }

    let resp = cella_daemon::management::send_management_request(
        control_socket_path,
        &ManagementRequest::QueryStatus,
    )
    .await
    .ok()?;

    let ManagementResponse::Status {
        daemon_version,
        daemon_started_at,
        ..
    } = resp
    else {
        return None;
    };

    if daemon_version.is_empty() {
        return Some(true);
    }

    if cfg!(debug_assertions) {
        Some(is_binary_newer_than(daemon_started_at))
    } else {
        Some(daemon_version != env!("CARGO_PKG_VERSION"))
    }
}

/// Check if the current CLI binary was modified after the given timestamp.
fn is_binary_newer_than(daemon_started_at: u64) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Ok(meta) = exe.metadata() else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let mtime_secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    mtime_secs > daemon_started_at
}

/// Shut down the old daemon and start a fresh one, then re-register containers.
async fn restart_daemon(pid_path: &std::path::Path, socket_path: &std::path::Path) {
    use cella_daemon::daemon;

    graceful_shutdown_daemon(pid_path, socket_path).await;

    if let Err(e) = daemon::start_daemon_background(socket_path, pid_path) {
        warn!("Failed to restart daemon: {e}");
        return;
    }

    wait_for_socket(socket_path).await;

    if let Err(e) = re_register_containers(socket_path).await {
        warn!("Failed to re-register containers after restart: {e}");
    }
}

/// Send shutdown request and wait for the old daemon to exit.
async fn graceful_shutdown_daemon(pid_path: &std::path::Path, socket_path: &std::path::Path) {
    use cella_port::protocol::ManagementRequest;

    if socket_path.exists() {
        let _ = cella_daemon::management::send_management_request(
            socket_path,
            &ManagementRequest::Shutdown,
        )
        .await;
    }

    for _ in 0..50 {
        if !pid_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    if pid_path.exists() {
        let _ = std::fs::remove_file(pid_path);
        let _ = std::fs::remove_file(socket_path);
    }
}

/// Wait for the daemon's socket to appear (max 2s).
async fn wait_for_socket(socket_path: &std::path::Path) {
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if socket_path.exists() {
            break;
        }
    }
}

/// Re-register all running cella containers with the daemon.
async fn re_register_containers(
    socket_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use cella_docker::DockerClient;
    use cella_port::protocol::ManagementRequest;

    let client = DockerClient::connect()?;
    let containers = client.list_cella_containers(true).await?;

    for container in &containers {
        let container_ip =
            cella_docker::network::get_container_cella_ip(client.inner(), &container.id).await;

        // Read ports_attributes from container label
        let (ports_attrs, other_ports_attrs) = container
            .labels
            .get("dev.cella.ports_attributes")
            .map(|label| {
                cella_orchestrator::config_map::ports::deserialize_ports_attributes_label(label)
            })
            .unwrap_or_default();

        let shutdown_action = container.labels.get("dev.cella.shutdown_action").cloned();

        let req = ManagementRequest::RegisterContainer {
            container_id: container.id.clone(),
            container_name: container.name.clone(),
            container_ip,
            ports_attributes: ports_attrs,
            other_ports_attributes: other_ports_attrs,
            forward_ports: vec![],
            shutdown_action,
        };

        match cella_daemon::management::send_management_request(socket_path, &req).await {
            Ok(resp) => {
                tracing::debug!("Re-registered container {}: {resp:?}", container.name);
            }
            Err(e) => {
                warn!("Failed to re-register container {}: {e}", container.name);
            }
        }
    }

    Ok(())
}
