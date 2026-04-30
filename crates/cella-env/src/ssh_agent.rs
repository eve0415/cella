//! SSH agent socket detection and mount generation.

use std::path::PathBuf;

use tracing::warn;

use crate::platform::DockerRuntime;

/// SSH agent forwarding configuration.
#[derive(Debug, Clone)]
pub struct SshAgentForwarding {
    /// Host-side socket path or Docker-internal path to mount from.
    pub mount_source: String,
    /// Container-side socket path to mount to.
    pub mount_target: String,
    /// Value for `SSH_AUTH_SOCK` inside the container.
    pub env_value: String,
}

/// What `cella-env` can determine about SSH agent forwarding without
/// daemon access. The orchestrator (Tier 2) translates `ProxyOnColima`
/// into a real mount by calling into the daemon.
#[derive(Debug, Clone)]
pub enum SshAgentRequest {
    /// Direct mount of an existing socket — used for Docker Desktop and
    /// `OrbStack` (magic VM socket) and for Linux/Podman direct host
    /// bind-mount. Ready to be mounted as-is.
    Direct(SshAgentForwarding),
    /// **Known broken on colima — see `cella_daemon::ssh_proxy` module
    /// docs.** Lima's OpenSSH-protocol `forwardAgent` degenerates with
    /// sandboxed agents (1Password) AND direct bind-mount of
    /// `~/Library/Group Containers/.../agent.sock` fails at docker
    /// `mkdir <source>: operation not supported`. The daemon-managed
    /// proxy was meant to fix this by mounting a socket under
    /// `~/.cella/run/` instead — but virtiofs rejects mkdir for that
    /// path too (Phase 1 probe failed). The orchestrator currently
    /// requests this variant, the daemon RPC succeeds, and the bind
    /// mount then fails at container create. Affected users should
    /// switch to `OrbStack` or Docker Desktop until a VM-side helper
    /// design lands.
    ProxyOnColima {
        upstream_socket: PathBuf,
        mount_target: String,
        env_value: String,
    },
}

/// The VM-side magic SSH agent socket path that Docker Desktop, `OrbStack`,
/// and colima (when `colima start --ssh-agent` is set) all expose inside
/// their VMs as a forwarded host agent. Lima creates this path as a symlink
/// to the host agent when `ssh.forwardAgent: true`; colima enables that
/// Lima option via its own `forwardAgent` config flag.
const VM_HOST_SERVICES_SSH_SOCK: &str = "/run/host-services/ssh-auth.sock";

/// Container-side socket path for direct bind-mount forwarding.
const CONTAINER_SSH_SOCK: &str = "/tmp/cella-ssh-agent.sock";

/// SSH agent forwarding for runtimes whose VM exposes the host agent at the
/// Docker Desktop / Lima magic path `/run/host-services/ssh-auth.sock`.
/// Used for Docker Desktop and `OrbStack`. Colima goes through the
/// daemon-managed proxy via `colima_proxy_request` instead.
fn vm_host_services_ssh_forwarding(
    runtime: &DockerRuntime,
    host_socket: Option<&String>,
) -> SshAgentForwarding {
    if host_socket.is_none() {
        warn!(
            "SSH_AUTH_SOCK not set on host, but {runtime} may still provide the SSH agent via /run/host-services/ssh-auth.sock"
        );
    }
    SshAgentForwarding {
        mount_source: VM_HOST_SERVICES_SSH_SOCK.to_string(),
        mount_target: VM_HOST_SERVICES_SSH_SOCK.to_string(),
        env_value: VM_HOST_SERVICES_SSH_SOCK.to_string(),
    }
}

/// SSH agent forwarding via direct bind-mount of the host socket.
///
/// Used as the fallback in `ssh_agent_request` for runtimes that aren't
/// `DockerDesktop` / `OrbStack` / `Colima` — typically `LinuxNative`
/// Docker, Podman, or Rancher Desktop. macOS sandbox-dir concerns
/// don't apply on these runtimes (no Lima virtiofs in the path), so
/// the host socket is reachable from the docker daemon as-is. (For
/// `Colima` direct mount fails — Docker's mkdir-source-if-missing
/// returns EOPNOTSUPP under macOS sandbox dirs — which is why colima
/// takes the daemon-managed proxy path instead.)
fn direct_ssh_forwarding(
    _runtime: &DockerRuntime,
    host_socket: Option<String>,
) -> Option<SshAgentForwarding> {
    let host_socket = host_socket?;

    if !std::path::Path::new(&host_socket).exists() {
        warn!(
            "SSH_AUTH_SOCK points to {host_socket} which does not exist, skipping SSH agent forwarding"
        );
        return None;
    }

    Some(SshAgentForwarding {
        mount_source: host_socket,
        mount_target: CONTAINER_SSH_SOCK.to_string(),
        env_value: CONTAINER_SSH_SOCK.to_string(),
    })
}

