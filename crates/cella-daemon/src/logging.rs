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
