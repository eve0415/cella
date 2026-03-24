//! Socket client for health checking the credential proxy daemon.

use std::path::Path;

use crate::CellaCredentialProxyError;

/// Re-export shared `DaemonStatus`.
pub use cella_daemon::shared::DaemonStatus;

/// Ping the credential proxy daemon and check if it responds.
///
/// # Errors
///
/// Returns `CellaCredentialProxyError::Socket` if connection or I/O fails.
pub fn ping_daemon(socket_path: &Path) -> Result<bool, CellaCredentialProxyError> {
    cella_daemon::shared::ping_daemon(socket_path).map_err(|e| CellaCredentialProxyError::Socket {
        message: format!("ping failed for {}: {e}", socket_path.display()),
    })
}

/// Check the full status of the credential proxy daemon.
pub fn daemon_status(socket_path: &Path, pid_path: &Path) -> DaemonStatus {
    cella_daemon::shared::daemon_status(socket_path, pid_path)
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
