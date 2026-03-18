mod branch;
mod build_cmd;
mod config;
mod doctor;
mod down;
mod exec;
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
}

impl Command {
    pub async fn execute(self, strict: bool) -> Result<(), Box<dyn std::error::Error>> {
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
            Self::Config(args) => args.execute(strict).await,
            Self::Template(args) => args.execute().await,
            Self::Init(args) => args.execute().await,
            Self::Nvim(args) => args.execute().await,
        }
    }
}
