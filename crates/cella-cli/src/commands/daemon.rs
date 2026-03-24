use std::path::PathBuf;

use clap::{Args, Subcommand};

use cella_daemon::client::daemon_status;
use cella_daemon::daemon;
use cella_daemon::health::running_cella_container_count;
use cella_env::git_credential::cella_data_dir;

/// Manage the cella daemon (internal).
#[derive(Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Start the daemon (foreground).
    Start(StartArgs),
    /// Stop the daemon.
    Stop,
    /// Show daemon status.
    Status,
}

#[derive(Args)]
struct StartArgs {
    /// Legacy credential socket path.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// PID file path.
    #[arg(long)]
    pid_file: Option<PathBuf>,
    /// TCP port file path.
    #[arg(long)]
    port_file: Option<PathBuf>,
    /// Control socket path (for agent communication).
    #[arg(long)]
    control_socket: Option<PathBuf>,
}

impl DaemonArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            DaemonCommand::Start(args) => run_start(args).await,
            DaemonCommand::Stop => run_stop(),
            DaemonCommand::Status => run_status().await,
        }
    }
}

async fn run_start(args: StartArgs) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = cella_data_dir().ok_or("cannot determine data dir: HOME not set")?;

    // Initialize file-based logging for the daemon process.
    cella_daemon::logging::init_daemon_logging(&data_dir.join("daemon.log"));

    let socket_path = args
        .socket
        .unwrap_or_else(|| data_dir.join("credential-proxy.sock"));
    let pid_path = args.pid_file.unwrap_or_else(|| data_dir.join("daemon.pid"));
    let port_path = args
        .port_file
        .unwrap_or_else(|| data_dir.join("credential-proxy.port"));
    let control_socket_path = args
        .control_socket
        .unwrap_or_else(|| data_dir.join("daemon.sock"));

    daemon::run_daemon(&socket_path, &pid_path, &port_path, &control_socket_path).await?;
    Ok(())
}

fn run_stop() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = cella_data_dir().ok_or("cannot determine data dir: HOME not set")?;

    let socket_path = data_dir.join("credential-proxy.sock");
    let pid_path = data_dir.join("daemon.pid");
    let port_path = data_dir.join("credential-proxy.port");
    let control_socket_path = data_dir.join("daemon.sock");

    daemon::stop_daemon(&pid_path, &socket_path, &port_path, &control_socket_path)?;
    eprintln!("Cella daemon stopped.");
    Ok(())
}

async fn run_status() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = cella_data_dir().ok_or("cannot determine data dir: HOME not set")?;

    let socket_path = data_dir.join("credential-proxy.sock");
    let pid_path = data_dir.join("daemon.pid");
    let mgmt_socket = data_dir.join("daemon.sock");

    let status = daemon_status(&socket_path, &pid_path);

    if status.running {
        eprintln!("Cella daemon: running (PID {})", status.pid.unwrap_or(0));
    } else {
        eprintln!("Cella daemon: not running");
        if status.pid.is_some() {
            eprintln!("  PID file exists but daemon not responsive");
        }
        if status.socket_exists {
            eprintln!("  Socket file exists: {}", socket_path.display());
        }
    }

    // Try to query daemon for detailed status
    if mgmt_socket.exists() {
        let result = cella_daemon::management::send_management_request(
            &mgmt_socket,
            &cella_port::protocol::ManagementRequest::QueryStatus,
        )
        .await;

        if let Ok(cella_port::protocol::ManagementResponse::Status {
            container_count,
            containers,
            is_orbstack,
            uptime_secs,
            daemon_version,
            control_port,
            ..
        }) = result
        {
            if !daemon_version.is_empty() {
                eprintln!("  Version: {daemon_version}");
            }
            eprintln!("  Uptime: {uptime_secs}s");
            eprintln!("  Control port: {control_port}");
            eprintln!("  OrbStack: {is_orbstack}");
            eprintln!("  Registered containers: {container_count}");
            for c in &containers {
                let agent_status = if c.agent_connected {
                    if c.last_seen_secs > 0 {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let ago = now.saturating_sub(c.last_seen_secs);
                        format!("connected (last seen {ago}s ago)")
                    } else {
                        "connected".to_string()
                    }
                } else {
                    "no connection".to_string()
                };
                eprintln!(
                    "    {} ({}) — {} ports, agent: {}",
                    c.container_name,
                    &c.container_id[..12.min(c.container_id.len())],
                    c.forwarded_port_count,
                    agent_status,
                );
            }

            let dir_display = cella_data_dir()
                .map_or_else(|| "(unknown)".to_string(), |d| d.display().to_string());
            eprintln!("  Data directory: {dir_display}");
            return Ok(());
        }
    }

    let container_count = running_cella_container_count();
    eprintln!("  Active cella containers: {container_count}");

    let dir_display =
        cella_data_dir().map_or_else(|| "(unknown)".to_string(), |d| d.display().to_string());
    eprintln!("  Data directory: {dir_display}");

    Ok(())
}
