//! Claude Code config detection and container path helpers.
//!
//! Detects host `~/.claude/` config directory and `~/.claude.json` for
//! bind-mounting into containers. Provides path helpers for computing
//! container-side paths based on the remote user.

use std::path::PathBuf;

/// Container home path for a given user.
pub fn container_home(remote_user: &str) -> String {
    if remote_user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{remote_user}")
    }
}

/// Container-side `~/.claude` directory path.
pub fn claude_dir_for_user(remote_user: &str) -> String {
    format!("{}/.claude", container_home(remote_user))
}

/// Host-side `~/.claude` directory path (if it exists).
pub fn host_claude_dir() -> Option<PathBuf> {
    let home = home_dir()?;
    let dir = home.join(".claude");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Host-side `~/.claude.json` file path (if it exists).
pub fn host_claude_json_path() -> Option<PathBuf> {
    let home = home_dir()?;
    let path = home.join(".claude.json");
    if path.is_file() { Some(path) } else { None }
}

/// Host home directory derived from the host `.claude` directory path.
///
/// Returns `None` if `~/.claude/` doesn't exist on the host.
pub fn host_home() -> Option<PathBuf> {
    host_claude_dir().and_then(|d| d.parent().map(PathBuf::from))
}

/// Get the host home directory.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_home_root() {
        assert_eq!(container_home("root"), "/root");
    }

    #[test]
    fn container_home_regular() {
        assert_eq!(container_home("vscode"), "/home/vscode");
    }

    #[test]
    fn claude_dir_for_root() {
        assert_eq!(claude_dir_for_user("root"), "/root/.claude");
    }

    #[test]
    fn claude_dir_for_regular() {
        assert_eq!(claude_dir_for_user("vscode"), "/home/vscode/.claude");
    }

    #[test]
    fn host_home_strips_claude_suffix() {
        // host_home() depends on the actual filesystem, so we test the logic
        // indirectly: if host_claude_dir() returns Some, host_home() returns its parent.
        if let Some(claude_dir) = host_claude_dir() {
            let home = host_home().expect("host_home should return Some when host_claude_dir does");
            assert_eq!(home, claude_dir.parent().unwrap());
        }
    }
}
