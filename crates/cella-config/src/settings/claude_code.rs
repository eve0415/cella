use serde::Deserialize;

const fn default_true() -> bool {
    true
}

fn default_latest() -> String {
    "latest".to_string()
}

/// Claude Code tool settings.
///
/// Controls config forwarding and version for Claude Code inside dev containers.
/// Installation is triggered via `cella install` or `[tools] install = ["claude-code"]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCode {
    /// Forward `~/.claude` and `~/.claude.json` from host (default: true).
    #[serde(default = "default_true")]
    pub forward_config: bool,

    /// Version to install: `"latest"`, `"stable"`, or pinned e.g. `"1.0.58"`.
    #[serde(default = "default_latest")]
    pub version: String,
}

impl Default for ClaudeCode {
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
        let settings = ClaudeCode::default();
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: ClaudeCode = toml::from_str("").unwrap();
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_forward_config_disabled() {
        let settings: ClaudeCode = toml::from_str("forward_config = false").unwrap();
        assert!(!settings.forward_config);
    }

    #[test]
    fn deserialize_pinned_version() {
        let settings: ClaudeCode = toml::from_str("version = \"1.0.58\"").unwrap();
        assert_eq!(settings.version, "1.0.58");
    }

    #[test]
    fn deserialize_stable_version() {
        let settings: ClaudeCode = toml::from_str("version = \"stable\"").unwrap();
        assert_eq!(settings.version, "stable");
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<ClaudeCode>("enabled = true").is_err());
    }
}
