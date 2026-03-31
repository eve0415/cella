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
