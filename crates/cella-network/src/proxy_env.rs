//! Host proxy environment variable auto-detection and env var set building.
//!
//! Detects `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` (and lowercase variants)
//! from the host environment and builds the env var set for containers.

use crate::config::ProxyConfig;

/// Proxy environment variables resolved for container injection.
#[derive(Debug, Clone, Default)]
pub struct ProxyEnvVars {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

impl ProxyEnvVars {
    /// Detect proxy env vars from the current process environment,
    /// then apply overrides from the proxy config.
    ///
    /// Returns `None` if proxy is disabled in config.
    pub fn detect(config: &ProxyConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }

        let http_proxy = config
            .http
            .clone()
            .or_else(|| std::env::var("HTTP_PROXY").ok())
            .or_else(|| std::env::var("http_proxy").ok());

        let https_proxy = config
            .https
            .clone()
            .or_else(|| std::env::var("HTTPS_PROXY").ok())
            .or_else(|| std::env::var("https_proxy").ok());

        let no_proxy = config
            .no_proxy
            .clone()
            .or_else(|| std::env::var("NO_PROXY").ok())
            .or_else(|| std::env::var("no_proxy").ok());

        // If nothing detected and nothing configured, return empty.
        if http_proxy.is_none() && https_proxy.is_none() {
            return Some(Self::default());
        }

        Some(Self {
            http_proxy,
            https_proxy,
            no_proxy,
        })
    }

    /// Returns `true` if any proxy URL is set.
    pub const fn has_proxy(&self) -> bool {
        self.http_proxy.is_some() || self.https_proxy.is_some()
    }

    /// Build the env var key-value pairs to inject into a container.
    ///
    /// Sets both uppercase and lowercase variants for maximum compatibility.
    /// Appends safety entries to `NO_PROXY` to prevent proxy loops.
    pub fn to_env_pairs(&self, cella_proxy_port: Option<u16>) -> Vec<(String, String)> {
        let mut pairs = Vec::new();

        if let Some(ref url) = self.http_proxy {
            pairs.push(("HTTP_PROXY".to_string(), url.clone()));
            pairs.push(("http_proxy".to_string(), url.clone()));
        }

        if let Some(ref url) = self.https_proxy {
            pairs.push(("HTTPS_PROXY".to_string(), url.clone()));
            pairs.push(("https_proxy".to_string(), url.clone()));
        }

        // Build NO_PROXY with safety entries to prevent proxy loops.
        let no_proxy = self.build_no_proxy(cella_proxy_port);
        if !no_proxy.is_empty() {
            pairs.push(("NO_PROXY".to_string(), no_proxy.clone()));
            pairs.push(("no_proxy".to_string(), no_proxy));
        }

        pairs
    }

    /// Build the `NO_PROXY` value, appending localhost entries to prevent loops.
    fn build_no_proxy(&self, cella_proxy_port: Option<u16>) -> String {
        let mut entries: Vec<String> = Vec::new();

        if let Some(ref existing) = self.no_proxy {
            for entry in existing.split(',') {
                let trimmed = entry.trim();
                if !trimmed.is_empty() {
                    entries.push(trimmed.to_string());
                }
            }
        }

        // Always add localhost entries to prevent proxy loops.
        let safety = ["localhost", "127.0.0.1", "::1"];
        for s in &safety {
            let s_str = (*s).to_string();
            if !entries.iter().any(|e| e.eq_ignore_ascii_case(&s_str)) {
                entries.push(s_str);
            }
        }

        // If cella-agent proxy is active, add its address too.
        if let Some(port) = cella_proxy_port {
            let addr = format!("127.0.0.1:{port}");
            if !entries.contains(&addr) {
                entries.push(addr);
            }
        }

        entries.join(",")
    }

    /// Build args to inject into Docker builds for proxy support.
    ///
    /// Docker automatically recognizes `HTTP_PROXY`, `HTTPS_PROXY`, and
    /// `NO_PROXY` as build args without requiring explicit `ARG` declarations.
    pub fn to_build_args(&self) -> Vec<(String, String)> {
        let mut args = Vec::new();
        if let Some(ref url) = self.http_proxy {
            args.push(("HTTP_PROXY".to_string(), url.clone()));
            args.push(("http_proxy".to_string(), url.clone()));
        }
        if let Some(ref url) = self.https_proxy {
            args.push(("HTTPS_PROXY".to_string(), url.clone()));
            args.push(("https_proxy".to_string(), url.clone()));
        }
        if let Some(ref np) = self.no_proxy {
            args.push(("NO_PROXY".to_string(), np.clone()));
            args.push(("no_proxy".to_string(), np.clone()));
        }
        args
    }

