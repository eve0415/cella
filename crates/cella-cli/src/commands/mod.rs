mod branch;
mod build_cmd;
mod config;
mod credential_proxy;
mod doctor;
mod down;
mod env_cache;
mod exec;
pub mod image;
mod init;
mod list;
mod logs;
mod nvim;
mod prune;
mod shell;
mod spawn;
mod switch;
mod template;
mod up;

use clap::Subcommand;
use tracing::warn;

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
    Build(build_cmd::BuildArgs),
    /// List all dev containers managed by cella.
    List(list::ListArgs),
    /// View logs from the dev container.
    Logs(logs::LogsArgs),
    /// Check system dependencies and configuration.
    Doctor(doctor::DoctorArgs),
    /// Create a new worktree-backed branch with its own dev container.
    Branch(branch::BranchArgs),
    /// Spawn an AI agent sandbox.
    Spawn(spawn::SpawnArgs),
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
    /// Manage the credential proxy daemon.
    #[command(name = "credential-proxy", hide = true)]
    CredentialProxy(credential_proxy::CredentialProxyArgs),
}

impl Command {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Up(args) => args.execute().await,
            Self::Down(args) => args.execute().await,
            Self::Shell(args) => args.execute().await,
            Self::Exec(args) => args.execute().await,
            Self::Build(args) => args.execute().await,
            Self::List(args) => args.execute().await,
            Self::Logs(args) => args.execute().await,
            Self::Doctor(args) => args.execute().await,
            Self::Branch(args) => args.execute().await,
            Self::Spawn(args) => args.execute().await,
            Self::Switch(args) => args.execute().await,
            Self::Prune(args) => args.execute().await,
            Self::Config(args) => args.execute().await,
            Self::Template(args) => args.execute().await,
            Self::Init(args) => args.execute().await,
            Self::Nvim(args) => args.execute().await,
            Self::CredentialProxy(args) => args.execute().await,
        }
    }
}

/// Ensure the credential proxy daemon is running.
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