/// Build the ordered list of SSH agent strategies for a given runtime.
///
/// Separated from env/config concerns for testability. Each runtime gets
/// strategies in preference order — the orchestrator tries them in sequence,
/// falling back on bind-mount failures.
fn ssh_agent_strategies_for_runtime(
    runtime: &DockerRuntime,
    host_socket: Option<String>,
) -> Vec<SshAgentRequest> {
    match runtime {
        DockerRuntime::DockerDesktop | DockerRuntime::OrbStack => {
            vec![SshAgentRequest::Direct(vm_host_services_ssh_forwarding(
                runtime,
                host_socket.as_ref(),
            ))]
        }
        DockerRuntime::Colima => colima_proxy_request(host_socket.as_deref())
            .into_iter()
            .collect(),
        DockerRuntime::LinuxNative => direct_ssh_forwarding(runtime, host_socket)
            .map(SshAgentRequest::Direct)
            .into_iter()
            .collect(),
        // VM-based or unknown: try magic socket first, then direct mount
        DockerRuntime::RancherDesktop | DockerRuntime::Podman | DockerRuntime::Unknown => {
            let mut strategies = vec![SshAgentRequest::Direct(vm_host_services_ssh_forwarding(
                runtime,
                host_socket.as_ref(),
            ))];
            if let Some(direct) = direct_ssh_forwarding(runtime, host_socket) {
                strategies.push(SshAgentRequest::Direct(direct));
            }
            strategies
        }
    }
}

/// Detect the host SSH agent socket and return ordered strategies to try.
///
/// Returns an empty vec if:
/// - The user has already configured `SSH_AUTH_SOCK` in `containerEnv`/`remoteEnv`
/// - The user has a mount targeting the SSH socket path
/// - No strategies are available for this runtime
pub fn ssh_agent_request(
    runtime: &DockerRuntime,
    config: &serde_json::Value,
) -> Vec<SshAgentRequest> {
    if has_user_ssh_override(config) {
        tracing::debug!("User has SSH_AUTH_SOCK override in config, skipping auto-forward");
        return Vec::new();
    }

    let host_socket = std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|s| !s.is_empty());

    ssh_agent_strategies_for_runtime(runtime, host_socket)
}

/// On colima, defer SSH-agent forwarding to a daemon-managed host-side
/// proxy. **The proxy itself is currently broken on colima — see
/// `cella_daemon::ssh_proxy` module docs for the empirical evidence.**
/// Two reasons direct mount cannot be used:
///
/// 1. Lima's OpenSSH-protocol `forwardAgent` mechanism silently
///    degenerates with sandboxed agents (1Password) and routes
///    `/run/host-services/ssh-auth.sock` to a connectable-but-empty
///    agent. Confirmed by side-by-side tests against VS Code.
/// 2. Bind-mounting the host's `$SSH_AUTH_SOCK` directly fails when
///    that path is in a macOS sandbox dir: docker daemon precreates
///    a missing mount source via mkdir, which virtiofs rejects with
///    `operation not supported` for paths under `~/Library/Group
///    Containers/`.
///
/// We routed colima through a daemon-managed proxy under `~/.cella/run/`
/// expecting that to sidestep both issues. It does not — the same
/// `mkdir <source>: operation not supported` error fires for the
/// `~/.cella/run/` socket too. Virtiofs rejects mkdir for any Unix
/// socket path created on the macOS host, not just sandboxed ones.
/// VS Code works because its `/tmp/vscode-ssh-auth-<uuid>.sock` is
/// created INSIDE the colima VM (via Remote-SSH), not on the host.
///
/// Returns `None` when `SSH_AUTH_SOCK` is unset on the host — the proxy
/// has nothing to bridge to, and the orchestrator should skip forwarding
/// rather than mount a dead socket. (Even when `Some(...)` is returned,
/// the resulting mount will currently fail at docker container create
/// time on colima.)
fn colima_proxy_request(host_socket: Option<&str>) -> Option<SshAgentRequest> {
    let upstream = host_socket?;
    Some(SshAgentRequest::ProxyOnColima {
        upstream_socket: PathBuf::from(upstream),
        mount_target: VM_HOST_SERVICES_SSH_SOCK.to_string(),
        env_value: VM_HOST_SERVICES_SSH_SOCK.to_string(),
    })
}

