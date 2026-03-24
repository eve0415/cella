//! Health monitoring: idle timeout and container liveness checks.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tracing::{debug, info};

use crate::shared::{
    cleanup_files, current_time_secs, is_docker_reachable, running_cella_container_count,
};

/// Default idle timeout (30 minutes).
const IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// Health check interval (5 minutes).
const HEALTH_CHECK_INTERVAL_SECS: u64 = 5 * 60;

/// Check if the idle timeout has been exceeded.
///
/// Returns `true` if the daemon should shut down due to idle timeout.
/// If containers are still running, resets the activity timer and returns `false`.
fn check_idle_timeout(last_activity: &AtomicU64) -> bool {
    let last = last_activity.load(Ordering::Relaxed);
    let now = current_time_secs();
    let elapsed = now.saturating_sub(last);

    if elapsed <= IDLE_TIMEOUT_SECS {
        return false;
    }

    let count = running_cella_container_count();
    if count > 0 {
        debug!("Idle timeout reached but {count} cella container(s) still running, staying alive");
        last_activity.store(current_time_secs(), Ordering::Relaxed);
        return false;
    }

    info!("Idle timeout ({IDLE_TIMEOUT_SECS}s) reached, shutting down");
    true
}

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

        if check_idle_timeout(&last_activity) {
            cleanup_files(&[&pid_path, &socket_path]);
            std::process::exit(0);
        }

        if !is_docker_reachable() {
            info!("Docker daemon not reachable, shutting down");
            cleanup_files(&[&pid_path, &socket_path]);
            std::process::exit(0);
        }

        let elapsed = current_time_secs().saturating_sub(last_activity.load(Ordering::Relaxed));
        debug!("Health check passed (idle {elapsed}s)");
    }
}
