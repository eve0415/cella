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

/// The VM-side magic SSH agent socket path that Docker Desktop, `OrbStack`,
/// and colima (when `colima start --ssh-agent` is set) all expose inside
/// their VMs as a forwarded host agent. Lima creates this path as a symlink
/// to the host agent when `ssh.forwardAgent: true`; colima enables that
/// Lima option via its own `forwardAgent` config flag.
const VM_HOST_SERVICES_SSH_SOCK: &str = "/run/host-services/ssh-auth.sock";

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

/// Pure path-builder used by `colima_config_paths`. Kept separate so tests
/// can exercise the ordering without manipulating process env vars
/// (workspace denies `unsafe_code`, which rules out `std::env::set_var`).
fn colima_config_paths_from_env(
    profile: &str,
    colima_home: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(ch) = colima_home {
        paths.push(
            std::path::PathBuf::from(ch)
                .join(profile)
                .join("colima.yaml"),
        );
    }
    if let Some(xdg) = xdg_config_home {
        paths.push(
            std::path::PathBuf::from(xdg)
                .join("colima")
                .join(profile)
                .join("colima.yaml"),
        );
    }
    if let Some(h) = home {
        paths.push(
            std::path::PathBuf::from(h)
                .join(".colima")
                .join(profile)
                .join("colima.yaml"),
        );
    }
    paths
}

/// Candidate paths for the active colima profile's `colima.yaml`, in the
/// lookup order colima itself uses (see `abiosoft/colima` `config/files.go`):
/// `$COLIMA_HOME` → `$XDG_CONFIG_HOME/colima` → `$HOME/.colima`, each joined
/// with the active profile name (`$COLIMA_PROFILE` or `default`).
fn colima_config_paths() -> Vec<std::path::PathBuf> {
    let profile = std::env::var("COLIMA_PROFILE").unwrap_or_else(|_| "default".to_string());
    colima_config_paths_from_env(
        &profile,
        std::env::var("COLIMA_HOME").ok().as_deref(),
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Returns `true` if the given colima YAML sets `forwardAgent: true` as a
/// top-level key. Tolerant of leading whitespace, trailing comments, and
/// mixed casing of the boolean; rejects nested keys or quoted booleans to
/// avoid false positives on fields like `ssh: {forwardAgent: ...}`.
fn parse_forward_agent_from_yaml(content: &str) -> bool {
    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim_end();
        // Top-level only — reject indented/nested entries.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(rest) = line.strip_prefix("forwardAgent:") else {
            continue;
        };
        if rest.trim().eq_ignore_ascii_case("true") {
            return true;
        }
    }
    false
}

/// Returns `true` if any provided candidate colima config file declares
/// `forwardAgent: true`. Missing or unreadable files are treated as
/// "not enabled". Split from `colima_forward_agent_enabled` so tests
/// can drive this with a tempdir-backed path without mutating process env.
fn colima_forward_agent_enabled_at(paths: &[std::path::PathBuf]) -> bool {
    paths.iter().any(|path| {
        std::fs::read_to_string(path).is_ok_and(|content| parse_forward_agent_from_yaml(&content))
    })
}

/// Returns `true` when any candidate colima config file for the active
/// profile declares `forwardAgent: true`. Missing or unreadable files are
/// treated as "not enabled".
fn colima_forward_agent_enabled() -> bool {
    colima_forward_agent_enabled_at(&colima_config_paths())
}

