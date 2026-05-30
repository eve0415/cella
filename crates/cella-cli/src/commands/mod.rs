mod branch;
mod build;
mod code;
mod completion;
mod compose_up;
mod config;
mod credential;
mod daemon;
mod doctor;
mod down;
mod exec;
pub mod features;
mod init;
mod install;
mod list;
mod logs;
mod network;
mod outdated;
mod ports;
mod prune;
mod read_configuration;
mod run_user_commands;
mod shell;
mod status;
mod switch;
mod template;
pub mod up;

use std::io::IsTerminal;

use clap::{Args, Subcommand, ValueEnum};
use tracing::warn;

use crate::progress::{Progress, Verbosity};

/// Validate an `--id-label` value (`name=value`, both non-empty). Shared by
/// `up` and `run-user-commands` (the official validation is identical).
pub fn parse_id_label(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() && !v.is_empty() => Ok(s.to_string()),
        _ => Err("id-label must match <name>=<value>".to_string()),
    }
}

/// Validate a `--remote-env` value (`name=value`, value may be empty). Shared
/// by `up` and `run-user-commands`.
pub fn parse_remote_env(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((k, _)) if !k.is_empty() => Ok(s.to_string()),
        _ => Err("remote-env must match <name>=<value>".to_string()),
    }
}

/// Dotfiles install flags (`--dotfiles-*`). Shared verbatim by `up` and
/// `run-user-commands`; flattened into each command's arg struct.
#[derive(Args)]
pub struct DotfilesArgs {
    /// URL of a dotfiles Git repository to clone into the container.
    #[arg(long = "dotfiles-repository")]
    pub(crate) repository: Option<String>,

    /// Command to run after cloning the dotfiles repository. Defaults to the
    /// first of install.sh, install, bootstrap.sh, bootstrap, setup.sh, setup.
    #[arg(long = "dotfiles-install-command")]
    pub(crate) install_command: Option<String>,

    /// Path to clone the dotfiles repository to (default `~/dotfiles`).
    #[arg(long = "dotfiles-target-path", default_value = "~/dotfiles")]
    pub(crate) target_path: String,
}

/// Common flags for commands that support verbose output.
#[derive(Args, Clone)]
pub struct VerboseArgs {
    /// Show expanded step details (container names, feature resolution, etc.).
    #[arg(short, long)]
    pub verbose: bool,
}

/// Output format for container commands.
#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    /// Resolve at runtime: `Json` when stdout is not a terminal
    /// (piped/scripted), `Text` when attached to a terminal.
    Auto,
    Text,
    Json,
}

impl OutputFormat {
    /// Collapse `Auto` to a concrete `Text`/`Json` variant.
    ///
    /// `Auto` resolves to `Json` when stdout is not a terminal (the output is
    /// being piped or captured by a script) and `Text` otherwise. `Text` and
    /// `Json` pass through unchanged, so callers can `match` on the result
    /// without ever seeing `Auto`.
    #[must_use]
    pub fn resolve(&self) -> Self {
        match self {
            Self::Auto => {
                if std::io::stdout().is_terminal() {
                    Self::Text
                } else {
                    Self::Json
                }
            }
            Self::Text => Self::Text,
            Self::Json => Self::Json,
        }
    }
}

/// Image pull policy for container builds.
#[derive(Clone, ValueEnum)]
pub enum ImagePullPolicy {
    Always,
    Missing,
    Never,
}

impl ImagePullPolicy {
    pub const fn as_str(&self) -> &str {
        match self {
            Self::Always => "always",
            Self::Missing => "missing",
            Self::Never => "never",
        }
    }
}

/// Strictness level for validation.
#[derive(Clone, ValueEnum)]
pub enum StrictnessLevel {
    /// Fail on unmet host requirements.
    #[value(name = "host-requirements")]
    HostRequirements,
    /// Enable all strictness checks.
    All,
}

