//! Credential proxy socket detection and mount generation.

use std::path::PathBuf;

use crate::FileUpload;

/// Container-side path for the credential proxy socket.
const CONTAINER_CREDENTIAL_SOCK: &str = "/tmp/cella-credential-proxy.sock";

/// Credential proxy forwarding configuration (Unix socket bind-mount).
pub struct CredentialForwarding {
    /// Host-side socket path.
    pub mount_source: String,
    /// Container-side socket path.
    pub mount_target: String,
    /// Value for `CELLA_CREDENTIAL_SOCKET` env var.
    pub env_value: String,
}

/// Credential proxy TCP forwarding configuration (for VM-based runtimes).
pub struct CredentialForwardingTcp {
    /// TCP port the credential proxy is listening on.
    pub port: u16,
}

/// Check if the credential proxy socket exists and generate forwarding configuration.
///
/// Returns `None` if the credential proxy daemon is not running
/// (socket doesn't exist).
pub fn credential_forwarding() -> Option<CredentialForwarding> {
    let socket_path = credential_proxy_socket_path()?;
    if !socket_path.exists() {
        return None;
    }

    Some(CredentialForwarding {
        mount_source: socket_path.to_string_lossy().to_string(),
        mount_target: CONTAINER_CREDENTIAL_SOCK.to_string(),
        env_value: CONTAINER_CREDENTIAL_SOCK.to_string(),
    })
}

/// Generate the credential helper shell script for injection into the container.
///
/// Supports two modes:
/// - **TCP mode**: when `CELLA_CREDENTIAL_HOST` env var is set (for VM-based runtimes)
/// - **Unix socket mode**: falls back to the bind-mounted socket at a well-known path
pub fn credential_helper_script(remote_user: &str) -> FileUpload {
    let script = r#"#!/bin/sh
# cella git credential helper — forwards to host credential proxy.
# Installed by cella for transparent credential forwarding.
op="$1"
if [ -n "$CELLA_CREDENTIAL_HOST" ]; then
  # TCP mode (VM-based runtimes: OrbStack, Colima)
  host="${CELLA_CREDENTIAL_HOST%%:*}"
  port="${CELLA_CREDENTIAL_HOST##*:}"
  if command -v socat >/dev/null 2>&1; then
    { printf '%s\n' "$op"; cat; } | socat - "TCP:$host:$port"
  elif command -v nc >/dev/null 2>&1; then
    { printf '%s\n' "$op"; cat; } | nc "$host" "$port"
  else
    echo "cella: no socat or nc available for credential forwarding" >&2
    exit 1
  fi
else
  # Unix socket mode (direct bind-mount: Linux, Docker Desktop)
  sock="/tmp/cella-credential-proxy.sock"
  if command -v socat >/dev/null 2>&1; then
    { printf '%s\n' "$op"; cat; } | socat - "UNIX-CONNECT:$sock"
  elif command -v nc >/dev/null 2>&1; then
    { printf '%s\n' "$op"; cat; } | nc -U "$sock"
  else
    echo "cella: no socat or nc available for credential forwarding" >&2
    exit 1
  fi
fi
"#;

    let _ = remote_user; // Path is the same regardless of user

    FileUpload {
        container_path: "/usr/local/bin/cella-git-credential-helper".to_string(),
        content: script.as_bytes().to_vec(),
        mode: 0o755,
    }
}

/// Check if the credential proxy TCP port is available and return TCP forwarding config.
///
/// Returns `None` if the port file doesn't exist or can't be read.
pub fn credential_forwarding_tcp() -> Option<CredentialForwardingTcp> {
    let port = credential_proxy_tcp_port()?;
    Some(CredentialForwardingTcp { port })
}

/// Read the TCP port from the credential proxy port file.
pub fn credential_proxy_tcp_port() -> Option<u16> {
    let port_path = credential_proxy_port_path()?;
    let content = std::fs::read_to_string(port_path).ok()?;
    content.trim().parse().ok()
}

/// Get the expected path for the credential proxy socket.
pub fn credential_proxy_socket_path() -> Option<PathBuf> {
    cella_data_dir().map(|d| d.join("credential-proxy.sock"))
}

/// Get the expected path for the credential proxy PID file.
pub fn credential_proxy_pid_path() -> Option<PathBuf> {
    cella_data_dir().map(|d| d.join("credential-proxy.pid"))
}

/// Get the expected path for the credential proxy TCP port file.
pub fn credential_proxy_port_path() -> Option<PathBuf> {
    cella_data_dir().map(|d| d.join("credential-proxy.port"))
}

/// Get the cella data directory (`~/.cella/`).
pub fn cella_data_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".cella"))
}

/// Get the daemon management socket path (`~/.cella/daemon.sock`).
pub fn daemon_management_socket_path() -> Option<PathBuf> {
    cella_data_dir().map(|d| d.join("daemon.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cella_data_dir_uses_home() {
        if let Ok(home) = std::env::var("HOME") {
            let dir = cella_data_dir().unwrap();
            assert_eq!(dir, PathBuf::from(home).join(".cella"));
        }
    }

    #[test]
    fn credential_helper_script_is_executable() {
        let script = credential_helper_script("vscode");
        assert_eq!(script.mode, 0o755);
        assert!(script.content.starts_with(b"#!/bin/sh"));
    }

    #[test]
    fn credential_helper_script_path() {
        let script = credential_helper_script("vscode");
        assert_eq!(
            script.container_path,
            "/usr/local/bin/cella-git-credential-helper"
        );
    }

    #[test]
    fn credential_helper_script_has_tcp_support() {
        let script = credential_helper_script("vscode");
        let content = String::from_utf8(script.content).unwrap();
        assert!(content.contains("CELLA_CREDENTIAL_HOST"));
        assert!(content.contains("TCP:"));
        assert!(content.contains("UNIX-CONNECT:"));
    }

    #[test]
    fn port_path_uses_cella_data_dir() {
        if let Ok(home) = std::env::var("HOME") {
            let path = credential_proxy_port_path().unwrap();
            assert_eq!(
                path,
                PathBuf::from(home).join(".cella/credential-proxy.port")
            );
        }
    }

    #[test]
    fn daemon_management_socket_path_format() {
        if let Ok(home) = std::env::var("HOME") {
            let path = daemon_management_socket_path().unwrap();
            assert_eq!(path, PathBuf::from(home).join(".cella/daemon.sock"));
        }
    }
}
