use serde::Deserialize;

const fn default_true() -> bool {
    true
}

/// Tmux tool settings.
///
/// Controls config forwarding for tmux inside dev containers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tmux {
    /// Forward tmux config from host into the container (default: true).
    #[serde(default = "default_true")]
    pub forward_config: bool,

    /// Override host config source path (default: `~/.tmux.conf` or `~/.config/tmux/`).
    pub config_path: Option<String>,
}

impl Default for Tmux {
    fn default() -> Self {
        Self {
            forward_config: true,
            config_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_forwarding() {
        let settings = Tmux::default();
        assert!(settings.forward_config);
        assert!(settings.config_path.is_none());
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Tmux = toml::from_str("").unwrap();
        assert!(settings.forward_config);
        assert!(settings.config_path.is_none());
    }

    #[test]
    fn deserialize_disabled() {
        let settings: Tmux = toml::from_str("forward_config = false").unwrap();
        assert!(!settings.forward_config);
    }

    #[test]
    fn deserialize_custom_config_path() {
        let settings: Tmux = toml::from_str("config_path = \"~/dotfiles/tmux.conf\"").unwrap();
        assert_eq!(
            settings.config_path.as_deref(),
            Some("~/dotfiles/tmux.conf")
        );
    }

    #[test]
    fn deserialize_all_fields() {
        let settings: Tmux =
            toml::from_str("forward_config = false\nconfig_path = \"~/dotfiles/.tmux.conf\"")
                .unwrap();
        assert!(!settings.forward_config);
        assert_eq!(
            settings.config_path.as_deref(),
            Some("~/dotfiles/.tmux.conf")
        );
    }
}
