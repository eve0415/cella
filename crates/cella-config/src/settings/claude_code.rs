use serde::Deserialize;

const fn default_true() -> bool {
    true
}

fn default_latest() -> String {
    "latest".to_string()
}

/// Claude Code tool settings.
///
/// Controls automatic installation and config forwarding of Claude Code
/// into dev containers.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeCodeSettings {
    /// Install Claude Code in the container (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Forward `~/.claude` and `~/.claude.json` from host (default: true).
    #[serde(default = "default_true")]
    pub forward_config: bool,

    /// Version to install: `"latest"`, `"stable"`, or pinned e.g. `"1.0.58"`.
    #[serde(default = "default_latest")]
    pub version: String,

    /// Glob patterns for additional files/dirs to exclude from the default copy set.
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Glob patterns for additional files/dirs to include beyond the default copy set.
    #[serde(default)]
    pub include: Vec<String>,
}

impl Default for ClaudeCodeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            forward_config: true,
            version: "latest".to_string(),
            exclude: Vec::new(),
            include: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_all() {
        let settings = ClaudeCodeSettings::default();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
        assert!(settings.exclude.is_empty());
        assert!(settings.include.is_empty());
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: ClaudeCodeSettings = toml::from_str("").unwrap();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_disabled() {
        let settings: ClaudeCodeSettings =
            toml::from_str("enabled = false\nforward_config = false").unwrap();
        assert!(!settings.enabled);
        assert!(!settings.forward_config);
    }

    #[test]
    fn deserialize_pinned_version() {
        let settings: ClaudeCodeSettings = toml::from_str("version = \"1.0.58\"").unwrap();
        assert_eq!(settings.version, "1.0.58");
    }

    #[test]
    fn deserialize_stable_version() {
        let settings: ClaudeCodeSettings = toml::from_str("version = \"stable\"").unwrap();
        assert_eq!(settings.version, "stable");
    }

    #[test]
    fn deserialize_glob_patterns() {
        let settings: ClaudeCodeSettings = toml::from_str(
            r#"
exclude = ["plans/**", "sessions/**"]
include = ["backups/important/**"]
"#,
        )
        .unwrap();
        assert_eq!(settings.exclude, vec!["plans/**", "sessions/**"]);
        assert_eq!(settings.include, vec!["backups/important/**"]);
    }

    #[test]
    fn deserialize_only_enabled() {
        let settings: ClaudeCodeSettings = toml::from_str("enabled = true").unwrap();
        assert!(settings.enabled);
        assert!(settings.forward_config);
        assert_eq!(settings.version, "latest");
    }
}
