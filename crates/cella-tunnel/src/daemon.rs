//! Daemon lifecycle: PID file, daemonization, liveness check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::CellaTunnelError;
use crate::server::run_control_server;
use crate::tunnel::TunnelManager;

/// Default idle timeout (30 minutes).
const IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// Health check interval (5 minutes).
const HEALTH_CHECK_INTERVAL_SECS: u64 = 5 * 60;

/// Run the tunnel daemon.
///
/// Creates the PID file, starts the control server, and monitors for
/// shutdown conditions.
///
/// # Errors
///
/// Returns error if socket binding or PID file creation fails.
pub async fn run_daemon(socket_path: &Path, pid_path: &Path) -> Result<(), CellaTunnelError> {
    // Ensure parent directory exists
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CellaTunnelError::PidFile {
            message: format!("failed to create directory {}: {e}", parent.display()),
        })?;
    }

    // Write PID file
    let pid = std::process::id();
    std::fs::write(pid_path, pid.to_string()).map_err(|e| CellaTunnelError::PidFile {
        message: format!("failed to write PID file: {e}"),
    })?;
    info!("Tunnel daemon started (PID {pid})");

    let last_activity = Arc::new(AtomicU64::new(current_time_secs()));
    let manager = Arc::new(TunnelManager::new(Arc::clone(&last_activity)));

    // Spawn idle timeout + health check monitor
    let la = Arc::clone(&last_activity);
    let pid_path_owned = pid_path.to_path_buf();
    let socket_path_owned = socket_path.to_path_buf();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS)).await;

            let last = la.load(Ordering::Relaxed);
            let now = current_time_secs();
            let elapsed = now.saturating_sub(last);

            if elapsed > IDLE_TIMEOUT_SECS {
                info!("Idle timeout ({IDLE_TIMEOUT_SECS}s) reached, shutting down");
                cleanup(&pid_path_owned, &socket_path_owned);
                std::process::exit(0);
            }

            if !is_docker_reachable() {
                info!("Docker daemon not reachable, shutting down");
                cleanup(&pid_path_owned, &socket_path_owned);
                std::process::exit(0);
            }

            debug!("Health check passed (idle {elapsed}s)");
        }
    });

    // Run the control server (blocks until error)
    let result = run_control_server(socket_path, manager, Arc::clone(&last_activity)).await;

    // Clean up on exit
    cleanup(pid_path, socket_path);

    result
}

/// Check if the daemon is already running.
pub fn is_daemon_running(pid_path: &Path, socket_path: &Path) -> bool {
    let Some(pid) = read_pid_file(pid_path) else {
        return false;
    };

    let alive = unsafe_process_alive(pid);
    if !alive {
        debug!("Stale PID file found (PID {pid}), cleaning up");
        cleanup(pid_path, socket_path);
        return false;
    }

    socket_path.exists()
}

/// Start the daemon as a detached background process.
///
/// Spawns `cella tunnel daemon` and returns immediately.
///
/// # Errors
///
/// Returns error if the daemon process cannot be spawned.
pub fn start_daemon_background(
    socket_path: &Path,
    pid_path: &Path,
) -> Result<(), CellaTunnelError> {
    let exe = std::env::current_exe().map_err(|e| CellaTunnelError::PidFile {
        message: format!("failed to get current exe: {e}"),
    })?;

    // Log daemon output to a file next to the PID file for debugging
    let stderr_cfg = pid_path
        .parent()
        .map(|p| p.join("tunnel-daemon.log"))
        .and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        })
        .map_or_else(std::process::Stdio::null, std::process::Stdio::from);

    std::process::Command::new(exe)
        .args([
            "tunnel",
            "daemon",
            "--socket",
            &socket_path.to_string_lossy(),
            "--pid-file",
            &pid_path.to_string_lossy(),
        ])
        .env("RUST_LOG", "info")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_cfg)
        .spawn()
        .map_err(|e| CellaTunnelError::PidFile {
            message: format!("failed to spawn daemon: {e}"),
        })?;

    info!("Tunnel daemon started in background");
    Ok(())
}

/// Ensure the tunnel daemon is running.
///
/// # Errors
///
/// Returns error if the daemon cannot be started.
pub fn ensure_daemon_running(
    socket_path: &Path,
    pid_path: &Path,
) -> Result<PathBuf, CellaTunnelError> {
    if is_daemon_running(pid_path, socket_path) {
        debug!("Tunnel daemon already running");
        return Ok(socket_path.to_path_buf());
    }

    start_daemon_background(socket_path, pid_path)?;

    // Brief wait for the daemon to create its socket
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if socket_path.exists() {
            return Ok(socket_path.to_path_buf());
        }
    }

    warn!("Tunnel daemon started but socket not yet available");
    Ok(socket_path.to_path_buf())
}

/// Stop the running daemon.
///
/// # Errors
///
/// Returns `CellaTunnelError::NotRunning` if no daemon is running.
pub fn stop_daemon(pid_path: &Path, socket_path: &Path) -> Result<(), CellaTunnelError> {
    let pid = read_pid_file(pid_path).ok_or(CellaTunnelError::NotRunning)?;

    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }

    cleanup(pid_path, socket_path);
    info!("Tunnel daemon stopped");
    Ok(())
}

fn read_pid_file(pid_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(pid_path).ok()?;
    content.trim().parse().ok()
}

fn unsafe_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        #[allow(unsafe_code, clippy::cast_possible_wrap)]
        unsafe {
            libc_kill(pid as i32, 0) == 0
        }
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe extern "C" {
        safe fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig)
}

fn is_docker_reachable() -> bool {
    std::process::Command::new("docker")
        .args(["info", "--format", "{{.ID}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn cleanup(pid_path: &Path, socket_path: &Path) {
    let _ = std::fs::remove_file(pid_path);
    let _ = std::fs::remove_file(socket_path);
}

/// Get the current time in seconds since the Unix epoch.
pub fn current_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pid_file_valid() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        std::fs::write(&pid_path, "12345").unwrap();
        assert_eq!(read_pid_file(&pid_path), Some(12345));
    }

    #[test]
    fn read_pid_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("nonexistent.pid");
        assert_eq!(read_pid_file(&pid_path), None);
    }

    #[test]
    fn read_pid_file_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("bad.pid");
        std::fs::write(&pid_path, "not-a-number").unwrap();
        assert_eq!(read_pid_file(&pid_path), None);
    }

    #[test]
    fn cleanup_removes_files() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        let socket_path = dir.path().join("test.sock");
        std::fs::write(&pid_path, "12345").unwrap();
        std::fs::write(&socket_path, "").unwrap();

        cleanup(&pid_path, &socket_path);

        assert!(!pid_path.exists());
        assert!(!socket_path.exists());
    }

    #[test]
    fn daemon_not_running_with_no_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        let socket_path = dir.path().join("test.sock");
        assert!(!is_daemon_running(&pid_path, &socket_path));
    }
}
