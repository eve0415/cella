//! Daemon lifecycle: PID file, daemonization, liveness check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::CellaCredentialProxyError;
use crate::server::{current_time_secs, run_server, run_tcp_server};

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
                cleanup_runtime(&pid_path_owned, &socket_path_owned);
                std::process::exit(0);
            }

            // Check if Docker daemon is reachable
            if !is_docker_reachable() {
                info!("Docker daemon not reachable, shutting down");
                cleanup_runtime(&pid_path_owned, &socket_path_owned);
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
    cleanup(pid_path, socket_path, port_path);

    result
}

/// Check if the daemon is already running.
///
/// Reads the PID file and checks if the process is alive.
pub fn is_daemon_running(pid_path: &Path, socket_path: &Path, _port_path: &Path) -> bool {
    let Some(pid) = read_pid_file(pid_path) else {
        return false;
    };

    // Check if process is alive (signal 0 = check existence)
    let alive = unsafe_process_alive(pid);
    if !alive {
        // Stale PID file — clean up (preserve port file for reuse)
        debug!("Stale PID file found (PID {pid}), cleaning up");
        cleanup_runtime(pid_path, socket_path);
        return false;
    }

    // Also verify the socket exists and is responsive
    socket_path.exists()
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
    let exe = std::env::current_exe().map_err(|e| CellaCredentialProxyError::PidFile {
        message: format!("failed to get current exe: {e}"),
    })?;

    std::process::Command::new(exe)
        .args([
            "credential-proxy",
            "daemon",
            "--socket",
            &socket_path.to_string_lossy(),
            "--pid-file",
            &pid_path.to_string_lossy(),
            "--port-file",
            &port_path.to_string_lossy(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| CellaCredentialProxyError::PidFile {
            message: format!("failed to spawn daemon: {e}"),
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
    if is_daemon_running(pid_path, socket_path, port_path) {
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
    let pid = read_pid_file(pid_path).ok_or(CellaCredentialProxyError::NotRunning)?;

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

    cleanup(pid_path, socket_path, port_path);
    info!("Credential proxy daemon stopped");
    Ok(())
}

/// Read the PID from the PID file.
fn read_pid_file(pid_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(pid_path).ok()?;
    content.trim().parse().ok()
}

/// Check if a process is alive by sending signal 0.
fn unsafe_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // kill(pid, 0) checks if process exists without sending a signal
        // SAFETY: We're only checking process existence via kill(pid, 0),
        // which is a standard POSIX operation that doesn't affect the process.
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

/// Minimal wrapper for kill(2) without pulling in libc crate.
#[cfg(unix)]
#[allow(unsafe_code)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe extern "C" {
        safe fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig)
}

/// Check if Docker is reachable by running `docker info`.
fn is_docker_reachable() -> bool {
    std::process::Command::new("docker")
        .args(["info", "--format", "{{.ID}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Count running containers managed by cella.
///
/// Returns 0 on any error (Docker unreachable, parse failure, etc.).
pub fn running_cella_container_count() -> usize {
    std::process::Command::new("docker")
        .args([
            "ps",
            "--filter",
            "label=dev.cella.tool=cella",
            "--filter",
            "status=running",
            "-q",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .map_or(0, |o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .count()
        })
}

/// Clean up PID file, socket, and port file.
fn cleanup(pid_path: &Path, socket_path: &Path, port_path: &Path) {
    let _ = std::fs::remove_file(pid_path);
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(port_path);
}

/// Clean up PID file and socket only, preserving the port file for reuse.
fn cleanup_runtime(pid_path: &Path, socket_path: &Path) {
    let _ = std::fs::remove_file(pid_path);
    let _ = std::fs::remove_file(socket_path);
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
    fn cleanup_removes_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        let socket_path = dir.path().join("test.sock");
        let port_path = dir.path().join("test.port");
        std::fs::write(&pid_path, "12345").unwrap();
        std::fs::write(&socket_path, "").unwrap();
        std::fs::write(&port_path, "54321").unwrap();

        cleanup(&pid_path, &socket_path, &port_path);

        assert!(!pid_path.exists());
        assert!(!socket_path.exists());
        assert!(!port_path.exists());
    }

    #[test]
    fn cleanup_runtime_preserves_port_file() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        let socket_path = dir.path().join("test.sock");
        let port_path = dir.path().join("test.port");
        std::fs::write(&pid_path, "12345").unwrap();
        std::fs::write(&socket_path, "").unwrap();
        std::fs::write(&port_path, "54321").unwrap();

        cleanup_runtime(&pid_path, &socket_path);

        assert!(!pid_path.exists());
        assert!(!socket_path.exists());
        assert!(port_path.exists());
    }

    #[test]
    #[ignore = "requires Docker"]
    fn running_container_count_with_no_containers() {
        // Docker-dependent: requires Docker to be reachable
        let count = running_cella_container_count();
        // When no cella containers are running, count should be 0
        assert_eq!(count, 0);
    }

    #[test]
    fn daemon_not_running_with_no_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        let socket_path = dir.path().join("test.sock");
        let port_path = dir.path().join("test.port");
        assert!(!is_daemon_running(&pid_path, &socket_path, &port_path));
    }
}
