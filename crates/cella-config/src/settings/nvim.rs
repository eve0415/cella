use serde::Deserialize;

const fn default_true() -> bool {
    true
}

fn default_stable() -> String {
    "stable".to_string()
}

/// Neovim tool settings.
///
/// Controls config forwarding and on-demand installation version for nvim
/// inside dev containers.
#[derive(Debug, Clone, Deserialize)]
pub struct Nvim {
    /// Forward nvim config from host into the container (default: true).
    #[serde(default = "default_true")]
    pub forward_config: bool,

    /// Version to install on-demand: `"stable"`, `"nightly"`, or pinned e.g. `"0.10.3"`.
    #[serde(default = "default_stable")]
    pub version: String,

    /// Override host config source directory (default: `~/.config/nvim`).
    pub config_path: Option<String>,
}

impl Default for Nvim {
    fn default() -> Self {
        Self {
            forward_config: true,
            version: "stable".to_string(),
            config_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_forwarding() {
        let settings = Nvim::default();
        assert!(settings.forward_config);
        assert_eq!(settings.version, "stable");
        assert!(settings.config_path.is_none());
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Nvim = toml::from_str("").unwrap();
        assert!(settings.forward_config);
        assert_eq!(settings.version, "stable");
        assert!(settings.config_path.is_none());
    }

    #[test]
    fn deserialize_disabled() {
        let settings: Nvim = toml::from_str("forward_config = false").unwrap();
        assert!(!settings.forward_config);
    }

    #[test]
    fn deserialize_pinned_version() {
        let settings: Nvim = toml::from_str("version = \"0.10.3\"").unwrap();
        assert_eq!(settings.version, "0.10.3");
    }

    #[test]
    fn deserialize_nightly_version() {
        let settings: Nvim = toml::from_str("version = \"nightly\"").unwrap();
        assert_eq!(settings.version, "nightly");
    }

    #[test]
    fn deserialize_custom_config_path() {
        let settings: Nvim = toml::from_str("config_path = \"~/dotfiles/nvim\"").unwrap();
        assert_eq!(settings.config_path.as_deref(), Some("~/dotfiles/nvim"));
    }

    #[test]
    fn deserialize_all_fields() {
        let settings: Nvim = toml::from_str(
            "forward_config = false\nversion = \"0.10.3\"\nconfig_path = \"~/my-nvim\"",
        )
        .unwrap();
        assert!(!settings.forward_config);
        assert_eq!(settings.version, "0.10.3");
        assert_eq!(settings.config_path.as_deref(), Some("~/my-nvim"));
    }
}
