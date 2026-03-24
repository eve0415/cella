use serde::Deserialize;

const fn default_true() -> bool {
    true
}

fn default_latest() -> String {
    "latest".to_string()
}

/// `OpenAI` Codex CLI tool settings.
///
/// Controls automatic installation and config forwarding of the Codex CLI
/// into dev containers.
#[derive(Debug, Clone, Deserialize)]
pub struct Codex {
    /// Install Codex in the container (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Bind-mount `~/.codex` from host into the container (default: true).
    #[serde(default = "default_true")]
    pub forward_config: bool,

    /// Version to install: `"latest"` or pinned e.g. `"0.1.2"`.
    #[serde(default = "default_latest")]
    pub version: String,
}

impl Default for Codex {
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
        let settings = Codex::default();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Codex = toml::from_str("").unwrap();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_disabled() {
        let settings: Codex = toml::from_str("enabled = false\nforward_config = false").unwrap();
        assert!(!settings.enabled);
        assert!(!settings.forward_config);
    }

    #[test]
    fn deserialize_pinned_version() {
        let settings: Codex = toml::from_str("version = \"0.1.2\"").unwrap();
        assert_eq!(settings.version, "0.1.2");
    }

    #[test]
    fn deserialize_only_enabled() {
        let settings: Codex = toml::from_str("enabled = true").unwrap();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }
}
