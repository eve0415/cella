//! Live credential resolution for daemon-side injection.
//!
//! Resolves real credentials on every request — never caches.
//! For AI providers, reads the host environment variable live.
//! For GitHub, invokes `gh auth token`.

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
pub async fn resolve_credential(
    provider_id: &str,
    meta: &ProviderMeta,
    hostname: &str,
) -> Option<ResolvedCredential> {
    let raw_value = if provider_id == "github" {
        resolve_github_token(hostname).await
    } else {
        std::env::var(&meta.env_var).ok().filter(|v| !v.is_empty())
    }?;

    let header_value = format!("{}{raw_value}", meta.prefix);

    Some(ResolvedCredential {
        header_name: meta.header.clone(),
        header_value,
    })
}

async fn resolve_github_token(hostname: &str) -> Option<String> {
    tokio::process::Command::new("gh")
        .args(["auth", "token", "-h", hostname])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn resolve_from_env_var() {
        let _guard = ENV_LOCK.lock().await;
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TEST_CRED_RESOLVE_KEY", "sk-test-123");
        }

        let meta = ProviderMeta {
            env_var: "TEST_CRED_RESOLVE_KEY".to_string(),
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
        };

        let result = resolve_credential("anthropic", &meta, "api.anthropic.com").await;
        assert!(result.is_some());
        let cred = result.unwrap();
        assert_eq!(cred.header_name, "Authorization");
        assert_eq!(cred.header_value, "Bearer sk-test-123");

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_CRED_RESOLVE_KEY");
        }
    }

    #[tokio::test]
    async fn resolve_missing_env_var() {
        let _guard = ENV_LOCK.lock().await;
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_CRED_MISSING_KEY");
        }

        let meta = ProviderMeta {
            env_var: "TEST_CRED_MISSING_KEY".to_string(),
            header: "x-api-key".to_string(),
            prefix: String::new(),
        };

        assert!(
            resolve_credential("openai", &meta, "api.openai.com")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn resolve_empty_env_var() {
        let _guard = ENV_LOCK.lock().await;
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TEST_CRED_EMPTY_KEY", "");
        }

        let meta = ProviderMeta {
            env_var: "TEST_CRED_EMPTY_KEY".to_string(),
            header: "x-api-key".to_string(),
            prefix: String::new(),
        };

        assert!(
            resolve_credential("test", &meta, "example.com")
                .await
                .is_none()
        );

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_CRED_EMPTY_KEY");
        }
    }

    #[tokio::test]
    async fn resolve_no_prefix() {
        let _guard = ENV_LOCK.lock().await;
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TEST_CRED_NOPREFIX", "raw-key");
        }

        let meta = ProviderMeta {
            env_var: "TEST_CRED_NOPREFIX".to_string(),
            header: "x-api-key".to_string(),
            prefix: String::new(),
        };

        let cred = resolve_credential("test", &meta, "example.com")
            .await
            .unwrap();
        assert_eq!(cred.header_value, "raw-key");

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_CRED_NOPREFIX");
        }
    }
}
