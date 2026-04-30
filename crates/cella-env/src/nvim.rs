//! Neovim config detection and container path helpers.
//!
//! Detects host `~/.config/nvim/` config directory for bind-mounting
//! into containers. Supports custom config path overrides.

use std::path::PathBuf;

use crate::claude_code::container_home;
use crate::paths::{expand_tilde, home_dir};

/// Host-side nvim config directory.
///
/// If `config_path` is provided, expands `~` and returns that path (if it exists).
/// Otherwise checks the default `~/.config/nvim/`.
pub fn host_nvim_config_dir(config_path: Option<&str>) -> Option<PathBuf> {
    if let Some(custom) = config_path {
        let expanded = expand_tilde(custom);
        if expanded.is_dir() {
            Some(expanded)
        } else {
            None
        }
    } else {
        let home = home_dir()?;
        let dir = home.join(".config").join("nvim");
        if dir.is_dir() { Some(dir) } else { None }
    }
}

/// Container-side `~/.config/nvim` directory path.
pub fn container_nvim_config_dir(remote_user: &str) -> String {
    format!("{}/.config/nvim", container_home(remote_user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_path_non_root() {
        assert_eq!(
            container_nvim_config_dir("vscode"),
            "/home/vscode/.config/nvim"
        );
    }

    #[test]
    fn container_path_root() {
        assert_eq!(container_nvim_config_dir("root"), "/root/.config/nvim");
    }

    #[test]
    fn host_config_missing_returns_none() {
        assert!(host_nvim_config_dir(Some("/nonexistent/path/nvim")).is_none());
    }

    #[test]
    fn expand_tilde_with_home() {
        let expanded = expand_tilde("~/dotfiles/nvim");
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expanded, PathBuf::from(format!("{home}/dotfiles/nvim")));
    }

    #[test]
    fn expand_tilde_absolute_path() {
        let expanded = expand_tilde("/absolute/path");
        assert_eq!(expanded, PathBuf::from("/absolute/path"));
    }
}
