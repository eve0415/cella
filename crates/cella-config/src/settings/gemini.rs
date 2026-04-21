use serde::Deserialize;

const fn default_true() -> bool {
    true
}

fn default_latest() -> String {
    "latest".to_string()
}

/// Google Gemini CLI tool settings.
///
/// Controls automatic installation and config forwarding of the Gemini CLI
/// into dev containers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gemini {
    /// Install Gemini CLI in the container (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Bind-mount `~/.gemini` from host into the container (default: true).
    #[serde(default = "default_true")]
    pub forward_config: bool,

    /// Version to install: `"latest"` or pinned e.g. `"0.1.2"`.
    #[serde(default = "default_latest")]
    pub version: String,
}

impl Default for Gemini {
    fn default() -> Self {
        Self {
            enabled: true,
            forward_config: true,
            version: "latest".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_all() {
        let settings = Gemini::default();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Gemini = toml::from_str("").unwrap();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_disabled() {
        let settings: Gemini = toml::from_str("enabled = false\nforward_config = false").unwrap();
        assert!(!settings.enabled);
        assert!(!settings.forward_config);
    }

    #[test]
    fn deserialize_pinned_version() {
        let settings: Gemini = toml::from_str("version = \"0.1.2\"").unwrap();
        assert_eq!(settings.version, "0.1.2");
    }

    #[test]
    fn deserialize_only_enabled() {
        let settings: Gemini = toml::from_str("enabled = true").unwrap();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }
}
