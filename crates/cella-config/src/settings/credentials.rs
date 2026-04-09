use serde::Deserialize;

use super::AiCredentials;

const fn default_true() -> bool {
    true
}

/// Credential forwarding settings.
#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    /// Forward gh CLI credentials into containers (default: true).
    #[serde(default = "default_true")]
    pub gh: bool,

    /// AI provider API key forwarding settings.
    #[serde(default)]
    pub ai: AiCredentials,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            gh: true,
            ai: AiCredentials::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_gh() {
        let settings = Credentials::default();
        assert!(settings.gh);
        assert!(settings.ai.enabled);
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Credentials = toml::from_str("").unwrap();
        assert!(settings.gh);
        assert!(settings.ai.enabled);
    }

    #[test]
    fn deserialize_explicit_false() {
        let settings: Credentials = toml::from_str("gh = false").unwrap();
        assert!(!settings.gh);
    }

    #[test]
    fn deserialize_explicit_true() {
        let settings: Credentials = toml::from_str("gh = true").unwrap();
        assert!(settings.gh);
    }

    #[test]
    fn deserialize_nested_ai_section() {
        let settings: Credentials = toml::from_str("[ai]\nopenai = false\nenabled = true").unwrap();
        assert!(settings.gh);
        assert!(settings.ai.enabled);
        assert!(!settings.ai.is_provider_enabled("openai"));
        assert!(settings.ai.is_provider_enabled("anthropic"));
    }
}
