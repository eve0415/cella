mod branch;
mod build;
mod compose_up;
mod config;
mod credential;
mod credential_proxy;
mod daemon;
mod doctor;
mod down;
mod env_cache;
mod exec;
pub mod image;
mod init;
mod list;
mod logs;
mod nvim;
mod ports;
mod prune;
mod read_configuration;
mod shell;

mod switch;
mod template;
mod up;

use clap::Subcommand;
use tracing::warn;

use crate::progress::Progress;

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
    /// Open neovim connected to the dev container.
    Nvim(nvim::NvimArgs),
    /// View port forwarding status for dev containers.
    Ports(ports::PortsArgs),
    /// Manage credential forwarding for dev containers.
    Credential(credential::CredentialArgs),
    /// Read and output the resolved devcontainer configuration.
    #[command(name = "read-configuration")]
    ReadConfiguration(read_configuration::ReadConfigurationArgs),
    /// Manage the credential proxy daemon (legacy).
    #[command(name = "credential-proxy", hide = true)]
    CredentialProxy(credential_proxy::CredentialProxyArgs),
    /// Manage the cella daemon.
    #[command(name = "daemon", hide = true)]
    Daemon(daemon::DaemonArgs),
}

impl Command {
    /// Whether this command uses text (non-JSON) output, i.e. spinners should be active.
    pub const fn is_text_output(&self) -> bool {
        match self {
            Self::Up(args) => args.is_text_output(),
            Self::Build(args) => args.is_text_output(),
            Self::Down(args) => args.is_text_output(),
            Self::ReadConfiguration(_) => false,
            _ => true,
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
            Self::Down(args) => args.execute().await,
            Self::Shell(args) => args.execute().await,
            Self::Exec(args) => args.execute().await,
            Self::Build(args) => args.execute(progress).await,
            Self::List(args) => args.execute().await,
            Self::Logs(args) => args.execute().await,
            Self::Doctor(args) => args.execute().await,
            Self::Branch(args) => args.execute(progress).await,

            Self::Switch(args) => args.execute(),
            Self::Prune(args) => args.execute().await,
            Self::ReadConfiguration(args) => args.execute(),
            Self::Config(args) => args.execute(),
            Self::Template(args) => args.execute(),
            Self::Init(args) => args.execute(),
            Self::Nvim(args) => args.execute(),
            Self::Credential(args) => args.execute().await,
            Self::CredentialProxy(args) => args.execute().await,
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

/// Ensure the credential proxy daemon is running (legacy).
///
/// Starts it as a background process if not already running.
/// Logs a warning and continues if it can't be started.
pub fn ensure_credential_proxy() {
    use cella_credential_proxy::daemon;
    use cella_env::git_credential::{
        credential_proxy_pid_path, credential_proxy_port_path, credential_proxy_socket_path,
    };

    let Some(socket_path) = credential_proxy_socket_path() else {
        return;
    };
    let Some(pid_path) = credential_proxy_pid_path() else {
        return;
    };
    let Some(port_path) = credential_proxy_port_path() else {
        return;
    };

    if let Err(e) = daemon::ensure_daemon_running(&socket_path, &pid_path, &port_path) {
        warn!("Failed to start credential proxy daemon: {e}");
    }
}

/// Ensure the unified cella daemon is running and version-compatible.
///
/// Starts it as a background process if not already running.
/// If running but stale (binary rebuilt in debug, or version mismatch in release),
/// shuts it down gracefully and restarts.
pub async fn ensure_cella_daemon() {
    use cella_daemon::daemon;
    use cella_env::git_credential::cella_data_dir;

    let Some(data_dir) = cella_data_dir() else {
        warn!("Cannot determine cella data directory, skipping daemon start");
        return;
    };

    let socket_path = data_dir.join("credential-proxy.sock");
    let pid_path = data_dir.join("daemon.pid");
    let port_path = data_dir.join("credential-proxy.port");
    let control_socket_path = data_dir.join("daemon.sock");

    if daemon::is_daemon_running(&pid_path, &socket_path) {
        check_and_restart_if_stale(&socket_path, &pid_path, &port_path, &control_socket_path).await;
        return;
    }

    if let Err(e) =
        daemon::ensure_daemon_running(&socket_path, &pid_path, &port_path, &control_socket_path)
    {
        warn!("Failed to start cella daemon: {e}");
    }
}

/// Check if the running daemon is stale and restart it if necessary.
async fn check_and_restart_if_stale(
    socket_path: &std::path::Path,
    pid_path: &std::path::Path,
    port_path: &std::path::Path,
    control_socket_path: &std::path::Path,
) {
    if check_daemon_needs_restart(control_socket_path).await == Some(true) {
        tracing::info!("Daemon version mismatch detected, restarting");
        restart_daemon(socket_path, pid_path, port_path, control_socket_path).await;
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
async fn restart_daemon(
    socket_path: &std::path::Path,
    pid_path: &std::path::Path,
    port_path: &std::path::Path,
    control_socket_path: &std::path::Path,
) {
    use cella_daemon::daemon;

    graceful_shutdown_daemon(socket_path, pid_path, port_path, control_socket_path).await;

    if let Err(e) =
        daemon::start_daemon_background(socket_path, pid_path, port_path, control_socket_path)
    {
        warn!("Failed to restart daemon: {e}");
        return;
    }

    wait_for_socket(control_socket_path).await;

    if let Err(e) = re_register_containers(control_socket_path).await {
        warn!("Failed to re-register containers after restart: {e}");
    }
}

/// Send shutdown request and wait for the old daemon to exit.
async fn graceful_shutdown_daemon(
    socket_path: &std::path::Path,
    pid_path: &std::path::Path,
    port_path: &std::path::Path,
    control_socket_path: &std::path::Path,
) {
    use cella_port::protocol::ManagementRequest;

    if control_socket_path.exists() {
        let _ = cella_daemon::management::send_management_request(
            control_socket_path,
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
        let _ = std::fs::remove_file(port_path);
        let _ = std::fs::remove_file(control_socket_path);
    }
}

/// Wait for the daemon's control socket to appear (max 2s).
async fn wait_for_socket(control_socket_path: &std::path::Path) {
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if control_socket_path.exists() {
            break;
        }
    }
}

/// Re-register all running cella containers with the daemon.
async fn re_register_containers(
    control_socket_path: &std::path::Path,
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
            .map(|label| cella_docker::config_map::ports::deserialize_ports_attributes_label(label))
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

        match cella_daemon::management::send_management_request(control_socket_path, &req).await {
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
