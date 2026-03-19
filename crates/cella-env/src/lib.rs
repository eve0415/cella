//! Environment forwarding orchestration for cella dev containers.
//!
//! Detects host SSH agent, git config, and credential proxy state,
//! then produces mounts, env vars, and post-start injection commands
//! for the container.

mod error;
pub mod git_config;
pub mod git_credential;
pub mod platform;
pub mod ssh_agent;
pub mod ssh_config;
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
    /// Whether the container needs tunnel-based forwarding (`OrbStack`, Colima, Unknown).
    pub needs_tunnel: bool,
}

/// Post-start injection commands and files.
#[derive(Debug, Clone, Default)]
pub struct PostStartInjection {
    /// Files to upload into the container (SSH config, credential helper).
    pub file_uploads: Vec<FileUpload>,
    /// Git config commands to execute inside the container.
    /// Each entry is a full command (e.g., `["git", "config", "--global", "user.name", "John"]`).
    pub git_config_commands: Vec<Vec<String>>,
    /// Credential helper script to install (if credential proxy is running).
    pub credential_helper: Option<FileUpload>,
}

/// Prepare environment forwarding for a container.
///
/// Detects the Docker runtime, probes host SSH agent and git config,
/// checks for the credential proxy daemon, and assembles the forwarding
/// configuration.
///
/// Never fails — individual features log warnings and are skipped
/// on error, per the design principle of never failing `cella up`.
pub fn prepare_env_forwarding(config: &serde_json::Value, remote_user: &str) -> EnvForwarding {
    let runtime = platform::detect_runtime();
    tracing::debug!("Detected Docker runtime: {runtime:?}");

    let mut fwd = EnvForwarding::default();

    // SSH agent forwarding
    if let Some(ssh) = ssh_agent::ssh_agent_forwarding(&runtime, config) {
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

    // SSH config files (known_hosts, config)
    let ssh_files = ssh_config::read_ssh_config_files(remote_user);
    if !ssh_files.is_empty() {
        tracing::info!(
            "Copying {} SSH config file(s) to container",
            ssh_files.len()
        );
        fwd.post_start.file_uploads.extend(ssh_files);
    }

    // Host git config (safe subset)
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

    // Credential proxy forwarding
    if ssh_agent::needs_tunnel(&runtime) {
        // Tunnel runtimes: SSH and credential forwarding handled by the tunnel daemon.
        // Sockets are created inside the container by cella-tunnel-server.
        tracing::info!("Using tunnel-based forwarding for SSH and credentials");
        fwd.needs_tunnel = true;

        // Set SSH_AUTH_SOCK to tunnel-server's socket path (no mount needed)
        if std::env::var("SSH_AUTH_SOCK")
            .ok()
            .is_some_and(|s| !s.is_empty())
        {
            fwd.env.push(ForwardEnv {
                key: "SSH_AUTH_SOCK".to_string(),
                value: "/tmp/cella-ssh-agent.sock".to_string(),
            });
        }

        // Set credential socket env var (no mount needed)
        fwd.env.push(ForwardEnv {
            key: "CELLA_CREDENTIAL_SOCKET".to_string(),
            value: "/tmp/cella-credential-proxy.sock".to_string(),
        });

        // Configure git to use the tunnel-server binary as credential helper
        fwd.post_start
            .git_config_commands
            .extend(git_credential::tunnel_credential_helper_commands());
    } else if let Some(cred) = git_credential::credential_forwarding() {
        // Bind-mount runtimes: mount the credential proxy socket directly.
        tracing::info!(
            "Credential proxy forwarding: {} -> {}",
            cred.mount_source,
            cred.mount_target
        );
        fwd.mounts.push(ForwardMount {
            source: cred.mount_source,
            target: cred.mount_target,
        });
        fwd.env.push(ForwardEnv {
            key: "CELLA_CREDENTIAL_SOCKET".to_string(),
            value: cred.env_value,
        });
        fwd.post_start.credential_helper =
            Some(git_credential::credential_helper_script(remote_user));

        // Configure git to use the credential helper
        fwd.post_start.git_config_commands.push(vec![
            "git".to_string(),
            "config".to_string(),
            "--global".to_string(),
            "credential.helper".to_string(),
            "/usr/local/bin/cella-git-credential-helper".to_string(),
        ]);
    }

    fwd
}
