//! Environment forwarding orchestration for cella dev containers.
//!
//! Detects host SSH agent, git config, and credential state,
//! then produces mounts, env vars, and post-start injection commands
//! for the container.

pub mod claude_code;
pub mod codex;
mod error;
pub mod gemini;
pub mod gh_credential;
pub mod git_config;
pub mod nvim;
pub mod paths;
pub mod platform;
pub mod ssh_agent;
pub mod ssh_config;
pub mod tmux;
pub mod user_env_probe;

pub use error::CellaEnvError;
pub use git_config::GitConfigEntry;
pub use platform::DockerRuntime;

/// A bind mount to add to the container at creation time.
#[derive(Debug, Clone)]
pub struct ForwardMount {
    pub source: String,
    pub target: String,
}

/// An environment variable to set in the container at creation time.
#[derive(Debug, Clone)]
pub struct ForwardEnv {
    pub key: String,
    pub value: String,
}

/// A file to upload into the container after start.
#[derive(Debug, Clone)]
pub struct FileUpload {
    /// Absolute path inside the container.
    pub container_path: String,
    /// File content.
    pub content: Vec<u8>,
    /// File permissions (octal, e.g., 0o600).
    pub mode: u32,
}

/// Result of preparing environment forwarding.
///
/// Split into two phases:
/// - Phase A: mounts and env vars set at container creation (immutable after create)
/// - Phase B: files and commands injected after start + UID remap
#[derive(Debug, Clone, Default)]
pub struct EnvForwarding {
    /// Bind mounts to add at container creation.
    pub mounts: Vec<ForwardMount>,
    /// Environment variables to set at container creation.
    pub env: Vec<ForwardEnv>,
    /// Post-start injection (after container start + UID remap).
    pub post_start: PostStartInjection,
}

/// Post-start injection commands and files.
#[derive(Debug, Clone, Default)]
pub struct PostStartInjection {
    /// Files to upload into the container (SSH config, credential helper).
    pub file_uploads: Vec<FileUpload>,
    /// Git config commands to execute inside the container.
    /// Each entry is a full command (e.g., `["git", "config", "--global", "user.name", "John"]`).
    pub git_config_commands: Vec<Vec<String>>,
}

/// Apply SSH agent forwarding to the environment.
fn apply_ssh_agent_forwarding(
    fwd: &mut EnvForwarding,
    runtime: &DockerRuntime,
    config: &serde_json::Value,
) {
    if let Some(ssh) = ssh_agent::ssh_agent_forwarding(runtime, config) {
        tracing::info!(
            "SSH agent forwarding: {} -> {}",
            ssh.mount_source,
            ssh.mount_target
        );
        fwd.mounts.push(ForwardMount {
            source: ssh.mount_source,
            target: ssh.mount_target,
        });
        fwd.env.push(ForwardEnv {
            key: "SSH_AUTH_SOCK".to_string(),
            value: ssh.env_value,
        });
    }
}

/// Apply SSH config file uploads to the environment.
fn apply_ssh_config_files(fwd: &mut EnvForwarding, remote_user: &str) {
    let ssh_files = ssh_config::read_ssh_config_files(remote_user);
    if !ssh_files.is_empty() {
        tracing::info!(
            "Copying {} SSH config file(s) to container",
            ssh_files.len()
        );
        fwd.post_start.file_uploads.extend(ssh_files);
    }
}

/// Apply host git config forwarding to the environment.
fn apply_git_config(fwd: &mut EnvForwarding) {
    let git_entries = git_config::read_host_git_config();
    if !git_entries.is_empty() {
        tracing::info!(
            "Forwarding {} git config entries to container",
            git_entries.len()
        );
        for entry in &git_entries {
            fwd.post_start.git_config_commands.push(vec![
                "git".to_string(),
                "config".to_string(),
                "--global".to_string(),
                entry.key.clone(),
                entry.value.clone(),
            ]);
        }
    }
}

/// Apply credential forwarding via the agent-based credential helper.
///
/// Always installs the git credential helper pointing to `/cella/bin/cella-credential`.
/// The in-container agent handles transport to the host daemon automatically.
fn apply_credential_forwarding(fwd: &mut EnvForwarding) {
    fwd.post_start.git_config_commands.push(vec![
        "git".to_string(),
        "config".to_string(),
        "--global".to_string(),
        "credential.helper".to_string(),
        "/cella/bin/cella-credential".to_string(),
    ]);
}

/// Prepare environment forwarding for a container.
///
/// Detects the Docker runtime, probes host SSH agent and git config,
/// and assembles the forwarding configuration.
///
/// Never fails — individual features log warnings and are skipped
/// on error, per the design principle of never failing `cella up`.
pub fn prepare_env_forwarding(config: &serde_json::Value, remote_user: &str) -> EnvForwarding {
    let runtime = platform::detect_runtime();
    tracing::debug!("Detected Docker runtime: {runtime:?}");

    let mut fwd = EnvForwarding::default();

    apply_ssh_agent_forwarding(&mut fwd, &runtime, config);
    apply_ssh_config_files(&mut fwd, remote_user);
    apply_git_config(&mut fwd);
    apply_credential_forwarding(&mut fwd);

    fwd
}
