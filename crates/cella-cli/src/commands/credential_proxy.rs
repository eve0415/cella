use std::path::PathBuf;

use clap::{Args, Subcommand};

use cella_credential_proxy::client::daemon_status;
use cella_credential_proxy::daemon;
use cella_env::git_credential::{
    cella_data_dir, credential_proxy_pid_path, credential_proxy_socket_path,
};

/// Manage the credential proxy daemon (internal).
#[derive(Args)]
pub struct CredentialProxyArgs {
    #[command(subcommand)]
    command: CredentialProxyCommand,
}

#[derive(Subcommand)]
enum CredentialProxyCommand {
    /// Start the credential proxy daemon (foreground).
    Daemon(DaemonArgs),
    /// Stop the credential proxy daemon.
    Stop,
    /// Show credential proxy daemon status.
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

impl CredentialProxyArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            CredentialProxyCommand::Daemon(args) => run_daemon(args).await,
            CredentialProxyCommand::Stop => run_stop(),
            CredentialProxyCommand::Status => run_status(),
        }
    }
}

async fn run_daemon(args: DaemonArgs) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = args
        .socket
        .or_else(credential_proxy_socket_path)
        .ok_or("cannot determine socket path: HOME not set")?;
    let pid_path = args
        .pid_file
        .or_else(credential_proxy_pid_path)
        .ok_or("cannot determine PID file path: HOME not set")?;

    daemon::run_daemon(&socket_path, &pid_path).await?;
    Ok(())
}

fn run_stop() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path =
        credential_proxy_socket_path().ok_or("cannot determine socket path: HOME not set")?;
    let pid_path =
        credential_proxy_pid_path().ok_or("cannot determine PID file path: HOME not set")?;

    daemon::stop_daemon(&pid_path, &socket_path)?;
    eprintln!("Credential proxy daemon stopped.");
    Ok(())
}

fn run_status() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path =
        credential_proxy_socket_path().ok_or("cannot determine socket path: HOME not set")?;
    let pid_path =
        credential_proxy_pid_path().ok_or("cannot determine PID file path: HOME not set")?;

    let status = daemon_status(&socket_path, &pid_path);

    if status.running {
        eprintln!(
            "Credential proxy daemon: running (PID {})",
            status.pid.unwrap_or(0)
        );
    } else {
        eprintln!("Credential proxy daemon: not running");
        if status.pid.is_some() {
            eprintln!("  PID file exists but daemon not responsive");
        }
        if status.socket_exists {
            eprintln!("  Socket file exists: {}", socket_path.display());
        }
    }

    // Show paths
    let data_dir =
        cella_data_dir().map_or_else(|| "(unknown)".to_string(), |d| d.display().to_string());
    eprintln!("  Data directory: {data_dir}");

    Ok(())
}