/// Check if a Docker error is a bind-mount failure for the SSH agent socket.
pub fn is_ssh_mount_error(error_msg: &str, ssh_mount_source: Option<&str>) -> bool {
    let Some(source) = ssh_mount_source else {
        return false;
    };
    error_msg.contains("bind source path does not exist") && error_msg.contains(source)
}

/// Actionable warning message when SSH agent forwarding is skipped after
/// all strategies fail.
pub fn ssh_skip_warning(runtime: &DockerRuntime) -> String {
    let base = "SSH agent forwarding skipped — git operations requiring SSH keys may not work inside the container.";
    let hint = match runtime {
        DockerRuntime::RancherDesktop => {
            " To enable, create ~/Library/Application Support/rancher-desktop/lima/_config/override.yaml with:\n  ssh:\n    forwardAgent: true\n  Then restart Rancher Desktop."
        }
        DockerRuntime::Colima => {
            " Try `colima start --ssh-agent` or add `forwardAgent: true` to ~/.colima/default/colima.yaml."
        }
        DockerRuntime::Podman => {
            " Podman Machine may not forward the SSH agent by default. Check podman-machine configuration."
        }
        DockerRuntime::Unknown => {
            " If using a VM-based Docker runtime, ensure SSH agent forwarding is enabled in the VM configuration."
        }
        DockerRuntime::DockerDesktop | DockerRuntime::OrbStack | DockerRuntime::LinuxNative => "",
    };
    format!("{base}{hint}")
}

/// Check if the user has already configured SSH agent forwarding in their config.
fn has_user_ssh_override(config: &serde_json::Value) -> bool {
    // Check containerEnv for SSH_AUTH_SOCK
    if config
        .get("containerEnv")
        .and_then(|v| v.as_object())
        .is_some_and(|env| env.contains_key("SSH_AUTH_SOCK"))
    {
        return true;
    }

    // Check remoteEnv for SSH_AUTH_SOCK
    if config
        .get("remoteEnv")
        .and_then(|v| v.as_object())
        .is_some_and(|env| env.contains_key("SSH_AUTH_SOCK"))
    {
        return true;
    }

    // Check mounts for SSH socket targets
    if let Some(mounts) = config.get("mounts").and_then(|v| v.as_array()) {
        for m in mounts {
            let target = match m {
                serde_json::Value::String(s) => extract_mount_target(s),
                serde_json::Value::Object(obj) => {
                    obj.get("target").and_then(|v| v.as_str()).map(String::from)
                }
                _ => None,
            };
            if let Some(t) = target
                && (t.contains("ssh-auth") || t.contains("ssh_auth") || t.contains("SSH_AUTH"))
            {
                return true;
            }
        }
    }

    false
}

