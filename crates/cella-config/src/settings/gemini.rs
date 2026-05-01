use serde::Deserialize;

const fn default_true() -> bool {
    true
}

fn default_latest() -> String {
    "latest".to_string()
}

/// Google Gemini CLI tool settings.
///
/// Controls config forwarding and version for the Gemini CLI inside dev containers.
/// Installation is triggered via `cella install` or `[tools] install = ["gemini"]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gemini {
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
            forward_config: true,
            version: "latest".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values() {
        let settings = Gemini::default();
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Gemini = toml::from_str("").unwrap();
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_forward_config_disabled() {
        let settings: Gemini = toml::from_str("forward_config = false").unwrap();
        assert!(!settings.forward_config);
    }

    #[test]
    fn deserialize_pinned_version() {
        let settings: Gemini = toml::from_str("version = \"0.1.2\"").unwrap();
        assert_eq!(settings.version, "0.1.2");
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<Gemini>("enabled = true").is_err());
    }
}
