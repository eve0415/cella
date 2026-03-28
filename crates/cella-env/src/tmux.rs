//! Tmux config detection and container path helpers.
//!
//! Detects host `~/.tmux.conf` and `~/.config/tmux/` for bind-mounting
//! into containers. Supports custom config path overrides.

use std::path::PathBuf;

use crate::claude_code::container_home;

/// Host-side tmux config file (`~/.tmux.conf`).
///
/// If `config_path` is provided and points to a file, returns that.
/// Otherwise checks `~/.tmux.conf`.
pub fn host_tmux_conf(config_path: Option<&str>) -> Option<PathBuf> {
    if let Some(custom) = config_path {
        let expanded = expand_tilde(custom);
        if expanded.is_file() {
            return Some(expanded);
        }
        return None;
    }
    let home = home_dir()?;
    let path = home.join(".tmux.conf");
    if path.is_file() { Some(path) } else { None }
}

/// Host-side tmux XDG config directory (`~/.config/tmux/`).
///
/// If `config_path` is provided and points to a directory, returns that.
/// Otherwise checks `~/.config/tmux/`.
pub fn host_tmux_config_dir(config_path: Option<&str>) -> Option<PathBuf> {
    if let Some(custom) = config_path {
        let expanded = expand_tilde(custom);
        if expanded.is_dir() {
            return Some(expanded);
        }
        return None;
    }
    let home = home_dir()?;
    let dir = home.join(".config").join("tmux");
    if dir.is_dir() { Some(dir) } else { None }
}

/// Container-side `~/.tmux.conf` path.
pub fn container_tmux_conf(remote_user: &str) -> String {
    format!("{}/.tmux.conf", container_home(remote_user))
}

/// Container-side `~/.config/tmux/` directory path.
pub fn container_tmux_config_dir(remote_user: &str) -> String {
    format!("{}/.config/tmux", container_home(remote_user))
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Get the host home directory.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_conf_non_root() {
        assert_eq!(container_tmux_conf("vscode"), "/home/vscode/.tmux.conf");
    }

    #[test]
    fn container_conf_root() {
        assert_eq!(container_tmux_conf("root"), "/root/.tmux.conf");
    }

    #[test]
    fn container_config_dir_non_root() {
        assert_eq!(
            container_tmux_config_dir("vscode"),
            "/home/vscode/.config/tmux"
        );
    }

    #[test]
    fn container_config_dir_root() {
        assert_eq!(container_tmux_config_dir("root"), "/root/.config/tmux");
    }

    #[test]
    fn host_conf_missing_returns_none() {
        assert!(host_tmux_conf(Some("/nonexistent/.tmux.conf")).is_none());
    }

    #[test]
    fn host_config_dir_missing_returns_none() {
        assert!(host_tmux_config_dir(Some("/nonexistent/tmux")).is_none());
    }

    #[test]
    fn expand_tilde_with_home() {
        let expanded = expand_tilde("~/dotfiles/tmux.conf");
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expanded,
            PathBuf::from(format!("{home}/dotfiles/tmux.conf"))
        );
    }

    #[test]
    fn expand_tilde_absolute_path() {
        let expanded = expand_tilde("/absolute/path");
        assert_eq!(expanded, PathBuf::from("/absolute/path"));
    }
}
