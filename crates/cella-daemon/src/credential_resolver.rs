//! Live credential resolution for daemon-side injection.
//!
//! Resolves real credentials on every request — never caches.
//! For AI providers, reads the host environment variable live.
//! For GitHub, invokes `gh auth token`.

use std::process::Command;

/// A resolved credential ready for HTTP header injection.
#[derive(Debug, Clone)]
pub struct ResolvedCredential {
    pub header_name: String,
    pub header_value: String,
}

/// Built-in provider metadata needed for resolution.
pub struct ProviderMeta {
    pub env_var: String,
    pub header: String,
    pub prefix: String,
}

/// Resolve a credential for the given provider.
///
/// Returns `None` if the credential is unavailable (env var unset,
/// gh CLI not authenticated, etc.).
pub fn resolve_credential(provider_id: &str, meta: &ProviderMeta) -> Option<ResolvedCredential> {
    let raw_value = if provider_id == "github" {
        resolve_github_token()
    } else {
        std::env::var(&meta.env_var).ok().filter(|v| !v.is_empty())
    }?;

    let header_value = format!("{}{raw_value}", meta.prefix);

    Some(ResolvedCredential {
        header_name: meta.header.clone(),
        header_value,
    })
}

fn resolve_github_token() -> Option<String> {
    Command::new("gh")
        .args(["auth", "token", "-h", "github.com"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_from_env_var() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TEST_CRED_RESOLVE_KEY", "sk-test-123");
        }

        let meta = ProviderMeta {
            env_var: "TEST_CRED_RESOLVE_KEY".to_string(),
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
        };

        let result = resolve_credential("anthropic", &meta);
        assert!(result.is_some());
        let cred = result.unwrap();
        assert_eq!(cred.header_name, "Authorization");
        assert_eq!(cred.header_value, "Bearer sk-test-123");

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_CRED_RESOLVE_KEY");
        }
    }

    #[test]
    fn resolve_missing_env_var() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_CRED_MISSING_KEY");
        }

        let meta = ProviderMeta {
            env_var: "TEST_CRED_MISSING_KEY".to_string(),
            header: "x-api-key".to_string(),
            prefix: String::new(),
        };

        assert!(resolve_credential("openai", &meta).is_none());
    }

    #[test]
    fn resolve_empty_env_var() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TEST_CRED_EMPTY_KEY", "");
        }

        let meta = ProviderMeta {
            env_var: "TEST_CRED_EMPTY_KEY".to_string(),
            header: "x-api-key".to_string(),
            prefix: String::new(),
        };

        assert!(resolve_credential("test", &meta).is_none());

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_CRED_EMPTY_KEY");
        }
    }

    #[test]
    fn resolve_no_prefix() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TEST_CRED_NOPREFIX", "raw-key");
        }

        let meta = ProviderMeta {
            env_var: "TEST_CRED_NOPREFIX".to_string(),
            header: "x-api-key".to_string(),
            prefix: String::new(),
        };

        let cred = resolve_credential("test", &meta).unwrap();
        assert_eq!(cred.header_value, "raw-key");

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_CRED_NOPREFIX");
        }
    }
}
