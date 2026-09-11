//! `OpenAI` Codex CLI host detection and container path helpers.
//!
//! Detects host `~/.codex/` directory for bind-mount forwarding.

use std::path::PathBuf;

use crate::claude_code::container_home;

/// Host-side `~/.codex` directory path (if it exists).
pub fn host_codex_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from)?;
    let dir = home.join(".codex");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Container-side `~/.codex` directory path for a given user.
pub fn container_codex_dir(remote_user: &str) -> String {
    format!("{}/.codex", container_home(remote_user))
}

/// Container-side directory for Codex's `SQLite` databases.
///
/// Deliberately outside `~/.codex`: that path is a bind mount of the host directory, and Codex opens every database in WAL mode, which cannot survive accessors on two sides of the host/VM boundary.
pub fn container_codex_db_dir(remote_user: &str) -> String {
    format!("{}/.codex-db", container_home(remote_user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_codex_dir_root() {
        assert_eq!(container_codex_dir("root"), "/root/.codex");
    }

    #[test]
    fn container_codex_dir_regular() {
        assert_eq!(container_codex_dir("vscode"), "/home/vscode/.codex");
    }

    #[test]
    fn container_codex_db_dir_is_outside_the_forwarded_mount() {
        let db = container_codex_db_dir("vscode");
        assert_eq!(db, "/home/vscode/.codex-db");
        // Compare on a path boundary: `.codex-db` shares a string prefix with
        // `.codex` while being a sibling of it, not a child.
        assert!(!db.starts_with(&format!("{}/", container_codex_dir("vscode"))));
    }

    #[test]
    fn container_codex_db_dir_sits_directly_in_the_home_directory() {
        // Codex creates this path itself, as the remote user. Nesting it under a
        // directory cella writes during `up` (`~/.cella`, created root-owned by
        // the env-probe cache) would leave the remote user unable to create it.
        let home = container_home("vscode");
        let db = container_codex_db_dir("vscode");
        let relative = db.strip_prefix(&format!("{home}/")).expect("under home");
        assert!(
            !relative.contains('/'),
            "must be a direct child of {home}, got {db}"
        );
    }

    #[test]
    fn container_codex_db_dir_root() {
        assert_eq!(container_codex_db_dir("root"), "/root/.codex-db");
    }

    #[test]
    #[allow(unsafe_code)]
    fn test_host_codex_dir_returns_none_when_no_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let original_home = std::env::var("HOME").ok();
        // SAFETY: test-only; mutating env var in a single-threaded test context.
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let result = host_codex_dir();
        unsafe {
            match original_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        assert!(result.is_none());
    }
}