    /// Build the env var pairs for the cella-agent proxy.
    ///
    /// When blocking rules are active, proxy env vars point to the local
    /// cella-agent proxy instead of the upstream proxy.
    pub fn to_agent_proxy_env_pairs(&self, proxy_port: u16) -> Vec<(String, String)> {
        let proxy_url = format!("http://127.0.0.1:{proxy_port}");
        let no_proxy = self.build_no_proxy(Some(proxy_port));

        let mut pairs = vec![
            ("HTTP_PROXY".to_string(), proxy_url.clone()),
            ("http_proxy".to_string(), proxy_url.clone()),
            ("HTTPS_PROXY".to_string(), proxy_url.clone()),
            ("https_proxy".to_string(), proxy_url),
        ];

        if !no_proxy.is_empty() {
            pairs.push(("NO_PROXY".to_string(), no_proxy.clone()));
            pairs.push(("no_proxy".to_string(), no_proxy));
        }

        pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_none_when_disabled() {
        let config = ProxyConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(ProxyEnvVars::detect(&config).is_none());
    }

    #[test]
    fn detect_with_explicit_config() {
        let config = ProxyConfig {
            http: Some("http://proxy:3128".to_string()),
            https: Some("http://proxy:3128".to_string()),
            no_proxy: Some("localhost,.internal".to_string()),
            ..Default::default()
        };
        let vars = ProxyEnvVars::detect(&config).unwrap();
        assert_eq!(vars.http_proxy.as_deref(), Some("http://proxy:3128"));
        assert_eq!(vars.https_proxy.as_deref(), Some("http://proxy:3128"));
        assert!(vars.has_proxy());
    }

    #[test]
    fn to_env_pairs_sets_both_cases() {
        let vars = ProxyEnvVars {
            http_proxy: Some("http://proxy:3128".to_string()),
            https_proxy: Some("http://proxy:3128".to_string()),
            no_proxy: None,
        };
        let pairs = vars.to_env_pairs(None);
        assert!(pairs.iter().any(|(k, _)| k == "HTTP_PROXY"));
        assert!(pairs.iter().any(|(k, _)| k == "http_proxy"));
        assert!(pairs.iter().any(|(k, _)| k == "HTTPS_PROXY"));
        assert!(pairs.iter().any(|(k, _)| k == "https_proxy"));
        // NO_PROXY should have safety entries
        assert!(pairs.iter().any(|(k, _)| k == "NO_PROXY"));
    }

    #[test]
    fn no_proxy_includes_safety_entries() {
        let vars = ProxyEnvVars {
            http_proxy: Some("http://proxy:3128".to_string()),
            no_proxy: Some(".internal".to_string()),
            ..Default::default()
        };
        let no_proxy = vars.build_no_proxy(None);
        assert!(no_proxy.contains(".internal"));
        assert!(no_proxy.contains("localhost"));
        assert!(no_proxy.contains("127.0.0.1"));
        assert!(no_proxy.contains("::1"));
    }

    #[test]
    fn no_proxy_deduplicates_safety_entries() {
        let vars = ProxyEnvVars {
            http_proxy: Some("http://proxy:3128".to_string()),
            no_proxy: Some("localhost,127.0.0.1,.corp".to_string()),
            ..Default::default()
        };
        let no_proxy = vars.build_no_proxy(None);
        let count = no_proxy.matches("localhost").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn agent_proxy_env_pairs() {
        let vars = ProxyEnvVars {
            no_proxy: Some(".internal".to_string()),
            ..Default::default()
        };
        let pairs = vars.to_agent_proxy_env_pairs(18080);
        let http = pairs.iter().find(|(k, _)| k == "HTTP_PROXY").unwrap();
        assert_eq!(http.1, "http://127.0.0.1:18080");
        let no_proxy = pairs.iter().find(|(k, _)| k == "NO_PROXY").unwrap();
        assert!(no_proxy.1.contains("127.0.0.1:18080"));
    }
}
