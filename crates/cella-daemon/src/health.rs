//! Health monitoring: idle timeout and container liveness checks.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tracing::{debug, info};

use crate::control_server::current_time_secs;

/// Default idle timeout (30 minutes).
const IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// Health check interval (5 minutes).
const HEALTH_CHECK_INTERVAL_SECS: u64 = 5 * 60;

/// Run the health monitor loop.
///
/// Checks for idle timeout and Docker reachability, shutting down
/// the daemon when appropriate.
pub async fn run_health_monitor(
    last_activity: Arc<AtomicU64>,
    pid_path: &Path,
    socket_path: &Path,
) {
    let pid_path = pid_path.to_path_buf();
    let socket_path = socket_path.to_path_buf();

    loop {
        tokio::time::sleep(Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS)).await;

        let last = last_activity.load(Ordering::Relaxed);
        let now = current_time_secs();
        let elapsed = now.saturating_sub(last);

        if elapsed > IDLE_TIMEOUT_SECS {
            let count = running_cella_container_count();
            if count > 0 {
                debug!(
                    "Idle timeout reached but {count} cella container(s) still running, staying alive"
                );
                last_activity.store(current_time_secs(), Ordering::Relaxed);
                continue;
            }
            info!("Idle timeout ({IDLE_TIMEOUT_SECS}s) reached, shutting down");
            cleanup_runtime(&pid_path, &socket_path);
            std::process::exit(0);
        }

        if !is_docker_reachable() {
            info!("Docker daemon not reachable, shutting down");
            cleanup_runtime(&pid_path, &socket_path);
            std::process::exit(0);
        }

        debug!("Health check passed (idle {elapsed}s)");
    }
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

/// Clean up PID file and socket.
fn cleanup_runtime(pid_path: &Path, socket_path: &Path) {
    let _ = std::fs::remove_file(pid_path);
    let _ = std::fs::remove_file(socket_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires Docker"]
    fn container_count_with_no_containers() {
        assert_eq!(running_cella_container_count(), 0);
    }
}
