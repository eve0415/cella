//! JSONL audit logging for credential proxy requests.
//!
//! Writes structured log entries to `~/.cella/credential-audit.log` with
//! automatic rotation at 50 MB.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use tracing::error;

/// Maximum log file size before rotation (50 MB).
const DEFAULT_MAX_SIZE: u64 = 50 * 1024 * 1024;

/// A single credential proxy audit record, serialized as one JSONL line.
#[derive(Serialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub container_name: String,
    pub provider_id: String,
    pub domain: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_ms: u64,
    pub trace_id: String,
    pub denial_reason: Option<String>,
}

/// Append-only JSONL logger with single-file rotation.
pub struct AuditLogger {
    file: Mutex<File>,
    log_path: PathBuf,
    max_size: u64,
}

impl AuditLogger {
    /// Open (or create) the audit log at `path` in append mode.
    ///
    /// On Unix the file permissions are set to `0600` (owner read/write only).
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or permissions cannot be set.
    pub fn new(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = open_audit_file(&path)?;

        Ok(Self {
            file: Mutex::new(file),
            log_path: path,
            max_size: DEFAULT_MAX_SIZE,
        })
    }

    /// Write an audit entry as a single JSONL line.
    ///
    /// Failures are logged via `tracing::error` rather than propagated so that
    /// audit I/O never disrupts request handling.
    pub fn log(&self, entry: &AuditEntry) {
        let line = match serde_json::to_string(entry) {
            Ok(json) => json,
            Err(err) => {
                error!("audit: failed to serialize entry: {err}");
                return;
            }
        };

        let needs_rotation = {
            let Ok(mut guard) = self.file.lock() else {
                error!("audit: mutex poisoned");
                return;
            };
            if let Err(err) = writeln!(guard, "{line}") {
                error!("audit: write failed: {err}");
                return;
            }
            self.log_path
                .metadata()
                .is_ok_and(|m| m.len() > self.max_size)
        };

        if needs_rotation {
            self.rotate();
        }
    }

    /// Rotate the current log file to `<path>.1`, replacing any previous backup.
    pub fn rotate(&self) {
        let backup = rotated_path(&self.log_path);

        let Ok(mut guard) = self.file.lock() else {
            error!("audit: mutex poisoned during rotation");
            return;
        };

        // Remove old backup if present.
        let _ = fs::remove_file(&backup);

        if let Err(err) = fs::rename(&self.log_path, &backup) {
            error!("audit: rotation rename failed: {err}");
            return;
        }

        match open_audit_file(&self.log_path) {
            Ok(new_file) => *guard = new_file,
            Err(err) => error!("audit: failed to create new log after rotation: {err}"),
        }
    }

    /// Strip the query string from a URL path.
    ///
    /// Everything from the first `?` onward is removed.
    #[must_use]
    pub fn strip_query(path: &str) -> &str {
        path.split_once('?').map_or(path, |(base, _)| base)
    }
}

/// Default audit log path: `~/.cella/credential-audit.log`.
#[must_use]
pub fn default_audit_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".cella").join("credential-audit.log"))
}

/// Build the rotated backup path by appending `.1` to the file name.
fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "audit.log".into(), std::ffi::OsStr::to_os_string);
    name.push(".1");
    path.with_file_name(name)
}

/// Open or create the audit log file in append mode with `0600` permissions on Unix.
fn open_audit_file(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> AuditEntry {
        AuditEntry {
            timestamp: "2026-05-25T12:00:00.000Z".to_string(),
            container_name: "cella-myapp-main".to_string(),
            provider_id: "github".to_string(),
            domain: "github.com".to_string(),
            method: "GET".to_string(),
            path: "/v1/credentials".to_string(),
            status: 200,
            duration_ms: 42,
            trace_id: "abc-123".to_string(),
            denial_reason: None,
        }
    }

    #[test]
    fn serialization_format() {
        let entry = sample_entry();
        let json = serde_json::to_string(&entry).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["timestamp"], "2026-05-25T12:00:00.000Z");
        assert_eq!(value["container_name"], "cella-myapp-main");
        assert_eq!(value["provider_id"], "github");
        assert_eq!(value["domain"], "github.com");
        assert_eq!(value["method"], "GET");
        assert_eq!(value["path"], "/v1/credentials");
        assert_eq!(value["status"], 200);
        assert_eq!(value["duration_ms"], 42);
        assert_eq!(value["trace_id"], "abc-123");
        assert!(value["denial_reason"].is_null());
    }

    #[test]
    fn serialization_with_denial() {
        let entry = AuditEntry {
            denial_reason: Some("domain_not_allowed".to_string()),
            ..sample_entry()
        };
        let json = serde_json::to_string(&entry).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["denial_reason"], "domain_not_allowed");
    }

    #[test]
    fn strip_query_no_query() {
        assert_eq!(AuditLogger::strip_query("/v1/messages"), "/v1/messages");
    }

    #[test]
    fn strip_query_with_single_param() {
        assert_eq!(
            AuditLogger::strip_query("/v1/messages?key=val"),
            "/v1/messages"
        );
    }

    #[test]
    fn strip_query_with_multiple_params() {
        assert_eq!(AuditLogger::strip_query("/path?a=1&b=2"), "/path");
    }

    #[test]
    fn strip_query_empty_string() {
        assert_eq!(AuditLogger::strip_query(""), "");
    }

    #[test]
    fn strip_query_only() {
        assert_eq!(AuditLogger::strip_query("?query_only"), "");
    }

    #[test]
    fn log_writes_jsonl_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let logger = AuditLogger::new(path.clone()).unwrap();

        logger.log(&sample_entry());

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);

        let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(value["container_name"], "cella-myapp-main");
    }

    #[test]
    fn rotation_at_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential-audit.log");

        let logger = AuditLogger {
            file: Mutex::new(open_audit_file(&path).unwrap()),
            log_path: path.clone(),
            max_size: 100,
        };

        // Write entries until we exceed 100 bytes.
        for _ in 0..10 {
            logger.log(&sample_entry());
        }

        let backup = rotated_path(&path);
        assert!(backup.exists(), "backup file should exist after rotation");
        assert!(path.exists(), "main log should still exist after rotation");

        let main_size = path.metadata().unwrap().len();
        assert!(
            main_size < logger.max_size,
            "main log ({main_size}) should be smaller than max ({})",
            logger.max_size
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let _ = AuditLogger::new(path.clone()).unwrap();

        let mode = path.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "audit log should be owner-only (0600)");
    }

    #[test]
    fn default_audit_path_under_home() {
        if let Some(path) = default_audit_path() {
            assert!(path.ends_with(".cella/credential-audit.log"));
        }
    }

    #[test]
    fn rotated_path_appends_dot_one() {
        let path = PathBuf::from("/tmp/credential-audit.log");
        let backup = rotated_path(&path);
        assert_eq!(backup, PathBuf::from("/tmp/credential-audit.log.1"));
    }
}
