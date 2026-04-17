//! SSH agent socket detection and mount generation.

use tracing::warn;

use crate::platform::DockerRuntime;

/// SSH agent forwarding configuration.
pub struct SshAgentForwarding {
    /// Host-side socket path or Docker-internal path to mount from.
    pub mount_source: String,
    /// Container-side socket path to mount to.
    pub mount_target: String,
    /// Value for `SSH_AUTH_SOCK` inside the container.
    pub env_value: String,
}

/// Docker Desktop's built-in SSH agent socket path (available inside the VM).
const DOCKER_DESKTOP_SSH_SOCK: &str = "/run/host-services/ssh-auth.sock";

/// Container-side socket path for direct bind-mount forwarding.
const CONTAINER_SSH_SOCK: &str = "/tmp/cella-ssh-agent.sock";

/// macOS App Sandbox directory markers. Paths inside these roots are
/// not reachable from Lima-class VMs (e.g. colima): macOS blocks the
/// virtiofs layer from stat'ing sandboxed files, which surfaces as
/// `operation not supported` from the Docker daemon on bind-mount.
const MACOS_SANDBOX_DIR_MARKERS: &[&str] = &["/Library/Group Containers/", "/Library/Containers/"];

/// Returns `true` if `path` — after symlink resolution — sits under a
/// macOS App Sandbox directory. 1Password 8 exposes a stable symlink at
/// `~/.1password/agent.sock` pointing into `~/Library/Group Containers/…`,
/// so canonicalization is required to catch both forms.
fn is_macos_sandboxed_path(path: &str) -> bool {
    let canonical = std::fs::canonicalize(path)
        .map_or_else(|_| path.to_string(), |p| p.to_string_lossy().into_owned());
    MACOS_SANDBOX_DIR_MARKERS
        .iter()
        .any(|marker| canonical.contains(marker))
}

/// SSH agent forwarding for Docker Desktop / `OrbStack` (VM-based runtimes).
fn desktop_ssh_forwarding(host_socket: Option<&String>) -> SshAgentForwarding {
    if host_socket.is_none() {
        warn!("SSH_AUTH_SOCK not set on host, but Docker Desktop may still provide SSH agent");
    }
    SshAgentForwarding {
        mount_source: DOCKER_DESKTOP_SSH_SOCK.to_string(),
        mount_target: DOCKER_DESKTOP_SSH_SOCK.to_string(),
        env_value: DOCKER_DESKTOP_SSH_SOCK.to_string(),
    }
}

/// SSH agent forwarding via direct bind-mount of the host socket.
///
/// On `DockerRuntime::Colima`, paths under macOS App Sandbox dirs (e.g.
/// 1Password's `~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock`)
/// are unreachable from the Lima VM and the bind-mount would fail at
/// container-create with `operation not supported`. Skip with a warning so
/// `cella up` still succeeds; users can override via `containerEnv.SSH_AUTH_SOCK`.
fn direct_ssh_forwarding(
    runtime: &DockerRuntime,
    host_socket: Option<String>,
) -> Option<SshAgentForwarding> {
    let host_socket = host_socket?;

    if !std::path::Path::new(&host_socket).exists() {
        warn!(
            "SSH_AUTH_SOCK points to {host_socket} which does not exist, skipping SSH agent forwarding"
        );
        return None;
    }

    if matches!(runtime, DockerRuntime::Colima) && is_macos_sandboxed_path(&host_socket) {
        warn!(
            "SSH_AUTH_SOCK at {host_socket} is a macOS sandboxed path unreachable from {runtime}'s VM; skipping SSH agent mount. Set SSH_AUTH_SOCK in devcontainer.json containerEnv to override."
        );
        return None;
    }

    Some(SshAgentForwarding {
        mount_source: host_socket,
        mount_target: CONTAINER_SSH_SOCK.to_string(),
        env_value: CONTAINER_SSH_SOCK.to_string(),
    })
}

