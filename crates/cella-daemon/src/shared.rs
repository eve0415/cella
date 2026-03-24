//! Shared daemon primitives: PID management, process checks, socket helpers.
//!
//! Extracted from `cella-daemon` and `cella-credential-proxy` to eliminate
//! duplication. Both crates import from this module.

use std::path::Path;

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

/// Common daemon lifecycle trait.
///
/// Provides default implementations for liveness checking and cleanup
/// based on PID and socket paths.
pub trait DaemonLifecycle {
    /// Path to the PID file.
    fn pid_path(&self) -> &Path;

    /// Path to the Unix socket.
    fn socket_path(&self) -> &Path;

    /// Human-readable daemon name for logging.
    fn name(&self) -> &str;

    /// Check if this daemon is running (PID alive + socket exists).
    fn is_running(&self) -> bool {
        read_pid_file(self.pid_path()).is_some_and(is_process_alive) && self.socket_path().exists()
    }

    /// Remove PID and socket files.
    fn cleanup(&self) {
        cleanup_files(&[self.pid_path(), self.socket_path()]);
    }
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
    #[ignore = "requires Docker"]
    fn container_count_with_no_containers() {
        assert_eq!(running_cella_container_count(), 0);
    }
}
