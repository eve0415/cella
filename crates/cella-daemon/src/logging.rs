//! Daemon file-based logging.
//!
//! Initializes a `tracing` subscriber that writes to `~/.cella/daemon.log`.
//! Uses `CELLA_DAEMON_LOG` (or `RUST_LOG`) for filtering, defaulting to `info`.

use std::fs::{File, OpenOptions};
use std::path::Path;

use tracing_subscriber::EnvFilter;

/// Maximum log file size before truncation (5 MB).
const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024;

/// Initialize file-based tracing for the daemon process.
///
/// Writes to the given log path. If the file exceeds [`MAX_LOG_SIZE`],
/// it is truncated before opening.
///
/// Filter priority: `CELLA_DAEMON_LOG` > `RUST_LOG` > default `info`.
pub fn init_daemon_logging(log_path: &Path) {
    // Truncate if over size limit.
    if std::fs::metadata(log_path).is_ok_and(|m| m.len() > MAX_LOG_SIZE) {
        let _ = File::create(log_path);
    }

    let file = OpenOptions::new().create(true).append(true).open(log_path);

    let Ok(file) = file else {
        // Can't open log file — fall back to no-op.
        return;
    };

    let filter = std::env::var("CELLA_DAEMON_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".to_string());

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(file)
        .with_ansi(false)
        .with_target(true)
        .compact();

    let _ = subscriber.try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn max_log_size_is_5mb() {
        assert_eq!(MAX_LOG_SIZE, 5 * 1024 * 1024);
    }

    #[test]
    fn init_logging_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        // try_init may fail if a global subscriber is already set (other tests),
        // but the file should still be created.
        init_daemon_logging(&log_path);
        assert!(log_path.exists());
    }

    #[test]
    fn init_logging_truncates_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("big.log");

        // Create a file larger than MAX_LOG_SIZE.
        #[allow(clippy::cast_possible_truncation)]
        let size = (MAX_LOG_SIZE as usize) + 1;
        let big = vec![b'x'; size];
        std::fs::write(&log_path, &big).unwrap();
        assert!(std::fs::metadata(&log_path).unwrap().len() > MAX_LOG_SIZE);

        init_daemon_logging(&log_path);

        // After truncation and re-open in append mode, the file should be
        // much smaller (truncated to 0 then appended nothing or header).
        let new_size = std::fs::metadata(&log_path).unwrap().len();
        assert!(
            new_size < MAX_LOG_SIZE,
            "expected truncated file, got {new_size} bytes"
        );
    }

    #[test]
    fn init_logging_does_not_truncate_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("small.log");
        let content = "some existing log data\n";
        std::fs::write(&log_path, content).unwrap();

        init_daemon_logging(&log_path);

        let after = std::fs::read_to_string(&log_path).unwrap();
        // The original content should still be present (append mode).
        assert!(
            after.contains(content.trim()),
            "expected original content preserved"
        );
    }

    #[test]
    fn init_logging_nonexistent_directory_does_not_panic() {
        let bad_path = PathBuf::from("/tmp/cella_test_nonexistent_dir_xyz/daemon.log");
        // Should not panic even if path is invalid.
        init_daemon_logging(&bad_path);
        // Clean up in case it did create something.
        let _ = std::fs::remove_file(&bad_path);
        let _ = std::fs::remove_dir("/tmp/cella_test_nonexistent_dir_xyz");
    }
}
