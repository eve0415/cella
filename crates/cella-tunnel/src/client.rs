//! Control socket client for interacting with the tunnel daemon.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::CellaTunnelError;

/// Send a `connect` command to the tunnel daemon.
///
/// # Errors
///
/// Returns error if connection or I/O fails.
pub fn connect_container(
    socket_path: &Path,
    container_id: &str,
) -> Result<String, CellaTunnelError> {
    send_command(socket_path, &format!("connect {container_id}\n"))
}

/// Send a `disconnect` command to the tunnel daemon.
///
/// # Errors
///
/// Returns error if connection or I/O fails.
pub fn disconnect_container(
    socket_path: &Path,
    container_id: &str,
) -> Result<String, CellaTunnelError> {
    send_command(socket_path, &format!("disconnect {container_id}\n"))
}

/// Send a `status` command to the tunnel daemon.
///
/// # Errors
///
/// Returns error if connection or I/O fails.
pub fn query_status(socket_path: &Path) -> Result<String, CellaTunnelError> {
    send_command(socket_path, "status\n")
}

/// Ping the tunnel daemon.
///
/// # Errors
///
/// Returns error if connection or I/O fails.
pub fn ping_daemon(socket_path: &Path) -> Result<bool, CellaTunnelError> {
    let response = send_command(socket_path, "ping\n")?;
    Ok(response.trim() == "pong")
}

/// Daemon status information.
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub socket_exists: bool,
    pub responsive: bool,
}

/// Check the full status of the tunnel daemon.
pub fn daemon_status(socket_path: &Path, pid_path: &Path) -> DaemonStatus {
    let pid = std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|s| s.trim().parse().ok());

    let socket_exists = socket_path.exists();

    let responsive = socket_exists && ping_daemon(socket_path).unwrap_or(false);

    let running = pid.is_some() && responsive;

    DaemonStatus {
        running,
        pid,
        socket_exists,
        responsive,
    }
}

fn send_command(socket_path: &Path, command: &str) -> Result<String, CellaTunnelError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|e| CellaTunnelError::Socket {
        message: format!("failed to connect to {}: {e}", socket_path.display()),
    })?;

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| CellaTunnelError::Socket {
            message: format!("failed to set timeout: {e}"),
        })?;

    stream
        .write_all(command.as_bytes())
        .map_err(|e| CellaTunnelError::Socket {
            message: format!("write error: {e}"),
        })?;

    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .map_err(|e| CellaTunnelError::Socket {
            message: format!("read error: {e}"),
        })?;

    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let status = daemon_status(
            &dir.path().join("nonexistent.sock"),
            &dir.path().join("nonexistent.pid"),
        );
        assert!(!status.running);
        assert!(status.pid.is_none());
        assert!(!status.socket_exists);
        assert!(!status.responsive);
    }
}
