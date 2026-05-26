//! Built-in credential provider registry for phantom token protection.
//!
//! Maps known API providers to their domains, auth headers, and host
//! environment variables. Custom providers from `[[credentials.providers]]`
//! in cella.toml are merged at runtime.

/// A credential provider that maps a host-side secret to an HTTP header
/// injection on one or more domains.
#[derive(Debug, Clone)]
pub struct CredentialProvider {
    /// Short identifier (e.g., `"anthropic"`, `"github"`).
    pub id: &'static str,
    /// Host environment variable holding the real credential.
    pub env_var: &'static str,
    /// API domains this provider protects.
    pub domains: &'static [&'static str],
    /// HTTP header name for injection.
    pub header: &'static str,
    /// Header value prefix (e.g., `"Bearer "`).
    pub prefix: &'static str,
}

/// All built-in credential providers (GitHub + 11 AI providers).
pub const CREDENTIAL_PROVIDERS: &[CredentialProvider] = &[
    CredentialProvider {
        id: "github",
        env_var: "GH_TOKEN",
        domains: &["github.com", "api.github.com"],
        header: "Authorization",
        prefix: "token ",
    },
    CredentialProvider {
        id: "anthropic",
        env_var: "ANTHROPIC_API_KEY",
        domains: &["api.anthropic.com"],
        header: "x-api-key",
        prefix: "",
    },
    CredentialProvider {
        id: "openai",
        env_var: "OPENAI_API_KEY",
        domains: &["api.openai.com"],
        header: "Authorization",
        prefix: "Bearer ",
    },
    CredentialProvider {
        id: "gemini",
        env_var: "GEMINI_API_KEY",
        domains: &["generativelanguage.googleapis.com"],
        header: "x-goog-api-key",
        prefix: "",
    },
    CredentialProvider {
        id: "groq",
        env_var: "GROQ_API_KEY",
        domains: &["api.groq.com"],
        header: "Authorization",
        prefix: "Bearer ",
    },
    CredentialProvider {
        id: "mistral",
        env_var: "MISTRAL_API_KEY",
        domains: &["api.mistral.ai"],
        header: "Authorization",
        prefix: "Bearer ",
    },
    CredentialProvider {
        id: "deepseek",
        env_var: "DEEPSEEK_API_KEY",
        domains: &["api.deepseek.com"],
        header: "Authorization",
        prefix: "Bearer ",
    },
    CredentialProvider {
        id: "xai",
        env_var: "XAI_API_KEY",
        domains: &["api.x.ai"],
        header: "Authorization",
        prefix: "Bearer ",
    },
    CredentialProvider {
        id: "fireworks",
        env_var: "FIREWORKS_API_KEY",
        domains: &["api.fireworks.ai"],
        header: "Authorization",
        prefix: "Bearer ",
    },
    CredentialProvider {
        id: "together",
        env_var: "TOGETHER_API_KEY",
        domains: &["api.together.xyz"],
        header: "Authorization",
        prefix: "Bearer ",
    },
    CredentialProvider {
        id: "perplexity",
        env_var: "PERPLEXITY_API_KEY",
        domains: &["api.perplexity.ai"],
        header: "Authorization",
        prefix: "Bearer ",
    },
    CredentialProvider {
        id: "cohere",
        env_var: "COHERE_API_KEY",
        domains: &["api.cohere.com"],
        header: "Authorization",
        prefix: "Bearer ",
    },
];

/// A merged credential provider entry (either built-in or custom).
#[derive(Debug, Clone)]
pub struct MergedProvider {
    pub id: String,
    pub env_var: String,
    pub domains: Vec<String>,
    pub header: String,
    pub prefix: String,
}

impl From<&CredentialProvider> for MergedProvider {
    fn from(p: &CredentialProvider) -> Self {
        Self {
            id: p.id.to_string(),
            env_var: p.env_var.to_string(),
            domains: p.domains.iter().map(|&d| d.to_string()).collect(),
            header: p.header.to_string(),
            prefix: p.prefix.to_string(),
        }
    }
}

/// Input for a custom credential provider (avoids circular dep on cella-config).
pub struct CustomProviderInput<'a> {
    pub name: &'a str,
    pub env: &'a str,
    pub domains: &'a [String],
    pub header: &'a str,
    pub prefix: &'a str,
}

