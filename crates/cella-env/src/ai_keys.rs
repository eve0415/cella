//! AI provider API key detection and forwarding.
//!
//! Reads known AI provider API keys from the host environment and
//! produces env var entries for container injection. Keys are read
//! live from the host process on every `exec`/`shell` invocation —
//! never stored in labels or baked at creation time.

/// A known AI provider and its primary API key environment variable.
pub struct AiProvider {
    /// Short provider identifier (e.g., `"anthropic"`, `"openai"`).
    ///
    /// Matches the field name in `[credentials.ai]` config.
    pub id: &'static str,

    /// Environment variable name (e.g., `"ANTHROPIC_API_KEY"`).
    pub env_var: &'static str,
}

/// All known AI provider API key mappings.
pub const AI_PROVIDERS: &[AiProvider] = &[
    AiProvider {
        id: "anthropic",
        env_var: "ANTHROPIC_API_KEY",
    },
    AiProvider {
        id: "openai",
        env_var: "OPENAI_API_KEY",
    },
    AiProvider {
        id: "gemini",
        env_var: "GEMINI_API_KEY",
    },
    AiProvider {
        id: "groq",
        env_var: "GROQ_API_KEY",
    },
    AiProvider {
        id: "mistral",
        env_var: "MISTRAL_API_KEY",
    },
    AiProvider {
        id: "deepseek",
        env_var: "DEEPSEEK_API_KEY",
    },
    AiProvider {
        id: "xai",
        env_var: "XAI_API_KEY",
    },
    AiProvider {
        id: "fireworks",
        env_var: "FIREWORKS_API_KEY",
    },
    AiProvider {
        id: "together",
        env_var: "TOGETHER_API_KEY",
    },
    AiProvider {
        id: "perplexity",
        env_var: "PERPLEXITY_API_KEY",
    },
    AiProvider {
        id: "cohere",
        env_var: "COHERE_API_KEY",
    },
];

/// Detect AI API keys present in the host environment.
///
/// Returns `(env_var_name, value)` pairs for keys that:
/// 1. Are present and non-empty in the host environment
/// 2. Are enabled via the `provider_enabled` predicate
/// 3. Are NOT already set by the user (checked via `user_env_keys`)
pub fn detect_ai_keys(
    provider_enabled: &dyn Fn(&str) -> bool,
    user_env_keys: &[&str],
) -> Vec<(String, String)> {
    AI_PROVIDERS
        .iter()
        .filter(|p| provider_enabled(p.id))
        .filter(|p| !user_env_keys.contains(&p.env_var))
        .filter_map(|p| {
            std::env::var(p.env_var)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (p.env_var.to_string(), v))
        })
        .collect()
}

/// Return the names of AI API keys detected on the host (for logging).
///
/// Never returns values, only key names.
pub fn detect_ai_key_names(provider_enabled: &dyn Fn(&str) -> bool) -> Vec<&'static str> {
    AI_PROVIDERS
        .iter()
        .filter(|p| provider_enabled(p.id))
        .filter(|p| std::env::var(p.env_var).ok().is_some_and(|v| !v.is_empty()))
        .map(|p| p.env_var)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_count() {
        assert_eq!(AI_PROVIDERS.len(), 11);
    }

    #[test]
    fn providers_unique_env_vars() {
        let mut seen = std::collections::HashSet::new();
        for p in AI_PROVIDERS {
            assert!(seen.insert(p.env_var), "duplicate env var: {}", p.env_var);
        }
    }

    #[test]
    fn providers_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for p in AI_PROVIDERS {
            assert!(seen.insert(p.id), "duplicate id: {}", p.id);
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn detect_skips_disabled_provider() {
        // Even if the env var is set, disabled providers are skipped
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-key") };
        let keys = detect_ai_keys(&|id| id != "anthropic", &[]);
        assert!(!keys.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    }

    #[test]
    #[allow(unsafe_code)]
    fn detect_skips_user_override() {
        unsafe { std::env::set_var("OPENAI_API_KEY", "test-key") };
        let keys = detect_ai_keys(&|_| true, &["OPENAI_API_KEY"]);
        assert!(!keys.iter().any(|(k, _)| k == "OPENAI_API_KEY"));
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
    }

    #[test]
    #[allow(unsafe_code)]
    fn detect_skips_empty_value() {
        unsafe { std::env::set_var("GEMINI_API_KEY", "") };
        let keys = detect_ai_keys(&|_| true, &[]);
        assert!(!keys.iter().any(|(k, _)| k == "GEMINI_API_KEY"));
        unsafe { std::env::remove_var("GEMINI_API_KEY") };
    }

    #[test]
    #[allow(unsafe_code)]
    fn detect_returns_present_key() {
        unsafe { std::env::set_var("COHERE_API_KEY", "ck-test") };
        let keys = detect_ai_keys(&|_| true, &[]);
        let found = keys.iter().find(|(k, _)| k == "COHERE_API_KEY");
        assert_eq!(found.map(|(_, v)| v.as_str()), Some("ck-test"));
        unsafe { std::env::remove_var("COHERE_API_KEY") };
    }

    #[test]
    #[allow(unsafe_code)]
    fn detect_key_names_never_returns_values() {
        unsafe { std::env::set_var("XAI_API_KEY", "secret-value") };
        let names = detect_ai_key_names(&|_| true);
        assert!(names.contains(&"XAI_API_KEY"));
        // Ensure we only get the name, not the value
        for name in &names {
            assert!(!name.contains("secret"));
        }
        unsafe { std::env::remove_var("XAI_API_KEY") };
    }
}
