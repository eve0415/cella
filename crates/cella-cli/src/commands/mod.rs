mod branch;
mod build;
mod code;
mod completions;
mod compose_up;
mod config;
mod credential;
mod daemon;
mod doctor;
mod down;
mod exec;
pub mod features;
mod init;
mod list;
mod logs;
mod network;
mod nvim;
mod ports;
mod prune;
mod read_configuration;
mod shell;
mod tmux;

mod switch;
mod template;
pub mod up;

use clap::{Args, Subcommand, ValueEnum};
use tracing::warn;

use crate::progress::{Progress, Verbosity};

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
    Text,
    Json,
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
    /// Manage devcontainer features.
    Features(features::FeaturesArgs),
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
    /// Generate shell completion scripts.
    Completions(completions::CompletionsArgs),
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
            Self::Features(args) => args.execute(progress).await,
            Self::Init(args) => args.execute(progress).await,
            Self::Nvim(args) => args.execute(progress).await,
            Self::Tmux(args) => args.execute(progress).await,
            Self::Completions(args) => {
                args.execute();
                Ok(())
            }
            Self::Credential(args) => args.execute().await,
            Self::Network(args) => args.execute(),
            Self::Ports(args) => args.execute().await,
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

/// Append AI provider API keys from the host environment into `env`.
///
/// Loads settings from the workspace path label on the container,
/// then detects keys that are present on the host, enabled in config,
/// and not already set by the user (via `remoteEnv` / `containerEnv`).
pub fn append_ai_keys(env: &mut Vec<String>, labels: &std::collections::HashMap<String, String>) {
    // Only forward host AI credentials to cella-managed containers.
    // Without the workspace label, this could be a non-cella container
    // targeted by --container-id, and leaking keys would be unexpected.
    let Some(workspace_path) = labels
        .get("dev.cella.workspace_path")
        .filter(|p| !p.trim().is_empty())
    else {
        return;
    };

    let settings = cella_config::settings::Settings::load(std::path::Path::new(workspace_path));
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
    use cella_protocol::ManagementRequest;

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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use cella_protocol::ManagementRequest;

    let client = crate::backend::BackendArgs::default()
        .resolve_client()
        .await?;
    let containers = client.list_cella_containers(true).await?;

    for container in &containers {
        let container_ip = client.get_container_ip(&container.id).await.unwrap_or(None);

        // Read ports_attributes from container label
        let (ports_attrs, other_ports_attrs) = container
            .labels
            .get("dev.cella.ports_attributes")
            .map(|label| {
                cella_orchestrator::config_map::ports::deserialize_ports_attributes_label(label)
            })
            .unwrap_or_default();

        let shutdown_action = container.labels.get("dev.cella.shutdown_action").cloned();

        let req = ManagementRequest::RegisterContainer(Box::new(
            cella_protocol::ContainerRegistrationData {
                container_id: container.id.clone(),
                container_name: container.name.clone(),
                container_ip,
                ports_attributes: ports_attrs,
                other_ports_attributes: other_ports_attrs,
                forward_ports: vec![],
                shutdown_action,
                backend_kind: container.labels.get(cella_backend::BACKEND_LABEL).cloned(),
                docker_host: std::env::var("DOCKER_HOST").ok(),
            },
        ));

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
}