/// Merge built-in providers with custom ones from config.
///
/// Custom providers with the same `id` as a built-in override the built-in.
pub fn merge_with_custom(custom: &[CustomProviderInput<'_>]) -> Vec<MergedProvider> {
    let custom_ids: std::collections::HashSet<&str> = custom.iter().map(|c| c.name).collect();

    let mut result: Vec<MergedProvider> = CREDENTIAL_PROVIDERS
        .iter()
        .filter(|p| !custom_ids.contains(p.id))
        .map(MergedProvider::from)
        .collect();

    for c in custom {
        result.push(MergedProvider {
            id: c.name.to_string(),
            env_var: c.env.to_string(),
            domains: c.domains.to_vec(),
            header: c.header.to_string(),
            prefix: c.prefix.to_string(),
        });
    }

    result
}

/// Find the provider that handles a given domain.
pub fn provider_for_domain<'a>(
    providers: &'a [MergedProvider],
    domain: &str,
) -> Option<&'a MergedProvider> {
    providers
        .iter()
        .find(|p| p.domains.iter().any(|d| d == domain))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_count() {
        assert_eq!(CREDENTIAL_PROVIDERS.len(), 12);
    }

    #[test]
    fn unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for p in CREDENTIAL_PROVIDERS {
            assert!(seen.insert(p.id), "duplicate id: {}", p.id);
        }
    }

    #[test]
    fn unique_domains() {
        let mut seen = std::collections::HashSet::new();
        for p in CREDENTIAL_PROVIDERS {
            for d in p.domains {
                assert!(seen.insert(d), "duplicate domain: {d}");
            }
        }
    }

    #[test]
    fn unique_env_vars() {
        let mut seen = std::collections::HashSet::new();
        for p in CREDENTIAL_PROVIDERS {
            assert!(seen.insert(p.env_var), "duplicate env var: {}", p.env_var);
        }
    }

    #[test]
    fn github_provider_config() {
        let gh = CREDENTIAL_PROVIDERS
            .iter()
            .find(|p| p.id == "github")
            .unwrap();
        assert_eq!(gh.domains, &["github.com", "api.github.com"]);
        assert_eq!(gh.header, "Authorization");
        assert_eq!(gh.prefix, "token ");
    }

    #[test]
    fn anthropic_provider_config() {
        let p = CREDENTIAL_PROVIDERS
            .iter()
            .find(|p| p.id == "anthropic")
            .unwrap();
        assert_eq!(p.domains, &["api.anthropic.com"]);
        assert_eq!(p.header, "x-api-key");
        assert_eq!(p.prefix, "");
    }

    #[test]
    fn merge_no_custom_returns_all_builtin() {
        let merged = merge_with_custom(&[]);
        assert_eq!(merged.len(), CREDENTIAL_PROVIDERS.len());
    }

    #[test]
    fn merge_custom_adds_provider() {
        let domains = vec!["api.internal.corp".to_string()];
        let custom = vec![CustomProviderInput {
            name: "internal",
            env: "INTERNAL_KEY",
            domains: &domains,
            header: "x-api-key",
            prefix: "",
        }];
        let merged = merge_with_custom(&custom);
        assert_eq!(merged.len(), CREDENTIAL_PROVIDERS.len() + 1);
        assert!(merged.iter().any(|p| p.id == "internal"));
    }

    #[test]
    fn merge_custom_overrides_builtin() {
        let domains = vec!["custom-anthropic.corp".to_string()];
        let custom = vec![CustomProviderInput {
            name: "anthropic",
            env: "MY_ANTHROPIC_KEY",
            domains: &domains,
            header: "Authorization",
            prefix: "Bearer ",
        }];
        let merged = merge_with_custom(&custom);
        assert_eq!(merged.len(), CREDENTIAL_PROVIDERS.len());
        let anthropic = merged.iter().find(|p| p.id == "anthropic").unwrap();
        assert_eq!(anthropic.domains, vec!["custom-anthropic.corp"]);
        assert_eq!(anthropic.env_var, "MY_ANTHROPIC_KEY");
    }

    #[test]
    fn provider_for_domain_finds_match() {
        let merged = merge_with_custom(&[]);
        let found = provider_for_domain(&merged, "api.anthropic.com");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "anthropic");
    }

    #[test]
    fn provider_for_domain_returns_none_for_unknown() {
        let merged = merge_with_custom(&[]);
        assert!(provider_for_domain(&merged, "unknown.example.com").is_none());
    }
}
