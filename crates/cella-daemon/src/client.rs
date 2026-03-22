//! Socket client for health checking the cella daemon.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::CellaDaemonError;

/// Ping the daemon and check if it responds.
///
/// # Errors
///
/// Returns error if connection or I/O fails.
pub fn ping_daemon(socket_path: &Path) -> Result<bool, CellaDaemonError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|e| CellaDaemonError::Socket {
        message: format!("failed to connect to {}: {e}", socket_path.display()),
    })?;

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("failed to set timeout: {e}"),
        })?;

    stream
        .write_all(b"ping\n\n")
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("write error: {e}"),
        })?;

    let mut buf = [0u8; 64];
    let n = stream
        .read(&mut buf)
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("read error: {e}"),
        })?;

    let response = String::from_utf8_lossy(&buf[..n]);
    Ok(response.trim() == "pong")
}

/// Get daemon status information.
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub socket_exists: bool,
    pub responsive: bool,
}

/// Check the full status of the cella daemon.
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
    }
}