/// Mount consistency mode for workspace mounts.
#[derive(Clone, Debug, ValueEnum)]
pub enum MountConsistency {
    Consistent,
    Cached,
    Delegated,
}

impl MountConsistency {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Consistent => "consistent",
            Self::Cached => "cached",
            Self::Delegated => "delegated",
        }
    }
}

/// Pull policy for Docker Compose services.
#[derive(Clone, ValueEnum)]
pub enum ComposePullPolicy {
    Always,
    Missing,
    Never,
    Build,
}

impl ComposePullPolicy {
    pub const fn as_str(&self) -> &str {
        match self {
            Self::Always => "always",
            Self::Missing => "missing",
            Self::Never => "never",
            Self::Build => "build",
        }
    }
}

/// Availability of GPUs for dev containers that request one.
#[derive(Clone, Copy, ValueEnum)]
pub enum GpuAvailability {
    /// Expect a GPU to be available.
    All,
    /// Use a GPU if one is detected and the config requires it (default).
    Detect,
    /// Never expose a GPU to the container.
    None,
}

/// Default for updating the remote user's UID/GID to the local user's.
#[derive(Clone, Copy, ValueEnum)]
pub enum UpdateRemoteUserUidDefault {
    /// Never update the remote user's UID/GID.
    Never,
    /// Update by default unless devcontainer.json opts out (default).
    On,
    /// Do not update by default unless devcontainer.json opts in.
    Off,
}

/// Log verbosity for lifecycle/terminal logging.
#[derive(Clone, Copy, ValueEnum)]
pub enum LogLevel {
    Info,
    Debug,
    Trace,
}

/// Log output format.
#[derive(Clone, Copy, ValueEnum)]
pub enum LogFormat {
    Text,
    Json,
}

