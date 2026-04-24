//! Environment forwarding orchestration for cella dev containers.
//!
//! Detects host SSH agent, git config, and credential state,
//! then produces mounts, env vars, and post-start injection commands
//! for the container.

pub mod ai_keys;
pub mod ca_bundle;
pub mod claude_code;
pub mod codex;
mod error;
pub mod gemini;
pub mod gh_credential;
pub mod git_config;
pub mod nvim;
pub mod paths;
pub mod platform;
pub mod proxy;
pub mod ssh_agent;
pub mod ssh_config;
pub mod tmux;
pub mod user_env_probe;

pub use error::CellaEnvError;
pub use git_config::GitConfigEntry;
pub use platform::DockerRuntime;

/// In-container path of the cella-agent proxy config file.
///
/// Uploaded post-start by the orchestrator (contains the MITM CA private key,
/// so it must not live in an image layer or env var) and read by `cella-agent`
/// at daemon startup via the `CELLA_PROXY_CONFIG` env var.
pub const PROXY_CONFIG_PATH: &str = "/tmp/.cella/proxy-config.json";

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

/// Pending request for the orchestrator to materialize a daemon-managed
/// SSH-agent proxy on colima.
///
/// Returned via `EnvForwarding` because `cella-env` (Tier 3) must not
/// depend on the daemon RPC client; the orchestrator (Tier 2) inspects
/// the request, calls into the daemon, receives the host-side proxy
/// socket path, and appends the resulting mount + env entries itself.
#[derive(Debug, Clone)]
pub struct SshAgentProxyRequest {
    /// Upstream socket the proxy should bridge to — usually the host's
    /// `$SSH_AUTH_SOCK` (e.g. 1Password's sandboxed agent socket).
    pub upstream_socket: std::path::PathBuf,
    /// Container-side path the orchestrator will use as the bind-mount
    /// target (and as the value of `SSH_AUTH_SOCK` inside the container).
    pub mount_target: String,
    /// `SSH_AUTH_SOCK` value to set in the container — usually identical
    /// to `mount_target`.
    pub env_value: String,
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
    /// On colima only: deferred SSH-agent proxy request the orchestrator
    /// must resolve via the daemon before mounting. `None` for runtimes
    /// where the SSH agent is forwarded directly via host bind-mount or
    /// the magic VM socket (already pushed into `mounts` and `env`).
    pub ssh_agent_proxy_request: Option<SshAgentProxyRequest>,
}

/// Post-start injection commands and files.
#[derive(Debug, Clone, Default)]
pub struct PostStartInjection {
    /// Files to upload into the container (SSH config, credential helper).
    pub file_uploads: Vec<FileUpload>,
    /// Git config commands to execute inside the container as `remote_user`.
    /// Each entry is a full command (e.g., `["git", "config", "--global", "user.name", "John"]`).
    pub git_config_commands: Vec<Vec<String>>,
    /// Commands that require root privileges (e.g., CA trust store updates).
    /// Executed as root after file uploads.
    pub root_commands: Vec<Vec<String>>,
}