/// Detect the host SSH agent socket and generate forwarding configuration.
///
/// Returns `None` if:
/// - `SSH_AUTH_SOCK` is unset or empty
/// - The user has already configured `SSH_AUTH_SOCK` in `containerEnv`/`remoteEnv`
/// - The user has a mount targeting the SSH socket path
pub fn ssh_agent_forwarding(
    runtime: &DockerRuntime,
    config: &serde_json::Value,
) -> Option<SshAgentForwarding> {
    if has_user_ssh_override(config) {
        tracing::debug!("User has SSH_AUTH_SOCK override in config, skipping auto-forward");
        return None;
    }

    let host_socket = std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|s| !s.is_empty());

    if matches!(
        runtime,
        DockerRuntime::DockerDesktop | DockerRuntime::OrbStack
    ) {
        Some(desktop_ssh_forwarding(host_socket.as_ref()))
    } else {
        direct_ssh_forwarding(runtime, host_socket)
    }
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
    fn test_desktop_ssh_forwarding_returns_docker_desktop_path() {
        let fwd = desktop_ssh_forwarding(None);
        assert_eq!(fwd.mount_source, "/run/host-services/ssh-auth.sock");
        assert_eq!(fwd.mount_target, "/run/host-services/ssh-auth.sock");
        assert_eq!(fwd.env_value, "/run/host-services/ssh-auth.sock");
    }

    #[test]
    fn test_desktop_ssh_forwarding_with_socket_ignores_it() {
        let host = "/tmp/ssh.sock".to_string();
        let fwd = desktop_ssh_forwarding(Some(&host));
        assert_eq!(fwd.mount_source, "/run/host-services/ssh-auth.sock");
        assert_eq!(fwd.mount_target, "/run/host-services/ssh-auth.sock");
        assert_eq!(fwd.env_value, "/run/host-services/ssh-auth.sock");
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
    fn is_macos_sandboxed_path_matches_group_containers() {
        let tmp = tempfile::tempdir().unwrap();
        let sandboxed = tmp
            .path()
            .join("Library/Group Containers/2BUA8C4S2C.com.1password/t");
        std::fs::create_dir_all(&sandboxed).unwrap();
        let sock = sandboxed.join("agent.sock");
        std::fs::File::create(&sock).unwrap();
        assert!(is_macos_sandboxed_path(sock.to_str().unwrap()));
    }

    #[test]
    fn is_macos_sandboxed_path_matches_containers() {
        let tmp = tempfile::tempdir().unwrap();
        let sandboxed = tmp.path().join("Library/Containers/com.example.app/Data");
        std::fs::create_dir_all(&sandboxed).unwrap();
        let sock = sandboxed.join("agent.sock");
        std::fs::File::create(&sock).unwrap();
        assert!(is_macos_sandboxed_path(sock.to_str().unwrap()));
    }

    #[test]
    fn is_macos_sandboxed_path_rejects_tmp_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ssh.sock");
        std::fs::File::create(&sock).unwrap();
        assert!(!is_macos_sandboxed_path(sock.to_str().unwrap()));
    }

    #[test]
    fn is_macos_sandboxed_path_handles_missing_path_via_literal_match() {
        // canonicalize fails on missing paths — we fall back to the literal
        // string, so the sandbox marker still matches (or doesn't).
        assert!(is_macos_sandboxed_path(
            "/does/not/exist/Library/Group Containers/x/sock"
        ));
        assert!(!is_macos_sandboxed_path("/does/not/exist/ssh.sock"));
    }

    #[test]
    fn is_macos_sandboxed_path_resolves_symlink_into_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("Library/Group Containers/vendor.app/t");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_sock = real_dir.join("agent.sock");
        std::fs::File::create(&real_sock).unwrap();

        let link_dir = tmp.path().join("home/.1password");
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("agent.sock");
        std::os::unix::fs::symlink(&real_sock, &link).unwrap();

        assert!(is_macos_sandboxed_path(link.to_str().unwrap()));
    }

    #[test]
    fn direct_ssh_forwarding_colima_skips_group_containers() {
        let tmp = tempfile::tempdir().unwrap();
        let sandboxed = tmp
            .path()
            .join("Library/Group Containers/2BUA8C4S2C.com.1password/t");
        std::fs::create_dir_all(&sandboxed).unwrap();
        let sock = sandboxed.join("agent.sock");
        std::fs::File::create(&sock).unwrap();

        let result = direct_ssh_forwarding(
            &DockerRuntime::Colima,
            Some(sock.to_string_lossy().into_owned()),
        );
        assert!(result.is_none());
    }

    #[test]
    fn direct_ssh_forwarding_colima_allows_tmp_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("ssh.sock");
        std::fs::File::create(&sock).unwrap();

        let result = direct_ssh_forwarding(
            &DockerRuntime::Colima,
            Some(sock.to_string_lossy().into_owned()),
        );
        let fwd = result.expect("non-sandboxed path should mount on colima");
        assert_eq!(fwd.mount_target, CONTAINER_SSH_SOCK);
    }

    #[test]
    fn direct_ssh_forwarding_linux_native_allows_sandboxed_path() {
        // On Linux there's no VM, so sandbox-marker paths are fine
        // (a Linux user with a literal "Library/Group Containers" dir
        // on their filesystem isn't affected by the colima limitation).
        let tmp = tempfile::tempdir().unwrap();
        let sandboxed = tmp.path().join("Library/Group Containers/whatever/t");
        std::fs::create_dir_all(&sandboxed).unwrap();
        let sock = sandboxed.join("agent.sock");
        std::fs::File::create(&sock).unwrap();

        let result = direct_ssh_forwarding(
            &DockerRuntime::LinuxNative,
            Some(sock.to_string_lossy().into_owned()),
        );
        assert!(
            result.is_some(),
            "LinuxNative should not apply the colima sandbox skip"
        );
    }

    #[test]
    fn direct_ssh_forwarding_colima_resolves_symlink_before_skip() {
        // Simulates 1Password's ~/.1password/agent.sock → Group Containers
        // symlink. Canonicalization must resolve it so the skip fires.
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("Library/Group Containers/vendor.app/t");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_sock = real_dir.join("agent.sock");
        std::fs::File::create(&real_sock).unwrap();

        let link_dir = tmp.path().join("home/.1password");
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("agent.sock");
        std::os::unix::fs::symlink(&real_sock, &link).unwrap();

        let result = direct_ssh_forwarding(
            &DockerRuntime::Colima,
            Some(link.to_string_lossy().into_owned()),
        );
        assert!(
            result.is_none(),
            "symlink pointing into Group Containers must be skipped on colima"
        );
    }
}
