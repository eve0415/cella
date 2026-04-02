use std::path::PathBuf;

use clap::{Args, Subcommand};

use cella_daemon::daemon;
use cella_daemon::shared::running_cella_container_count;
use cella_env::paths::cella_data_dir;

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
    /// PID file path.
    #[arg(long)]
    pid_file: Option<PathBuf>,
    /// Control socket path (for management and agent communication).
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
    cella_daemon::logging::init_daemon_logging(&data_dir.join("daemon.log"));

    let pid_path = args.pid_file.unwrap_or_else(|| data_dir.join("daemon.pid"));
    let socket_path = args
        .control_socket
        .unwrap_or_else(|| data_dir.join("daemon.sock"));

    daemon::run_daemon(&socket_path, &pid_path).await?;
    Ok(())
}

fn run_stop() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = cella_data_dir().ok_or("cannot determine data dir: HOME not set")?;
    let pid_path = data_dir.join("daemon.pid");
    let socket_path = data_dir.join("daemon.sock");
    daemon::stop_daemon(&pid_path, &socket_path)?;
    eprintln!("Cella daemon stopped.");
    Ok(())
}

async fn run_status() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = cella_data_dir().ok_or("cannot determine data dir: HOME not set")?;
    let pid_path = data_dir.join("daemon.pid");
    let socket_path = data_dir.join("daemon.sock");

    let running = daemon::is_daemon_running(&pid_path, &socket_path);

    if running {
        eprintln!("Cella daemon: running");
    } else {
        eprintln!("Cella daemon: not running");
    }

    // Try to query daemon for detailed status via management socket
    if socket_path.exists() {
        let result = cella_daemon::management::send_management_request(
            &socket_path,
            &cella_protocol::ManagementRequest::QueryStatus,
        )
        .await;

        if let Ok(cella_protocol::ManagementResponse::Status {
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