/// Controls whether `BuildKit` is used when building images.
#[derive(Clone, Copy, ValueEnum)]
pub enum BuildKitMode {
    /// Use `BuildKit` when available (default).
    Auto,
    /// Never use `BuildKit`.
    Never,
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
    /// Install tools into the running dev container.
    Install(install::InstallArgs),
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
    /// Manage devcontainer features.
    Features(features::FeaturesArgs),
    /// Show current and available versions.
    Outdated(outdated::OutdatedArgs),
    /// Initialize cella in the current repository.
    Init(init::InitArgs),
    /// Open VS Code connected to the dev container.
    Code(code::CodeArgs),
    /// Inspect network proxy and blocking configuration.
    Network(network::NetworkArgs),
    /// View port forwarding status for dev containers.
    Ports(ports::PortsArgs),
    /// Show system-level status overview.
    Status(status::StatusArgs),
    /// Manage credential forwarding for dev containers.
    Credential(credential::CredentialArgs),
    /// Read and output the resolved devcontainer configuration.
    #[command(name = "read-configuration")]
    ReadConfiguration(read_configuration::ReadConfigurationArgs),
    /// Re-run lifecycle hooks against an existing dev container.
    #[command(name = "run-user-commands")]
    RunUserCommands(run_user_commands::RunUserCommandsArgs),
    /// Generate shell completion scripts.
    Completion(completion::CompletionArgs),
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
            Self::Build(args) => args.is_text_output(),
            Self::Down(args) => args.is_text_output(),
            // Both emit a JSON envelope on stdout; spinners would fight it.
            Self::ReadConfiguration(_) | Self::RunUserCommands(_) => false,
            _ => true,
        }
    }

    /// Extract verbosity from subcommands that support `--verbose`.
    pub const fn verbosity(&self) -> Verbosity {
        let verbose = match self {
            Self::Up(args) => args.verbose.verbose,
            Self::Code(args) => args.up.verbose.verbose,
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

    /// The `--log-level` value, if the subcommand carries one.
    ///
    /// Only `up` (and `code`, which embeds the `up` arg surface) expose
    /// `--log-level`; every other variant returns `None`. main.rs reads this
    /// once, before subcommand dispatch, to seed the global tracing filter —
    /// the level can't be applied inside `execute()` because the subscriber is
    /// already installed by then.
    pub const fn log_level(&self) -> Option<LogLevel> {
        match self {
            Self::Up(args) => args.compat.log_level,
            Self::Code(args) => args.up.compat.log_level,
            Self::RunUserCommands(args) => args.compat.log_level,
            _ => None,
        }
    }

    /// The `--log-format` value (defaults to `Text`).
    ///
    /// Only `up`/`code` expose `--log-format`; every other variant returns
    /// `Text`. Read once in main.rs to select the tracing formatter and to
    /// force spinners off under `Json` (indicatif ANSI escapes would corrupt
    /// machine-readable JSON log lines on stderr).
    pub const fn log_format(&self) -> LogFormat {
        match self {
            Self::Up(args) => args.compat.log_format,
            Self::Code(args) => args.up.compat.log_format,
            Self::RunUserCommands(args) => args.compat.log_format,
            _ => LogFormat::Text,
        }
    }

    pub async fn execute(
        self,
        progress: Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Up(args) => args.execute(progress).await,
            Self::Code(args) => args.execute(progress).await,
            Self::Down(args) => args.execute().await,
            Self::Shell(args) => args.execute().await,
            Self::Exec(args) => args.execute().await,
            Self::Install(args) => args.execute().await,
            Self::Build(args) => args.execute(progress).await,
            Self::List(args) => args.execute().await,
            Self::Logs(args) => args.execute().await,
            Self::Doctor(args) => args.execute().await,
            Self::Branch(args) => args.execute(progress).await,
            Self::Switch(args) => args.execute().await,
            Self::Prune(args) => args.execute().await,
            Self::ReadConfiguration(args) => args.execute().await,
            Self::RunUserCommands(args) => args.execute(progress).await,
            Self::Config(args) => args.execute(),
            Self::Template(args) => args.execute(),
            Self::Features(args) => args.execute(progress).await,
            Self::Outdated(args) => args.execute().await,
            Self::Init(args) => args.execute(progress).await,
            Self::Completion(args) => {
                args.execute();
                Ok(())
            }
            Self::Credential(args) => args.execute().await,
            Self::Network(args) => args.execute(),
            Self::Ports(args) => args.execute().await,
            Self::Status(args) => args.execute().await,
            Self::Daemon(args) => args.execute().await,
        }
    }
}

/// Emit a warning if a container lacks the `dev.cella.backend` label.
///
/// Pre-existing containers (created before backend labeling) won't have
/// this label; we assume Docker and nudge the user to rebuild.
pub fn warn_if_missing_backend_label(container: &cella_backend::ContainerInfo) {
    if !container.labels.contains_key(cella_backend::BACKEND_LABEL) {
        warn!(
            "Container '{}' has no backend label. Assuming Docker. \
             Run `cella up --rebuild` to add it.",
            container.name
        );
    }
}

/// Resolve the workspace folder from an optional argument or the current directory.
///
/// # Errors
///
/// Returns error if the current directory cannot be determined.
pub fn resolve_workspace_folder(
    opt: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
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
    client: &dyn cella_backend::ContainerBackend,
    container: cella_backend::ContainerInfo,
    service: Option<&str>,
) -> Result<cella_backend::ContainerInfo, Box<dyn std::error::Error + Send + Sync>> {
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
        .find_compose_service(project, svc)
        .await?
        .ok_or_else(|| format!("Service '{svc}' not found in compose project '{project}'").into())
}

