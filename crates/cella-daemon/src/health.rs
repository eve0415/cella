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

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // check_idle_timeout — core logic tests
    // ---------------------------------------------------------------

    #[test]
    fn no_timeout_when_recently_active() {
        let now = current_time_secs();
        let last_activity = AtomicU64::new(now);
        // Just set the activity to "now" — should not time out.
        assert!(!check_idle_timeout(&last_activity));
    }

    #[test]
    fn no_timeout_at_exactly_threshold() {
        let now = current_time_secs();
        // Set last activity to exactly IDLE_TIMEOUT_SECS ago.
        // elapsed == IDLE_TIMEOUT_SECS, which is <= so should NOT time out.
        let last_activity = AtomicU64::new(now.saturating_sub(IDLE_TIMEOUT_SECS));
        assert!(!check_idle_timeout(&last_activity));
    }

    // Note: We cannot easily test the "timeout exceeded + no containers" path
    // without mocking running_cella_container_count(). However, we can test
    // that check_idle_timeout returns false when last_activity is recent.

    #[test]
    fn no_timeout_one_second_ago() {
        let now = current_time_secs();
        let last_activity = AtomicU64::new(now.saturating_sub(1));
        assert!(!check_idle_timeout(&last_activity));
    }

    #[test]
    fn no_timeout_half_of_threshold() {
        let now = current_time_secs();
        let last_activity = AtomicU64::new(now.saturating_sub(IDLE_TIMEOUT_SECS / 2));
        assert!(!check_idle_timeout(&last_activity));
    }

    #[test]
    fn no_timeout_future_activity() {
        // If last_activity is in the future (clock skew), saturating_sub gives 0.
        let now = current_time_secs();
        let last_activity = AtomicU64::new(now + 1000);
        assert!(!check_idle_timeout(&last_activity));
    }

    // ---------------------------------------------------------------
    // Constants
    // ---------------------------------------------------------------

    #[test]
    fn idle_timeout_is_30_minutes() {
        assert_eq!(IDLE_TIMEOUT_SECS, 30 * 60);
        assert_eq!(IDLE_TIMEOUT_SECS, 1800);
    }

    #[test]
    fn health_check_interval_is_5_minutes() {
        assert_eq!(HEALTH_CHECK_INTERVAL_SECS, 5 * 60);
        assert_eq!(HEALTH_CHECK_INTERVAL_SECS, 300);
    }
}
