//! Socket client for health checking the cella daemon.

use std::path::Path;

use crate::CellaDaemonError;

/// Re-export shared `DaemonStatus`.
pub use crate::shared::DaemonStatus;

/// Ping the daemon and check if it responds.
///
/// # Errors
///
/// Returns error if connection or I/O fails.
pub fn ping_daemon(socket_path: &Path) -> Result<bool, CellaDaemonError> {
    crate::shared::ping_daemon(socket_path).map_err(|e| CellaDaemonError::Socket {
        message: format!("ping failed for {}: {e}", socket_path.display()),
    })
}

/// Check the full status of the cella daemon.
pub fn daemon_status(socket_path: &Path, pid_path: &Path) -> DaemonStatus {
    crate::shared::daemon_status(socket_path, pid_path)
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
