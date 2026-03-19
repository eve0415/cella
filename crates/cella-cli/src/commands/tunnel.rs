use std::path::PathBuf;

use clap::{Args, Subcommand};

use cella_env::git_credential::cella_data_dir;
use cella_tunnel::client::daemon_status;
use cella_tunnel::daemon;

/// Manage the tunnel daemon (internal).
#[derive(Args)]
pub struct TunnelArgs {
    #[command(subcommand)]
    command: TunnelCommand,
}

#[derive(Subcommand)]
enum TunnelCommand {
    /// Start the tunnel daemon (foreground).
    Daemon(DaemonArgs),
    /// Stop the tunnel daemon.
    Stop,
    /// Show tunnel daemon status.
    Status,
}

#[derive(Args)]
struct DaemonArgs {
    /// Socket path override.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// PID file path override.
    #[arg(long)]
    pid_file: Option<PathBuf>,
}

impl TunnelArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            TunnelCommand::Daemon(args) => run_daemon(args).await,
            TunnelCommand::Stop => run_stop(),
            TunnelCommand::Status => run_status(),
        }
    }
}

async fn run_daemon(args: DaemonArgs) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = args
        .socket
        .or_else(tunnel_socket_path)
        .ok_or("cannot determine socket path: HOME not set")?;
    let pid_path = args
        .pid_file
        .or_else(tunnel_pid_path)
        .ok_or("cannot determine PID file path: HOME not set")?;

    daemon::run_daemon(&socket_path, &pid_path).await?;
    Ok(())
}

fn run_stop() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = tunnel_socket_path().ok_or("cannot determine socket path: HOME not set")?;
    let pid_path = tunnel_pid_path().ok_or("cannot determine PID file path: HOME not set")?;

    daemon::stop_daemon(&pid_path, &socket_path)?;
    eprintln!("Tunnel daemon stopped.");
    Ok(())
}

fn run_status() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = tunnel_socket_path().ok_or("cannot determine socket path: HOME not set")?;
    let pid_path = tunnel_pid_path().ok_or("cannot determine PID file path: HOME not set")?;

    let status = daemon_status(&socket_path, &pid_path);

    if status.running {
        eprintln!("Tunnel daemon: running (PID {})", status.pid.unwrap_or(0));
    } else {
        eprintln!("Tunnel daemon: not running");
        if status.pid.is_some() {
            eprintln!("  PID file exists but daemon not responsive");
        }
        if status.socket_exists {
            eprintln!("  Socket file exists: {}", socket_path.display());
        }
    }

    // Show tunnel statuses if daemon is running
    if status.running
        && let Ok(tunnels) = cella_tunnel::client::query_status(&socket_path)
    {
        let trimmed = tunnels.trim();
        if trimmed.is_empty() {
            eprintln!("  No active tunnels");
        } else {
            eprintln!("  Tunnels:");
            for line in trimmed.lines() {
                eprintln!("    {line}");
            }
        }
    }

    let data_dir =
        cella_data_dir().map_or_else(|| "(unknown)".to_string(), |d| d.display().to_string());
    eprintln!("  Data directory: {data_dir}");

    Ok(())
}

/// Get the expected path for the tunnel daemon socket.
pub fn tunnel_socket_path() -> Option<PathBuf> {
    cella_data_dir().map(|d| d.join("tunnel.sock"))
}

/// Get the expected path for the tunnel daemon PID file.
pub fn tunnel_pid_path() -> Option<PathBuf> {
    cella_data_dir().map(|d| d.join("tunnel.pid"))
}
