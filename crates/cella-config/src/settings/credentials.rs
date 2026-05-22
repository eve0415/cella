use serde::Deserialize;

use super::AiCredentials;

const fn default_true() -> bool {
    true
}

/// Credential forwarding settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// Forward gh CLI credentials into containers (default: true).
    #[serde(default = "default_true")]
    pub gh: bool,

    /// AI provider API key forwarding settings.
    #[serde(default)]
    pub ai: AiCredentials,

    /// Replace real credentials with opaque phantom tokens inside containers.
    ///
    /// When enabled, the daemon holds real credentials and injects them into
    /// outbound requests via daemon-side HTTPS proxying. Processes inside the
    /// container never see real credentials.
    #[serde(default)]
    pub protect: bool,

    /// Custom credential providers beyond the built-in set.
    #[serde(default)]
    pub providers: Vec<CustomCredentialProvider>,

    /// Active credential profile name (for per-project multi-account scoping).
    #[serde(default)]
    pub profile: Option<String>,
}

/// A user-defined credential provider configured in `[[credentials.providers]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomCredentialProvider {
    /// Short identifier for this provider.
    pub name: String,
    /// Host environment variable holding the real credential.
    pub env: String,
    /// Target domain this provider protects.
    pub domain: String,
    /// HTTP header name for injection.
    pub header: String,
    /// Header value prefix (e.g., `"Bearer "`).
    #[serde(default)]
    pub prefix: String,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            gh: true,
            ai: AiCredentials::default(),
            protect: false,
            providers: Vec::new(),
            profile: None,
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
        assert!(!settings.protect);
        assert!(settings.providers.is_empty());
        assert!(settings.profile.is_none());
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Credentials = toml::from_str("").unwrap();
        assert!(settings.gh);
        assert!(settings.ai.enabled);
        assert!(!settings.protect);
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

    #[test]
    fn deserialize_protect_enabled() {
        let settings: Credentials = toml::from_str("protect = true").unwrap();
        assert!(settings.protect);
        assert!(settings.gh);
    }

    #[test]
    fn deserialize_custom_provider() {
        let toml = r#"
[[providers]]
name = "internal-api"
env = "INTERNAL_API_KEY"
domain = "api.internal.corp"
header = "Authorization"
prefix = "Bearer "
"#;
        let settings: Credentials = toml::from_str(toml).unwrap();
        assert_eq!(settings.providers.len(), 1);
        assert_eq!(settings.providers[0].name, "internal-api");
        assert_eq!(settings.providers[0].env, "INTERNAL_API_KEY");
        assert_eq!(settings.providers[0].domain, "api.internal.corp");
        assert_eq!(settings.providers[0].header, "Authorization");
        assert_eq!(settings.providers[0].prefix, "Bearer ");
    }

    #[test]
    fn deserialize_custom_provider_default_prefix() {
        let toml = r#"
[[providers]]
name = "simple"
env = "API_KEY"
domain = "api.example.com"
header = "x-api-key"
"#;
        let settings: Credentials = toml::from_str(toml).unwrap();
        assert_eq!(settings.providers[0].prefix, "");
    }

    #[test]
    fn deserialize_multiple_providers() {
        let toml = r#"
protect = true

[[providers]]
name = "a"
env = "A_KEY"
domain = "a.example.com"
header = "x-api-key"

[[providers]]
name = "b"
env = "B_KEY"
domain = "b.example.com"
header = "Authorization"
prefix = "Token "
"#;
        let settings: Credentials = toml::from_str(toml).unwrap();
        assert!(settings.protect);
        assert_eq!(settings.providers.len(), 2);
    }

    #[test]
    fn deserialize_profile() {
        let settings: Credentials = toml::from_str(r#"profile = "work""#).unwrap();
        assert_eq!(settings.profile.as_deref(), Some("work"));
    }

    #[test]
    fn deserialize_rejects_unknown_fields() {
        let result: Result<Credentials, _> = toml::from_str("unknown_field = true");
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_rejects_unknown_provider_fields() {
        let toml = r#"
[[providers]]
name = "x"
env = "X"
domain = "x.com"
header = "h"
bogus = "nope"
"#;
        let result: Result<Credentials, _> = toml::from_str(toml);
        assert!(result.is_err());
    }
}
