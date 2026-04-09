use std::collections::BTreeMap;

use serde::Deserialize;

const fn default_true() -> bool {
    true
}

/// AI provider API key forwarding settings.
///
/// Controls which AI provider API keys are automatically forwarded
/// from the host environment into dev containers during `exec`/`shell`.
///
/// Per-provider toggles are stored as a flattened map so that adding
/// new providers does not require struct changes. Unknown keys default
/// to enabled.
///
/// Config section: `[credentials.ai]`
#[derive(Debug, Clone, Deserialize)]
pub struct AiCredentials {
    /// Global toggle for AI key forwarding (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Per-provider overrides. Key is the provider id (e.g. `"openai"`),
    /// value is whether forwarding is enabled. Missing providers default
    /// to `true`.
    #[serde(flatten)]
    pub providers: BTreeMap<String, bool>,
}

impl AiCredentials {
    /// Check whether a specific provider is enabled.
    ///
    /// Returns `false` if the global toggle is off.
    /// Returns the per-provider override if set, otherwise `true`.
    pub fn is_provider_enabled(&self, id: &str) -> bool {
        if !self.enabled {
            return false;
        }
        self.providers.get(id).copied().unwrap_or(true)
    }
}

impl Default for AiCredentials {
    fn default() -> Self {
        Self {
            enabled: true,
            providers: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_all() {
        let creds = AiCredentials::default();
        assert!(creds.enabled);
        assert!(creds.is_provider_enabled("anthropic"));
        assert!(creds.is_provider_enabled("openai"));
        assert!(creds.is_provider_enabled("gemini"));
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let creds: AiCredentials = toml::from_str("").unwrap();
        assert!(creds.enabled);
        assert!(creds.is_provider_enabled("anthropic"));
        assert!(creds.is_provider_enabled("openai"));
    }

    #[test]
    fn deserialize_global_disable() {
        let creds: AiCredentials = toml::from_str("enabled = false").unwrap();
        assert!(!creds.enabled);
        // Global toggle overrides even unlisted providers
        assert!(!creds.is_provider_enabled("anthropic"));
    }

    #[test]
    fn deserialize_individual_disable() {
        let creds: AiCredentials = toml::from_str("openai = false\ngroq = false").unwrap();
        assert!(creds.enabled);
        assert!(!creds.is_provider_enabled("openai"));
        assert!(!creds.is_provider_enabled("groq"));
        assert!(creds.is_provider_enabled("anthropic"));
        assert!(creds.is_provider_enabled("gemini"));
    }
}