/// Apply SSH agent forwarding to the environment.
///
/// For most runtimes this synchronously appends the mount and env entries.
/// For colima, the daemon must materialize a proxy socket first — we set
/// `ssh_agent_proxy_request` and let the orchestrator resolve it.
fn apply_ssh_agent_forwarding(
    fwd: &mut EnvForwarding,
    runtime: &DockerRuntime,
    config: &serde_json::Value,
) {
    match ssh_agent::ssh_agent_request(runtime, config) {
        Some(ssh_agent::SshAgentRequest::Direct(ssh)) => {
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
        Some(ssh_agent::SshAgentRequest::ProxyOnColima {
            upstream_socket,
            mount_target,
            env_value,
        }) => {
            tracing::info!(
                "SSH agent forwarding (colima proxy): bridging {} via daemon",
                upstream_socket.display()
            );
            fwd.ssh_agent_proxy_request = Some(SshAgentProxyRequest {
                upstream_socket,
                mount_target,
                env_value,
            });
        }
        None => {}
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

/// Mark all directories as safe for git inside the container.
///
/// Matches the official devcontainer CLI behavior: sets
/// `safe.directory = *` so git operations succeed regardless of file
/// ownership (common after UID remapping or bind-mount mismatches).
fn apply_safe_directory(fwd: &mut EnvForwarding) {
    fwd.post_start.git_config_commands.push(vec![
        "git".to_string(),
        "config".to_string(),
        "--global".to_string(),
        "--replace-all".to_string(),
        "safe.directory".to_string(),
        "*".to_string(),
    ]);
}

/// Apply credential forwarding via the agent-based credential helper.
///
/// Always installs the git credential helper pointing to the agent binary.
/// The in-container agent handles transport to the host daemon automatically.
fn apply_credential_forwarding(fwd: &mut EnvForwarding) {
    fwd.post_start.git_config_commands.push(vec![
        "git".to_string(),
        "config".to_string(),
        "--global".to_string(),
        "credential.helper".to_string(),
        "/cella/bin/cella-agent credential".to_string(),
    ]);
}

/// Network configuration for proxy forwarding.
pub struct ProxyForwardingConfig {
    /// Proxy configuration from cella settings.
    pub proxy: cella_network::config::ProxyConfig,
    /// Whether blocking rules are active (determines if proxy env vars
    /// point to cella-agent proxy or directly to upstream).
    pub has_blocking_rules: bool,
    /// Full network config (needed to build agent proxy config JSON).
    pub full_config: Option<cella_network::config::NetworkConfig>,
    /// Detected container distro (for CA trust store paths).
    pub container_distro: ca_bundle::ContainerDistro,
}

/// Prepare environment forwarding for a container.
///
/// Detects the Docker runtime, probes host SSH agent and git config,
/// and assembles the forwarding configuration.
///
/// Never fails — individual features log warnings and are skipped
/// on error, per the design principle of never failing `cella up`.
pub fn prepare_env_forwarding(
    config: &serde_json::Value,
    remote_user: &str,
    network: Option<&ProxyForwardingConfig>,
) -> EnvForwarding {
    let runtime = platform::detect_runtime();
    tracing::debug!("Detected Docker runtime: {runtime:?}");

    let mut fwd = EnvForwarding::default();

    apply_ssh_agent_forwarding(&mut fwd, &runtime, config);
    apply_ssh_config_files(&mut fwd, remote_user);
    apply_git_config(&mut fwd);
    apply_safe_directory(&mut fwd);
    apply_credential_forwarding(&mut fwd);

    if let Some(net_config) = network {
        proxy::apply_proxy_env(&mut fwd, &net_config.proxy, net_config.has_blocking_rules);

        // If blocking rules are active, pass the proxy config to cella-agent
        // via a file with restrictive permissions (contains CA private key).
        if net_config.has_blocking_rules
            && let Some(ref net_full) = net_config.full_config
        {
            let json = proxy::build_agent_proxy_config_json(net_full);
            fwd.post_start.file_uploads.push(FileUpload {
                container_path: PROXY_CONFIG_PATH.to_string(),
                content: json.into_bytes(),
                mode: 0o600,
            });
            fwd.env.push(ForwardEnv {
                key: "CELLA_PROXY_CONFIG".to_string(),
                value: PROXY_CONFIG_PATH.to_string(),
            });
        }

        // Inject host CA bundle into the container so TLS works behind
        // corporate proxies.
        let distro = &net_config.container_distro;
        let additional_ca = net_config.proxy.ca_cert.as_deref();
        if let Some(ca_injection) = ca_bundle::prepare_ca_injection(additional_ca) {
            ca_injection.apply_to(&mut fwd.post_start, distro);
        }

        // If MITM CA was generated (for path-level blocking), also inject it
        // so the container trusts cella's intercepted certificates.
        if let Some(ref net_full) = net_config.full_config {
            let has_path_rules = net_full.rules.iter().any(|r| !r.paths.is_empty());
            if has_path_rules && let Ok(ca) = cella_network::ca::ensure_ca() {
                let mitm_path = distro.ca_cert_path("cella-mitm-ca.crt");
                let mitm_upload = FileUpload {
                    container_path: mitm_path,
                    content: ca.cert_pem.clone().into_bytes(),
                    mode: 0o644,
                };
                fwd.post_start.file_uploads.push(mitm_upload);

                // For unknown distro, also upload to RHEL path.
                if *distro == ca_bundle::ContainerDistro::Unknown {
                    fwd.post_start.file_uploads.push(FileUpload {
                        container_path: "/etc/pki/ca-trust/source/anchors/cella-mitm-ca.crt"
                            .to_string(),
                        content: ca.cert_pem.into_bytes(),
                        mode: 0o644,
                    });
                }

                // Always refresh trust store after MITM CA upload,
                // even when host CA injection was None.
                fwd.post_start
                    .root_commands
                    .push(distro.trust_store_update_command());
            }
        }
    }

    fwd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_credential_forwarding() {
        let mut fwd = EnvForwarding::default();
        apply_credential_forwarding(&mut fwd);

        assert_eq!(
            fwd.post_start.git_config_commands.len(),
            1,
            "should add exactly one git config command"
        );

        let cmd = &fwd.post_start.git_config_commands[0];
        assert!(
            cmd.iter().any(|s| s == "credential.helper"),
            "command should set credential.helper"
        );
        assert!(
            cmd.iter()
                .any(|s| s.contains("/cella/bin/cella-agent credential")),
            "command should point to cella-agent credential helper"
        );
    }

    #[test]
    fn test_env_forwarding_default() {
        let fwd = EnvForwarding::default();
        assert!(fwd.mounts.is_empty(), "default mounts should be empty");
        assert!(fwd.env.is_empty(), "default env should be empty");
        assert!(
            fwd.post_start.file_uploads.is_empty(),
            "default file_uploads should be empty"
        );
        assert!(
            fwd.post_start.git_config_commands.is_empty(),
            "default git_config_commands should be empty"
        );
        assert!(
            fwd.post_start.root_commands.is_empty(),
            "default root_commands should be empty"
        );
    }

    #[test]
    fn test_apply_safe_directory() {
        let mut fwd = EnvForwarding::default();
        apply_safe_directory(&mut fwd);

        assert_eq!(
            fwd.post_start.git_config_commands.len(),
            1,
            "should add exactly one git config command"
        );

        let cmd = &fwd.post_start.git_config_commands[0];
        assert!(
            cmd.iter().any(|s| s == "safe.directory"),
            "command should set safe.directory"
        );
        assert!(
            cmd.iter().any(|s| s == "*"),
            "safe.directory should be set to wildcard"
        );
        assert!(
            cmd.iter().any(|s| s == "--replace-all"),
            "should use --replace-all flag"
        );
    }

    #[test]
    fn test_prepare_env_forwarding_minimal() {
        let config: serde_json::Value = serde_json::from_str("{}").unwrap();
        let fwd = prepare_env_forwarding(&config, "root", None);

        // Credential forwarding is always added regardless of other config.
        let has_credential_helper = fwd
            .post_start
            .git_config_commands
            .iter()
            .any(|cmd| cmd.iter().any(|s| s == "credential.helper"));
        assert!(
            has_credential_helper,
            "credential helper should always be present"
        );

        // safe.directory wildcard is always set for container safety.
        let has_safe_directory = fwd
            .post_start
            .git_config_commands
            .iter()
            .any(|cmd| cmd.iter().any(|s| s == "safe.directory"));
        assert!(
            has_safe_directory,
            "safe.directory should always be present"
        );
    }

    #[test]
    fn test_proxy_forwarding_config_fields() {
        let proxy = cella_network::config::ProxyConfig::default();
        let net_config = cella_network::config::NetworkConfig::default();

        let pfc = ProxyForwardingConfig {
            proxy,
            has_blocking_rules: true,
            full_config: Some(net_config),
            container_distro: ca_bundle::ContainerDistro::Debian,
        };

        assert!(pfc.has_blocking_rules);
        assert!(pfc.proxy.enabled);
        assert!(pfc.full_config.is_some());
        assert_eq!(pfc.container_distro, ca_bundle::ContainerDistro::Debian);
    }
}
