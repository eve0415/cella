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

/// Host-side `~/.claude/plugins` directory path (if it exists).
pub fn host_plugins_dir() -> Option<PathBuf> {
    let dir = host_claude_dir()?.join("plugins");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Host home directory derived from the host `.claude` directory path.
///
/// Returns `None` if `~/.claude/` doesn't exist on the host.
pub fn host_home() -> Option<PathBuf> {
    host_claude_dir().and_then(|d| d.parent().map(PathBuf::from))
}

/// Replace home-path prefix in file content.
///
/// Performs a simple string replacement of `{from_home}/.claude` with
/// `{to_home}/.claude` for rewriting plugin manifest paths.
pub fn rewrite_claude_home(content: &str, from_home: &str, to_home: &str) -> String {
    content.replace(
        &format!("{from_home}/.claude"),
        &format!("{to_home}/.claude"),
    )
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

    #[test]
    fn rewrite_claude_home_replaces_paths() {
        let content = r#"{"installPath": "/home/node/.claude/plugins/cache/foo"}"#;
        let result = rewrite_claude_home(content, "/home/node", "/home/vscode");
        assert_eq!(
            result,
            r#"{"installPath": "/home/vscode/.claude/plugins/cache/foo"}"#
        );
    }

    #[test]
    fn rewrite_claude_home_multiple_occurrences() {
        let content = "/home/node/.claude/a /home/node/.claude/b";
        let result = rewrite_claude_home(content, "/home/node", "/home/vscode");
        assert_eq!(result, "/home/vscode/.claude/a /home/vscode/.claude/b");
    }

    #[test]
    fn rewrite_claude_home_noop_when_same() {
        let content = "/home/vscode/.claude/plugins";
        let result = rewrite_claude_home(content, "/home/vscode", "/home/vscode");
        assert_eq!(result, content);
    }

    #[test]
    fn rewrite_claude_home_macos_to_linux() {
        let content = r#"{"path": "/Users/alice/.claude/plugins"}"#;
        let result = rewrite_claude_home(content, "/Users/alice", "/home/vscode");
        assert_eq!(result, r#"{"path": "/home/vscode/.claude/plugins"}"#);
    }
}