/// SSH agent forwarding for runtimes whose VM exposes the host agent at the
/// Docker Desktop / Lima magic path `/run/host-services/ssh-auth.sock`.
/// Used for Docker Desktop, `OrbStack`, and colima-with-`forwardAgent`.
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
/// Only reached when the dispatcher in `ssh_agent_forwarding` couldn't
/// satisfy the request from a VM-side magic path: for colima this means
/// `forwardAgent` is OFF in colima.yaml, so we fall back to attempting a
/// direct bind-mount of the host socket. If that host path is under a
/// macOS App Sandbox dir (the 1Password case), Lima's virtiofs cannot
/// stat it and the bind-mount would fail at container-create with
/// `operation not supported`; short-circuit here with a warning that
/// tells the user exactly which colima knob to flip to get full
/// host-agent forwarding on the next up.
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
            "SSH_AUTH_SOCK at {host_socket} sits in a macOS sandboxed directory that colima's Lima VM cannot stat; skipping SSH agent mount. Restart colima with `colima start --ssh-agent` and cella will pick up the host agent from /run/host-services/ssh-auth.sock on the next up."
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

    match runtime {
        DockerRuntime::DockerDesktop | DockerRuntime::OrbStack => Some(
            vm_host_services_ssh_forwarding(runtime, host_socket.as_ref()),
        ),
        DockerRuntime::Colima if colima_forward_agent_enabled() => {
            tracing::info!(
                "colima forwardAgent is enabled; using {VM_HOST_SERVICES_SSH_SOCK} for SSH agent forwarding"
            );
            Some(vm_host_services_ssh_forwarding(
                runtime,
                host_socket.as_ref(),
            ))
        }
        _ => direct_ssh_forwarding(runtime, host_socket),
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

    #[test]
    fn vm_host_services_ssh_forwarding_works_for_colima() {
        let fwd = vm_host_services_ssh_forwarding(&DockerRuntime::Colima, None);
        assert_eq!(fwd.mount_source, "/run/host-services/ssh-auth.sock");
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

    #[test]
    fn parse_forward_agent_yaml_detects_top_level_true() {
        assert!(parse_forward_agent_from_yaml("forwardAgent: true\n"));
        assert!(parse_forward_agent_from_yaml(
            "cpu: 4\nforwardAgent: true\nmemory: 8\n"
        ));
        assert!(parse_forward_agent_from_yaml(
            "forwardAgent: TRUE # enable 1Password\n"
        ));
    }

    #[test]
    fn parse_forward_agent_yaml_rejects_false_or_missing() {
        assert!(!parse_forward_agent_from_yaml(""));
        assert!(!parse_forward_agent_from_yaml("cpu: 4\nmemory: 8\n"));
        assert!(!parse_forward_agent_from_yaml("forwardAgent: false\n"));
        assert!(!parse_forward_agent_from_yaml("forwardAgent: no\n"));
    }

    #[test]
    fn parse_forward_agent_yaml_rejects_nested_key() {
        // `ssh.forwardAgent` is NOT colima's top-level knob. Lima uses
        // `ssh.forwardAgent` internally but colima's user-facing field
        // is a top-level `forwardAgent`. Reject indented matches to avoid
        // accidentally picking up unrelated nested fields.
        let yaml = "ssh:\n  forwardAgent: true\n";
        assert!(!parse_forward_agent_from_yaml(yaml));
    }

    #[test]
    fn parse_forward_agent_yaml_ignores_comments() {
        assert!(!parse_forward_agent_from_yaml("# forwardAgent: true\n"));
        assert!(parse_forward_agent_from_yaml(
            "forwardAgent: true # commented explanation\n"
        ));
    }

    #[test]
    fn colima_config_paths_full_env_returns_three_paths_in_priority_order() {
        let paths = colima_config_paths_from_env("work", Some("/ch"), Some("/xdg"), Some("/h"));
        assert_eq!(
            paths,
            vec![
                std::path::PathBuf::from("/ch/work/colima.yaml"),
                std::path::PathBuf::from("/xdg/colima/work/colima.yaml"),
                std::path::PathBuf::from("/h/.colima/work/colima.yaml"),
            ]
        );
    }

    #[test]
    fn colima_config_paths_skips_unset_env_vars() {
        // Only HOME set — the other two candidates should be omitted.
        let paths = colima_config_paths_from_env("default", None, None, Some("/h"));
        assert_eq!(
            paths,
            vec![std::path::PathBuf::from("/h/.colima/default/colima.yaml")]
        );
    }

    #[test]
    fn colima_config_paths_no_env_returns_empty() {
        assert!(colima_config_paths_from_env("default", None, None, None).is_empty());
    }

    #[test]
    fn colima_forward_agent_enabled_at_reads_tempdir_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("default");
        std::fs::create_dir_all(&dir).unwrap();
        let yaml = dir.join("colima.yaml");
        std::fs::write(&yaml, "cpu: 2\nforwardAgent: true\nmemory: 4\n").unwrap();

        assert!(colima_forward_agent_enabled_at(&[yaml]));
    }

    #[test]
    fn colima_forward_agent_enabled_at_missing_files_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("missing/colima.yaml");
        assert!(!colima_forward_agent_enabled_at(&[nonexistent]));
    }

    #[test]
    fn colima_forward_agent_enabled_at_disabled_yaml_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let yaml = tmp.path().join("colima.yaml");
        std::fs::write(&yaml, "forwardAgent: false\n").unwrap();
        assert!(!colima_forward_agent_enabled_at(&[yaml]));
    }
}