/// Load shell preferences from cella config.
///
/// Uses the container's workspace path label to find project-level config.
/// Returns an empty list if the workspace label is missing or config is unavailable.
pub fn load_shell_preferred(labels: &std::collections::HashMap<String, String>) -> Vec<String> {
    let Some(workspace_path) = labels
        .get("dev.cella.workspace_path")
        .filter(|p| !p.trim().is_empty())
    else {
        return Vec::new();
    };

    let workspace = std::path::Path::new(workspace_path);
    let resolved = cella_config::devcontainer::resolve::config(workspace, None).ok();
    let Ok(settings) = cella_config::CellaConfig::load(workspace, resolved.as_ref()) else {
        return Vec::new();
    };
    settings.shell.preferred
}

/// Resolve the `userEnvProbe` type from container labels, falling back to the given default.
pub fn resolve_probe_type_from_labels(
    labels: &std::collections::HashMap<String, String>,
    default: cella_env::user_env_probe::UserEnvProbe,
) -> cella_env::user_env_probe::UserEnvProbe {
    labels
        .get("dev.cella.user_env_probe")
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Append AI provider API keys from the host environment into `env`.
///
/// Loads settings from the workspace path label on the container,
/// then detects keys that are present on the host, enabled in config,
/// and not already set by the user (via `remoteEnv` / `containerEnv`).
pub async fn append_ai_keys(
    env: &mut Vec<String>,
    labels: &std::collections::HashMap<String, String>,
) {
    // Credential protection: inject phantom tokens from daemon, not real keys.
    if labels
        .get("dev.cella.credential_protect")
        .is_some_and(|v| v == "true")
    {
        append_phantom_ai_keys(env, labels).await;
        return;
    }

    // Fast path: skip settings I/O when no AI key env vars exist on the host.
    if !cella_env::ai_keys::any_ai_key_present() {
        return;
    }

    // Only forward host AI credentials to cella-managed containers.
    // Without the workspace label, this could be a non-cella container
    // targeted by --container-id, and leaking keys would be unexpected.
    let Some(workspace_path) = labels
        .get("dev.cella.workspace_path")
        .filter(|p| !p.trim().is_empty())
    else {
        return;
    };

    let workspace = std::path::Path::new(workspace_path);
    let resolved = cella_config::devcontainer::resolve::config(workspace, None).ok();
    let Ok(settings) = cella_config::CellaConfig::load(workspace, resolved.as_ref()) else {
        return;
    };
    let ai = &settings.credentials.ai;
    if !ai.enabled {
        return;
    }
    let existing_keys: Vec<&str> = env
        .iter()
        .filter_map(|e| e.split_once('=').map(|(k, _)| k))
        .collect();
    let ai_keys =
        cella_env::ai_keys::detect_ai_keys(&|id| ai.is_provider_enabled(id), &existing_keys);
    for (key, value) in ai_keys {
        env.push(format!("{key}={value}"));
    }
}

async fn append_phantom_ai_keys(
    env: &mut Vec<String>,
    labels: &std::collections::HashMap<String, String>,
) {
    let Some(container_name) = labels.get("dev.cella.container_name") else {
        return;
    };

    let phantom_tokens = query_daemon_phantom_tokens(container_name).await;
    if phantom_tokens.is_empty() {
        tracing::debug!("Credential protection active but no phantom tokens from daemon");
        return;
    }

    for (env_var, phantom_value) in phantom_tokens {
        if env.iter().any(|e| e.starts_with(&format!("{env_var}="))) {
            continue;
        }
        env.push(format!("{env_var}={phantom_value}"));
    }
}

async fn query_daemon_phantom_tokens(
    container_name: &str,
) -> std::collections::HashMap<String, String> {
    let Some(socket_path) = cella_env::paths::daemon_socket_path() else {
        return std::collections::HashMap::new();
    };

    let req = cella_protocol::ManagementRequest::GetPhantomTokens {
        container_name: container_name.to_string(),
    };

    match cella_daemon_client::send_management_request(&socket_path, &req).await {
        Ok(cella_protocol::ManagementResponse::PhantomTokenValues { tokens }) => tokens,
        _ => std::collections::HashMap::new(),
    }
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
    use cella_protocol::{ManagementRequest, ManagementResponse};

    if !control_socket_path.exists() {
        return None;
    }

    let resp = cella_daemon_client::send_management_request(
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
///
/// Order is load-bearing: `.daemon_addr` must be written BEFORE agents are
/// restarted, because restarted agents read this file on startup via
/// `read_daemon_addr_file()`.
async fn restart_daemon(pid_path: &std::path::Path, socket_path: &std::path::Path) {
    use cella_daemon::daemon;

    graceful_shutdown_daemon(pid_path, socket_path).await;

    if let Err(e) = daemon::start_daemon_background(socket_path, pid_path) {
        warn!("Failed to restart daemon: {e}");
        return;
    }

    wait_for_socket(socket_path).await;

    // Construct a single client for all post-restart operations.
    let Ok(client) = crate::backend::BackendArgs::default()
        .resolve_client()
        .await
    else {
        warn!("Failed to resolve container backend after daemon restart");
        return;
    };

    // Update the shared-volume address file so agents can discover the new daemon.
    // Only restart agents if the write succeeded — restarting with a stale
    // .daemon_addr would put agents into standalone mode permanently, which is
    // worse than leaving the existing process with its background retry loop.
    let addr_updated = up::write_daemon_addr_to_volume(&*client).await;

    if let Err(e) = re_register_containers(socket_path, &*client).await {
        warn!("Failed to re-register containers after restart: {e}");
    }

    if addr_updated {
        restart_agents_in_all_containers(&*client).await;
    } else {
        warn!("Skipping agent restart: .daemon_addr was not updated");
    }
}

/// Send shutdown request and wait for the old daemon to exit.
async fn graceful_shutdown_daemon(pid_path: &std::path::Path, socket_path: &std::path::Path) {
    use cella_protocol::ManagementRequest;

    if socket_path.exists() {
        let _ =
            cella_daemon_client::send_management_request(socket_path, &ManagementRequest::Shutdown)
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
    client: &dyn cella_backend::ContainerBackend,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let containers = client.list_cella_containers(true).await?;

    for container in &containers {
        if let Some(workspace_path) = container.labels.get("dev.cella.workspace_path")
            && let Err(e) = client
                .ensure_container_network(&container.id, std::path::Path::new(workspace_path))
                .await
        {
            tracing::debug!(
                "Failed to connect container {} to cella network: {e}",
                container.name
            );
        }

        let container_ip = client.get_container_ip(&container.id).await.unwrap_or(None);

        let registration = cella_orchestrator::daemon_registration::from_container_labels(
            container,
            container_ip,
            std::env::var("DOCKER_HOST").ok(),
        );

        match cella_daemon_client::DaemonClient::new(socket_path)
            .register_container(registration)
            .await
        {
            Ok(container_name) => {
                tracing::debug!("Re-registered container {container_name}");
            }
            Err(e) => {
                warn!("Failed to re-register container {}: {e}", container.name);
            }
        }
    }

    Ok(())
}

/// Restart agents in all running cella containers so they reconnect to the new daemon.
async fn restart_agents_in_all_containers(client: &dyn cella_backend::ContainerBackend) {
    let Ok(containers) = client.list_cella_containers(true).await else {
        warn!("Failed to list containers for agent restart");
        return;
    };

    for container in &containers {
        cella_backend::agent::restart_agent_in_container(client, &container.id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    // ── TERMINAL_ENV_VARS ───────────────────────────────────────────

    #[test]
    fn terminal_env_vars_contains_term() {
        assert!(TERMINAL_ENV_VARS.contains(&"TERM"));
    }

    #[test]
    fn terminal_env_vars_contains_colorterm() {
        assert!(TERMINAL_ENV_VARS.contains(&"COLORTERM"));
    }

    #[test]
    fn terminal_env_vars_contains_lang() {
        assert!(TERMINAL_ENV_VARS.contains(&"LANG"));
    }

    #[test]
    fn terminal_env_vars_is_not_empty() {
        assert!(!TERMINAL_ENV_VARS.is_empty());
    }

    #[test]
    fn terminal_env_vars_all_uppercase() {
        for var in TERMINAL_ENV_VARS {
            assert_eq!(
                *var,
                var.to_uppercase(),
                "env var should be uppercase: {var}"
            );
        }
    }

    // ── resolve_workspace_folder ────────────────────────────────────

    #[test]
    fn resolve_workspace_folder_none_returns_cwd() {
        let result = resolve_workspace_folder(None).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(result, cwd);
    }

    #[test]
    fn resolve_workspace_folder_with_existing_path() {
        let tmp = std::env::temp_dir();
        let result = resolve_workspace_folder(Some(&tmp)).unwrap();
        // canonicalize should succeed for an existing directory
        assert_eq!(result, tmp.canonicalize().unwrap());
    }

    #[test]
    fn resolve_workspace_folder_with_nonexistent_path() {
        let fake = PathBuf::from("/nonexistent/path/to/workspace");
        let result =
            resolve_workspace_folder(Some(Path::new("/nonexistent/path/to/workspace"))).unwrap();
        // canonicalize fails, so it returns the path as-is
        assert_eq!(result, fake);
    }

    // ── is_binary_newer_than ────────────────────────────────────────

    #[test]
    fn is_binary_newer_than_zero_is_true() {
        // Current binary was certainly modified after epoch
        assert!(is_binary_newer_than(0));
    }

    #[test]
    fn is_binary_newer_than_far_future_is_false() {
        // A timestamp far in the future should not be older than the binary
        let far_future = u64::MAX / 2;
        assert!(!is_binary_newer_than(far_future));
    }

    // ── VerboseArgs ─────────────────────────────────────────────────

    #[test]
    fn verbose_args_default_is_false() {
        let args = VerboseArgs { verbose: false };
        assert!(!args.verbose);
    }

    #[test]
    fn verbose_args_set_to_true() {
        let args = VerboseArgs { verbose: true };
        assert!(args.verbose);
    }

    // ── TERMINAL_ENV_VARS coverage ─────────────────────────────────

    #[test]
    fn terminal_env_vars_count() {
        assert_eq!(TERMINAL_ENV_VARS.len(), 7);
    }

    #[test]
    fn terminal_env_vars_contains_term_program() {
        assert!(TERMINAL_ENV_VARS.contains(&"TERM_PROGRAM"));
    }

    #[test]
    fn terminal_env_vars_contains_term_program_version() {
        assert!(TERMINAL_ENV_VARS.contains(&"TERM_PROGRAM_VERSION"));
    }

    #[test]
    fn terminal_env_vars_contains_columns() {
        assert!(TERMINAL_ENV_VARS.contains(&"COLUMNS"));
    }

    #[test]
    fn terminal_env_vars_contains_lines() {
        assert!(TERMINAL_ENV_VARS.contains(&"LINES"));
    }

    #[test]
    fn terminal_env_vars_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for var in TERMINAL_ENV_VARS {
            assert!(seen.insert(var), "duplicate env var: {var}");
        }
    }

    // ── is_binary_newer_than ────────────────────────────────────────

    #[test]
    fn is_binary_newer_than_recent_past() {
        // A timestamp from a few seconds ago should still be older than the binary
        let recent = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 5;
        // Binary may or may not be newer than 5s ago, but it shouldn't panic
        let _ = is_binary_newer_than(recent);
    }

    // ── resolve_workspace_folder edge cases ────────────────────────

    #[test]
    fn resolve_workspace_folder_returns_absolute_path() {
        let result = resolve_workspace_folder(None).unwrap();
        assert!(result.is_absolute());
    }

    // ── append_ai_keys ─────────────────────────────────────────────

    /// Serialize tests that mutate the process environment.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn append_ai_keys_skips_without_workspace_label() {
        let labels = std::collections::HashMap::new();
        let mut env = Vec::new();
        append_ai_keys(&mut env, &labels).await;
        assert!(
            env.is_empty(),
            "no keys should be forwarded without workspace label"
        );
    }

    #[tokio::test]
    async fn append_ai_keys_skips_empty_workspace_label() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("dev.cella.workspace_path".to_string(), "  ".to_string());
        let mut env = Vec::new();
        append_ai_keys(&mut env, &labels).await;
        assert!(
            env.is_empty(),
            "no keys should be forwarded with empty workspace label"
        );
    }

    #[tokio::test]
    #[allow(unsafe_code)]
    async fn append_ai_keys_forwards_host_key() {
        let _guard = ENV_LOCK.lock().await;
        unsafe { std::env::set_var("COHERE_API_KEY", "test-cohere") };

        let tmp = std::env::temp_dir();
        let mut labels = std::collections::HashMap::new();
        labels.insert(
            "dev.cella.workspace_path".to_string(),
            tmp.to_string_lossy().to_string(),
        );

        let mut env = Vec::new();
        append_ai_keys(&mut env, &labels).await;
        assert!(
            env.iter().any(|e| e == "COHERE_API_KEY=test-cohere"),
            "host COHERE_API_KEY should be forwarded"
        );
        unsafe { std::env::remove_var("COHERE_API_KEY") };
    }

    #[tokio::test]
    #[allow(unsafe_code)]
    async fn append_ai_keys_skips_existing_override() {
        let _guard = ENV_LOCK.lock().await;
        unsafe { std::env::set_var("OPENAI_API_KEY", "host-key") };

        let tmp = std::env::temp_dir();
        let mut labels = std::collections::HashMap::new();
        labels.insert(
            "dev.cella.workspace_path".to_string(),
            tmp.to_string_lossy().to_string(),
        );

        let mut env = vec!["OPENAI_API_KEY=user-override".to_string()];
        append_ai_keys(&mut env, &labels).await;

        let count = env
            .iter()
            .filter(|e| e.starts_with("OPENAI_API_KEY="))
            .count();
        assert_eq!(count, 1, "existing override should block auto-forwarding");
        assert_eq!(env[0], "OPENAI_API_KEY=user-override");

        unsafe { std::env::remove_var("OPENAI_API_KEY") };
    }

    #[tokio::test]
    #[allow(unsafe_code)]
    async fn append_ai_keys_respects_config_disables() {
        let _guard = ENV_LOCK.lock().await;
        unsafe { std::env::set_var("OPENAI_API_KEY", "host-key") };

        let workspace = std::env::temp_dir().join(format!(
            "cella-append-ai-keys-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let devcontainer_dir = workspace.join(".devcontainer");
        std::fs::create_dir_all(&devcontainer_dir).expect("create .devcontainer directory");

        let mut labels = std::collections::HashMap::new();
        labels.insert(
            "dev.cella.workspace_path".to_string(),
            workspace.to_string_lossy().to_string(),
        );

        std::fs::write(
            devcontainer_dir.join("cella.toml"),
            "[credentials.ai]\nenabled = false\n",
        )
        .expect("write disabled AI config");

        let mut env = Vec::new();
        append_ai_keys(&mut env, &labels).await;
        assert!(
            !env.iter().any(|e| e.starts_with("OPENAI_API_KEY=")),
            "OPENAI_API_KEY should not be forwarded when [credentials.ai] enabled = false"
        );

        std::fs::write(
            devcontainer_dir.join("cella.toml"),
            "[credentials.ai]\nopenai = false\n",
        )
        .expect("write provider-disabled AI config");

        let mut env = Vec::new();
        append_ai_keys(&mut env, &labels).await;
        assert!(
            !env.iter().any(|e| e.starts_with("OPENAI_API_KEY=")),
            "OPENAI_API_KEY should not be forwarded when [credentials.ai] openai = false"
        );

        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
