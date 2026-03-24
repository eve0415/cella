//! Daemon lifecycle: PID file, daemonization, liveness check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tracing::{debug, info, warn};

use cella_daemon::shared::{
    cleanup_files, current_time_secs, is_docker_reachable, running_cella_container_count,
};

use crate::CellaCredentialProxyError;
use crate::server::{run_server, run_tcp_server};

/// Default idle timeout (30 minutes).
const IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// Health check interval (5 minutes).
const HEALTH_CHECK_INTERVAL_SECS: u64 = 5 * 60;

/// Run the credential proxy daemon.
///
/// This is the main entry point for `cella credential-proxy daemon`.
/// Creates the PID file, starts both Unix socket and TCP listeners,
/// and monitors for shutdown conditions.
///
/// # Errors
///
/// Returns error if socket binding or PID file creation fails.
pub async fn run_daemon(
    socket_path: &Path,
    pid_path: &Path,
    port_path: &Path,
) -> Result<(), CellaCredentialProxyError> {
    // Ensure parent directory exists
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CellaCredentialProxyError::PidFile {
            message: format!("failed to create directory {}: {e}", parent.display()),
        })?;
    }

    // Write PID file
    let pid = std::process::id();
    std::fs::write(pid_path, pid.to_string()).map_err(|e| CellaCredentialProxyError::PidFile {
        message: format!("failed to write PID file: {e}"),
    })?;
    info!("Credential proxy daemon started (PID {pid})");

    let last_activity = Arc::new(AtomicU64::new(current_time_secs()));

    // Spawn idle timeout + health check monitor
    let la = last_activity.clone();
    let pid_path_owned = pid_path.to_path_buf();
    let socket_path_owned = socket_path.to_path_buf();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS)).await;

            let last = la.load(Ordering::Relaxed);
            let now = current_time_secs();
            let elapsed = now.saturating_sub(last);

            if elapsed > IDLE_TIMEOUT_SECS {
                let count = running_cella_container_count();
                if count > 0 {
                    debug!(
                        "Idle timeout reached but {count} cella container(s) still running, staying alive"
                    );
                    la.store(current_time_secs(), Ordering::Relaxed);
                    continue;
                }
                info!("Idle timeout ({IDLE_TIMEOUT_SECS}s) reached, shutting down");
                cleanup_files(&[&pid_path_owned, &socket_path_owned]);
                std::process::exit(0);
            }

            // Check if Docker daemon is reachable
            if !is_docker_reachable() {
                info!("Docker daemon not reachable, shutting down");
                cleanup_files(&[&pid_path_owned, &socket_path_owned]);
                std::process::exit(0);
            }

            debug!("Health check passed (idle {elapsed}s)");
        }
    });

    // Start TCP server alongside the Unix socket server
    let tcp_activity = last_activity.clone();
    let port_path_owned = port_path.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = run_tcp_server(&port_path_owned, tcp_activity).await {
            warn!("TCP server error: {e}");
        }
    });

    // Run the Unix socket server (blocks until error)
    let result = run_server(socket_path, last_activity).await;

    // Clean up on exit
    cleanup_files(&[pid_path, socket_path, port_path]);

    result
}

/// Check if the daemon is already running.
pub fn is_daemon_running(pid_path: &Path, socket_path: &Path) -> bool {
    cella_daemon::shared::is_daemon_running(pid_path, socket_path)
}

/// Start the daemon as a detached background process.
///
/// Spawns `cella credential-proxy daemon` and returns immediately.
///
/// # Errors
///
/// Returns error if the daemon process cannot be spawned.
pub fn start_daemon_background(
    socket_path: &Path,
    pid_path: &Path,
    port_path: &Path,
) -> Result<(), CellaCredentialProxyError> {
    let args = [
        "credential-proxy",
        "daemon",
        "--socket",
        &socket_path.to_string_lossy(),
        "--pid-file",
        &pid_path.to_string_lossy(),
        "--port-file",
        &port_path.to_string_lossy(),
    ];
    cella_daemon::shared::start_background_process(&args).map_err(|e| {
        CellaCredentialProxyError::PidFile {
            message: format!("failed to spawn daemon: {e}"),
        }
    })?;

    info!("Credential proxy daemon started in background");
    Ok(())
}

/// Ensure the credential proxy daemon is running.
///
/// If not running, start it. Returns the socket path.
///
/// # Errors
///
/// Returns error if the daemon cannot be started.
pub fn ensure_daemon_running(
    socket_path: &Path,
    pid_path: &Path,
    port_path: &Path,
) -> Result<PathBuf, CellaCredentialProxyError> {
    if is_daemon_running(pid_path, socket_path) {
        debug!("Credential proxy daemon already running");
        return Ok(socket_path.to_path_buf());
    }

    start_daemon_background(socket_path, pid_path, port_path)?;

    // Brief wait for the daemon to create its socket
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if socket_path.exists() {
            return Ok(socket_path.to_path_buf());
        }
    }

    warn!("Credential proxy daemon started but socket not yet available");
    Ok(socket_path.to_path_buf())
}

/// Stop the running daemon.
///
/// # Errors
///
/// Returns `CellaCredentialProxyError::NotRunning` if no daemon is running.
pub fn stop_daemon(
    pid_path: &Path,
    socket_path: &Path,
    port_path: &Path,
) -> Result<(), CellaCredentialProxyError> {
    let pid = cella_daemon::shared::read_pid_file(pid_path)
        .ok_or(CellaCredentialProxyError::NotRunning)?;

    // Send SIGTERM
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .exec();
    }

    // On non-unix, just try kill command
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }

    cleanup_files(&[pid_path, socket_path, port_path]);
    info!("Credential proxy daemon stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        let socket_path = dir.path().join("test.sock");
        let port_path = dir.path().join("test.port");
        std::fs::write(&pid_path, "12345").unwrap();
        std::fs::write(&socket_path, "").unwrap();
        std::fs::write(&port_path, "54321").unwrap();

        cleanup_files(&[&pid_path, &socket_path, &port_path]);

        assert!(!pid_path.exists());
        assert!(!socket_path.exists());
        assert!(!port_path.exists());
    }

    #[test]
    fn cleanup_preserves_unspecified_files() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        let socket_path = dir.path().join("test.sock");
        let port_path = dir.path().join("test.port");
        std::fs::write(&pid_path, "12345").unwrap();
        std::fs::write(&socket_path, "").unwrap();
        std::fs::write(&port_path, "54321").unwrap();

        cleanup_files(&[&pid_path, &socket_path]);

        assert!(!pid_path.exists());
        assert!(!socket_path.exists());
        assert!(port_path.exists());
    }

    #[test]
    #[ignore = "requires Docker"]
    fn running_container_count_with_no_containers() {
        let count = running_cella_container_count();
        assert_eq!(count, 0);
    }

    #[test]
    fn daemon_not_running_with_no_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        let socket_path = dir.path().join("test.sock");
        assert!(!is_daemon_running(&pid_path, &socket_path));
    }
}
