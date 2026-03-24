//! Google Gemini CLI host detection and container path helpers.
//!
//! Detects host `~/.gemini/` directory for bind-mount forwarding.

use std::path::PathBuf;

use crate::claude_code::container_home;

/// Host-side `~/.gemini` directory path (if it exists).
pub fn host_gemini_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().map(PathBuf::from)?;
    let dir = home.join(".gemini");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Container-side `~/.gemini` directory path for a given user.
pub fn container_gemini_dir(remote_user: &str) -> String {
    format!("{}/.gemini", container_home(remote_user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_gemini_dir_root() {
        assert_eq!(container_gemini_dir("root"), "/root/.gemini");
    }

    #[test]
    fn container_gemini_dir_regular() {
        assert_eq!(container_gemini_dir("vscode"), "/home/vscode/.gemini");
    }
}
