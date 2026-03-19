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

    if *runtime == DockerRuntime::DockerDesktop {
        // Docker Desktop provides SSH agent via its VM at a well-known path.
        // No host socket needed — Docker Desktop forwards automatically.
        if host_socket.is_none() {
            warn!("SSH_AUTH_SOCK not set on host, but Docker Desktop may still provide SSH agent");
        }
        Some(SshAgentForwarding {
            mount_source: DOCKER_DESKTOP_SSH_SOCK.to_string(),
            mount_target: DOCKER_DESKTOP_SSH_SOCK.to_string(),
            env_value: DOCKER_DESKTOP_SSH_SOCK.to_string(),
        })
    } else {
        // Direct bind-mount of host SSH agent socket
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
}