/// Extract the target path from a mount string (e.g., "type=bind,source=/a,target=/b").
fn extract_mount_target(mount_str: &str) -> Option<String> {
    mount_str.split(',').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        if matches!(key.trim(), "target" | "dst" | "destination") {
            Some(value.to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_override_container_env() {
        let config = json!({
            "containerEnv": {"SSH_AUTH_SOCK": "/custom/socket"}
        });
        assert!(has_user_ssh_override(&config));
    }

    #[test]
    fn user_override_remote_env() {
        let config = json!({
            "remoteEnv": {"SSH_AUTH_SOCK": "/custom/socket"}
        });
        assert!(has_user_ssh_override(&config));
    }

    #[test]
    fn user_override_mount_string() {
        let config = json!({
            "mounts": ["type=bind,source=/host/ssh-auth.sock,target=/container/ssh-auth.sock"]
        });
        assert!(has_user_ssh_override(&config));
    }

    #[test]
    fn user_override_mount_object() {
        let config = json!({
            "mounts": [{"type": "bind", "source": "/host/sock", "target": "/run/ssh_auth_sock"}]
        });
        assert!(has_user_ssh_override(&config));
    }

    #[test]
    fn no_override_empty_config() {
        let config = json!({});
        assert!(!has_user_ssh_override(&config));
    }

    #[test]
    fn no_override_unrelated_env() {
        let config = json!({
            "containerEnv": {"FOO": "bar"},
            "remoteEnv": {"BAZ": "qux"}
        });
        assert!(!has_user_ssh_override(&config));
    }

    #[test]
    fn extract_target_from_mount_string() {
        assert_eq!(
            extract_mount_target("type=bind,source=/a,target=/b"),
            Some("/b".to_string())
        );
        assert_eq!(
            extract_mount_target("source=/a,dst=/b"),
            Some("/b".to_string())
        );
        assert_eq!(extract_mount_target("source=/a"), None);
    }

    #[test]
    fn vm_host_services_ssh_forwarding_returns_magic_path() {
        let fwd = vm_host_services_ssh_forwarding(&DockerRuntime::DockerDesktop, None);
        assert_eq!(fwd.mount_source, "/run/host-services/ssh-auth.sock");
        assert_eq!(fwd.mount_target, "/run/host-services/ssh-auth.sock");
        assert_eq!(fwd.env_value, "/run/host-services/ssh-auth.sock");
    }

    #[test]
    fn vm_host_services_ssh_forwarding_with_socket_ignores_it() {
        let host = "/tmp/ssh.sock".to_string();
        let fwd = vm_host_services_ssh_forwarding(&DockerRuntime::OrbStack, Some(&host));
        assert_eq!(fwd.mount_source, "/run/host-services/ssh-auth.sock");
        assert_eq!(fwd.mount_target, "/run/host-services/ssh-auth.sock");
        assert_eq!(fwd.env_value, "/run/host-services/ssh-auth.sock");
    }

    // -------------------------------------------------------------
    // ssh_agent_request dispatcher: colima → proxy, others → direct
    // -------------------------------------------------------------

    #[test]
    fn colima_proxy_request_returns_request_when_socket_set() {
        let req = colima_proxy_request(Some("/host/agent.sock"));
        match req {
            Some(SshAgentRequest::ProxyOnColima {
                upstream_socket,
                mount_target,
                env_value,
            }) => {
                assert_eq!(upstream_socket, PathBuf::from("/host/agent.sock"));
                assert_eq!(mount_target, "/run/host-services/ssh-auth.sock");
                assert_eq!(env_value, "/run/host-services/ssh-auth.sock");
            }
            other => panic!("expected ProxyOnColima, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn colima_proxy_request_returns_none_when_socket_unset() {
        // Proxy can't bridge to nothing — orchestrator must skip forwarding
        // entirely rather than hand the container a dead socket.
        assert!(colima_proxy_request(None).is_none());
    }

    #[test]
    fn ssh_agent_request_user_override_skips_forward() {
        let config = json!({"containerEnv": {"SSH_AUTH_SOCK": "/custom/socket"}});
        assert!(ssh_agent_request(&DockerRuntime::Colima, &config).is_empty());
        assert!(ssh_agent_request(&DockerRuntime::DockerDesktop, &config).is_empty());
        assert!(ssh_agent_request(&DockerRuntime::OrbStack, &config).is_empty());
    }

    #[test]
    fn ssh_agent_request_orbstack_returns_direct_magic_socket() {
        let config = json!({});
        let strategies = ssh_agent_request(&DockerRuntime::OrbStack, &config);
        assert!(!strategies.is_empty());
        match &strategies[0] {
            SshAgentRequest::Direct(fwd) => {
                assert_eq!(fwd.mount_source, "/run/host-services/ssh-auth.sock");
                assert_eq!(fwd.env_value, "/run/host-services/ssh-auth.sock");
            }
            SshAgentRequest::ProxyOnColima { .. } => {
                panic!("expected Direct on OrbStack, got ProxyOnColima")
            }
        }
    }

    #[test]
    fn ssh_agent_request_docker_desktop_returns_direct_magic_socket() {
        let config = json!({});
        let strategies = ssh_agent_request(&DockerRuntime::DockerDesktop, &config);
        assert!(!strategies.is_empty());
        assert!(matches!(&strategies[0], SshAgentRequest::Direct(_)));
    }

    #[test]
    fn test_direct_ssh_forwarding_none_returns_none() {
        assert!(direct_ssh_forwarding(&DockerRuntime::LinuxNative, None).is_none());
    }

    #[test]
    fn test_direct_ssh_forwarding_nonexistent_returns_none() {
        assert!(
            direct_ssh_forwarding(
                &DockerRuntime::LinuxNative,
                Some("/nonexistent/path/to/ssh.sock".to_string())
            )
            .is_none()
        );
    }

    #[test]
    fn test_extract_mount_target_destination_key() {
        assert_eq!(
            extract_mount_target("source=/a,destination=/b"),
            Some("/b".to_string())
        );
    }

    #[test]
    fn test_extract_mount_target_no_match() {
        assert_eq!(extract_mount_target("type=bind,source=/a"), None);
    }

    #[test]
    fn test_no_override_empty_mounts() {
        let config = json!({
            "mounts": []
        });
        assert!(!has_user_ssh_override(&config));
    }

    #[test]
    fn direct_ssh_forwarding_passes_through_existing_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ssh.sock");
        std::fs::File::create(&sock).unwrap();

        let result = direct_ssh_forwarding(
            &DockerRuntime::LinuxNative,
            Some(sock.to_string_lossy().into_owned()),
        );
        let fwd = result.expect("existing socket path should mount");
        assert_eq!(fwd.mount_source, sock.to_string_lossy());
        assert_eq!(fwd.mount_target, CONTAINER_SSH_SOCK);
        assert_eq!(fwd.env_value, CONTAINER_SSH_SOCK);
    }

    // -----------------------------------------------------------------
    // ssh_agent_strategies_for_runtime: ordered strategy tests
    // -----------------------------------------------------------------

    #[test]
    fn strategies_rancher_desktop_returns_vm_then_direct() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ssh.sock");
        std::fs::File::create(&sock).unwrap();
        let host_socket = Some(sock.to_string_lossy().into_owned());

        let strategies =
            ssh_agent_strategies_for_runtime(&DockerRuntime::RancherDesktop, host_socket);
        assert_eq!(strategies.len(), 2);
        match &strategies[0] {
            SshAgentRequest::Direct(fwd) => {
                assert_eq!(fwd.mount_source, VM_HOST_SERVICES_SSH_SOCK);
            }
            SshAgentRequest::ProxyOnColima { .. } => {
                panic!("first strategy should be vm_host_services, got ProxyOnColima")
            }
        }
        match &strategies[1] {
            SshAgentRequest::Direct(fwd) => {
                assert_eq!(fwd.mount_source, sock.to_string_lossy());
            }
            SshAgentRequest::ProxyOnColima { .. } => {
                panic!("second strategy should be direct mount, got ProxyOnColima")
            }
        }
    }

    #[test]
    fn strategies_podman_returns_vm_then_direct() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ssh.sock");
        std::fs::File::create(&sock).unwrap();
        let host_socket = Some(sock.to_string_lossy().into_owned());

        let strategies = ssh_agent_strategies_for_runtime(&DockerRuntime::Podman, host_socket);
        assert_eq!(strategies.len(), 2);
    }

    #[test]
    fn strategies_unknown_returns_vm_then_direct() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ssh.sock");
        std::fs::File::create(&sock).unwrap();
        let host_socket = Some(sock.to_string_lossy().into_owned());

        let strategies = ssh_agent_strategies_for_runtime(&DockerRuntime::Unknown, host_socket);
        assert_eq!(strategies.len(), 2);
    }

    #[test]
    fn strategies_linux_native_returns_direct_only() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ssh.sock");
        std::fs::File::create(&sock).unwrap();
        let host_socket = Some(sock.to_string_lossy().into_owned());

        let strategies = ssh_agent_strategies_for_runtime(&DockerRuntime::LinuxNative, host_socket);
        assert_eq!(strategies.len(), 1);
        match &strategies[0] {
            SshAgentRequest::Direct(fwd) => {
                assert_eq!(fwd.mount_source, sock.to_string_lossy());
            }
            SshAgentRequest::ProxyOnColima { .. } => {
                panic!("should be direct mount, got ProxyOnColima")
            }
        }
    }

    #[test]
    fn strategies_docker_desktop_returns_vm_only() {
        let strategies = ssh_agent_strategies_for_runtime(&DockerRuntime::DockerDesktop, None);
        assert_eq!(strategies.len(), 1);
        match &strategies[0] {
            SshAgentRequest::Direct(fwd) => {
                assert_eq!(fwd.mount_source, VM_HOST_SERVICES_SSH_SOCK);
            }
            SshAgentRequest::ProxyOnColima { .. } => {
                panic!("should be vm_host_services, got ProxyOnColima")
            }
        }
    }

    #[test]
    fn strategies_orbstack_returns_vm_only() {
        let strategies = ssh_agent_strategies_for_runtime(&DockerRuntime::OrbStack, None);
        assert_eq!(strategies.len(), 1);
        assert!(matches!(&strategies[0], SshAgentRequest::Direct(_)));
    }

    #[test]
    fn strategies_colima_returns_proxy() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ssh.sock");
        std::fs::File::create(&sock).unwrap();
        let host_socket = Some(sock.to_string_lossy().into_owned());

        let strategies = ssh_agent_strategies_for_runtime(&DockerRuntime::Colima, host_socket);
        assert_eq!(strategies.len(), 1);
        assert!(
            matches!(&strategies[0], SshAgentRequest::ProxyOnColima { .. }),
            "expected ProxyOnColima, got {:?}",
            strategies[0]
        );
    }

    #[test]
    fn strategies_no_host_socket_vm_runtimes_still_return_magic() {
        for runtime in [
            DockerRuntime::RancherDesktop,
            DockerRuntime::Podman,
            DockerRuntime::Unknown,
        ] {
            let strategies = ssh_agent_strategies_for_runtime(&runtime, None);
            assert_eq!(
                strategies.len(),
                1,
                "{runtime:?}: should have magic socket only (no direct without host socket)"
            );
        }
    }

    #[test]
    fn strategies_no_host_socket_linux_native_returns_empty() {
        let strategies = ssh_agent_strategies_for_runtime(&DockerRuntime::LinuxNative, None);
        assert!(strategies.is_empty());
    }

    // -----------------------------------------------------------------
    // Error detection and warning helpers
    // -----------------------------------------------------------------

    #[test]
    fn is_ssh_mount_error_matches_bind_source() {
        let msg = r#"Docker responded with status code 400: invalid mount config for type "bind": bind source path does not exist: /run/host-services/ssh-auth.sock"#;
        assert!(is_ssh_mount_error(
            msg,
            Some("/run/host-services/ssh-auth.sock")
        ));
    }

    #[test]
    fn is_ssh_mount_error_no_match_different_path() {
        let msg = r"bind source path does not exist: /some/other/path";
        assert!(!is_ssh_mount_error(
            msg,
            Some("/run/host-services/ssh-auth.sock")
        ));
    }

    #[test]
    fn is_ssh_mount_error_no_match_different_error() {
        let msg = "container name already in use";
        assert!(!is_ssh_mount_error(
            msg,
            Some("/run/host-services/ssh-auth.sock")
        ));
    }

    #[test]
    fn is_ssh_mount_error_none_source() {
        let msg = "bind source path does not exist: /foo";
        assert!(!is_ssh_mount_error(msg, None));
    }

    #[test]
    fn ssh_skip_warning_rancher_desktop_has_lima_hint() {
        let msg = ssh_skip_warning(&DockerRuntime::RancherDesktop);
        assert!(msg.contains("forwardAgent: true"));
        assert!(msg.contains("Rancher Desktop"));
    }

    #[test]
    fn ssh_skip_warning_linux_native_no_hint() {
        let msg = ssh_skip_warning(&DockerRuntime::LinuxNative);
        assert!(msg.contains("SSH agent forwarding skipped"));
        assert!(!msg.contains("forwardAgent"));
    }

    #[test]
    fn ssh_skip_warning_all_runtimes_have_base_message() {
        for runtime in [
            DockerRuntime::DockerDesktop,
            DockerRuntime::OrbStack,
            DockerRuntime::LinuxNative,
            DockerRuntime::Colima,
            DockerRuntime::Podman,
            DockerRuntime::RancherDesktop,
            DockerRuntime::Unknown,
        ] {
            let warning = ssh_skip_warning(&runtime);
            assert!(
                warning.contains("SSH agent forwarding skipped"),
                "{runtime:?} missing base warning"
            );
        }
    }
}
