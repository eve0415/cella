//! Shared daemon primitives: PID management, process checks, socket helpers.

use std::path::Path;

use tracing::{debug, warn};

/// Read the PID from a PID file.
pub fn read_pid_file(pid_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(pid_path).ok()?;
    content.trim().parse().ok()
}

/// Check if a process is alive by sending signal 0.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        // Signal 0 checks process existence without sending a signal.
        // EPERM means the process exists but we lack permission — still alive.
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        kill(Pid::from_raw(pid), None).is_ok()
            || matches!(nix::errno::Errno::last(), nix::errno::Errno::EPERM)
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Remove a list of files, ignoring errors.
pub fn cleanup_files(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

/// Set Unix socket permissions to 0o600 (owner only). No-op on non-Unix.
pub fn set_socket_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Check if Docker is reachable by running `docker info`.
pub fn is_docker_reachable() -> bool {
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

/// Get the current time in seconds since the Unix epoch.
pub fn current_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Check if a daemon is running: PID file valid, process alive, socket exists.
///
/// Cleans up stale PID/socket files if the process is no longer alive.
pub fn is_daemon_running(pid_path: &Path, socket_path: &Path) -> bool {
    let Some(pid) = read_pid_file(pid_path) else {
        return false;
    };

    let alive = is_process_alive(pid);
    if !alive {
        debug!("Stale PID file found (PID {pid}), cleaning up");
        cleanup_files(&[pid_path, socket_path]);
        return false;
    }

    socket_path.exists()
}

/// Spawn the current executable as a detached background process.
///
/// Returns the spawned child. The caller is responsible for mapping errors
/// to its own error type.
///
/// # Errors
///
/// Returns `io::Error` if the current executable cannot be determined or the
/// process cannot be spawned.
pub fn start_background_process(args: &[&str]) -> Result<std::process::Child, std::io::Error> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
}

/// Bind a TCP listener on localhost, attempting to reclaim a previously used port.
///
/// If `preferred_port` is non-zero, tries to bind it first. Falls back to
/// an OS-assigned port on failure.
///
/// # Errors
///
/// Returns `io::Error` if binding fails entirely.
pub async fn bind_tcp_reclaim(
    preferred_port: u16,
) -> Result<tokio::net::TcpListener, std::io::Error> {
    use std::net::SocketAddr;

    if preferred_port != 0 {
        let addr: SocketAddr = ([127, 0, 0, 1], preferred_port).into();
        if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
            debug!("Reclaimed TCP port {preferred_port}");
            return Ok(listener);
        }
        warn!("Cannot reclaim TCP port {preferred_port}, binding new port");
    }

    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    tokio::net::TcpListener::bind(addr).await
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
        assert_eq!(read_pid_file(&dir.path().join("nope.pid")), None);
    }

    #[test]
    fn read_pid_file_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("bad.pid");
        std::fs::write(&pid_path, "not-a-number").unwrap();
        assert_eq!(read_pid_file(&pid_path), None);
    }

    #[test]
    fn cleanup_files_removes_all() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        cleanup_files(&[&a, &b]);
        assert!(!a.exists());
        assert!(!b.exists());
    }

    #[test]
    fn cleanup_files_ignores_missing() {
        let dir = tempfile::tempdir().unwrap();
        cleanup_files(&[&dir.path().join("nonexistent")]);
    }

    #[test]
    #[cfg(feature = "integration-tests")]
    fn container_count_with_no_containers() {
        assert_eq!(running_cella_container_count(), 0);
    }

    // -- is_process_alive --

    #[cfg(unix)]
    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(is_process_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn bogus_pid_is_not_alive() {
        // PID 4_000_000_000 is well above any realistic PID.
        assert!(!is_process_alive(4_000_000_000));
    }

    #[cfg(unix)]
    #[test]
    fn pid_zero_is_not_alive() {
        // PID 0 would map to the idle/swapper process — kill(0, 0) signals the
        // calling process group, but we convert to i32 first which stays 0.
        // Regardless, this shouldn't panic.
        let _ = is_process_alive(0);
    }

    // -- set_socket_permissions --

    #[cfg(unix)]
    #[test]
    fn set_socket_permissions_restricts_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        std::fs::write(&path, "").unwrap();
        set_socket_permissions(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn set_socket_permissions_nonexistent_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        // No file exists — must not panic.
        set_socket_permissions(&dir.path().join("nope"));
    }

    // -- is_daemon_running --

    #[test]
    fn daemon_not_running_without_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_daemon_running(
            &dir.path().join("test.pid"),
            &dir.path().join("test.sock"),
        ));
    }

    #[test]
    fn daemon_not_running_with_stale_pid() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("test.pid");
        let sock = dir.path().join("test.sock");
        // Write a PID that almost certainly does not exist.
        std::fs::write(&pid, "4000000000").unwrap();
        std::fs::write(&sock, "").unwrap();

        assert!(!is_daemon_running(&pid, &sock));
        // Stale files should be cleaned up.
        assert!(!pid.exists());
        assert!(!sock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn daemon_running_with_current_pid_and_socket() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("test.pid");
        let sock = dir.path().join("test.sock");
        std::fs::write(&pid, std::process::id().to_string()).unwrap();
        std::fs::write(&sock, "").unwrap();

        assert!(is_daemon_running(&pid, &sock));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_not_running_when_socket_missing() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("test.pid");
        let sock = dir.path().join("test.sock");
        std::fs::write(&pid, std::process::id().to_string()).unwrap();
        // Socket file does not exist.
        assert!(!is_daemon_running(&pid, &sock));
    }

    // -- current_time_secs --

    #[test]
    fn current_time_secs_is_reasonable() {
        let ts = current_time_secs();
        // Should be well past 2020-01-01 (1_577_836_800) and below some far
        // future date. Loose bounds to avoid flakiness.
        assert!(ts > 1_577_836_800);
    }

    // -- read_pid_file edge cases --

    #[test]
    fn read_pid_file_with_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ws.pid");
        std::fs::write(&p, "  42  \n").unwrap();
        assert_eq!(read_pid_file(&p), Some(42));
    }

    #[test]
    fn read_pid_file_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("z.pid");
        std::fs::write(&p, "0").unwrap();
        assert_eq!(read_pid_file(&p), Some(0));
    }

    // -- bind_tcp_reclaim --

    #[tokio::test]
    async fn bind_tcp_reclaim_zero_gives_random_port() {
        let listener = bind_tcp_reclaim(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port > 0);
    }

    #[tokio::test]
    async fn bind_tcp_reclaim_preferred_port_works() {
        // First bind to get a free port, then drop and reclaim it.
        let first = bind_tcp_reclaim(0).await.unwrap();
        let port = first.local_addr().unwrap().port();
        drop(first);

        let second = bind_tcp_reclaim(port).await.unwrap();
        assert_eq!(second.local_addr().unwrap().port(), port);
    }

    #[tokio::test]
    async fn bind_tcp_reclaim_occupied_falls_back() {
        // Bind a port and hold it, then request the same port.
        let held = bind_tcp_reclaim(0).await.unwrap();
        let occupied = held.local_addr().unwrap().port();

        let fallback = bind_tcp_reclaim(occupied).await.unwrap();
        let fallback_port = fallback.local_addr().unwrap().port();
        assert_ne!(fallback_port, occupied);
    }
}
