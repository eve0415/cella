//! Network configuration types for proxy and blocking rules.

use serde::Deserialize;

/// Top-level network configuration.
///
/// Loaded from `[network]` in `cella.toml` or `customizations.cella.network`
/// in `devcontainer.json`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetworkConfig {
    /// Blocking mode: `denylist` (block matching, allow rest) or
    /// `allowlist` (allow matching, block rest).
    #[serde(default)]
    pub mode: NetworkMode,

    /// Proxy configuration.
    #[serde(default)]
    pub proxy: ProxyConfig,

    /// Network blocking rules.
    #[serde(default)]
    pub rules: Vec<NetworkRule>,
}

impl NetworkConfig {
    /// Returns `true` if any blocking rules are configured.
    pub const fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }
}

/// Blocking mode determines the default disposition of traffic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Everything is allowed unless a rule explicitly blocks it.
    #[default]
    Denylist,
    /// Everything is blocked unless a rule explicitly allows it.
    Allowlist,
}

/// Proxy configuration for upstream proxy servers.
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    /// Whether proxy forwarding is enabled. Defaults to `true`.
    /// When `true`, host proxy env vars are auto-detected and forwarded.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// HTTP proxy URL (overrides `HTTP_PROXY` env var).
    #[serde(default)]
    pub http: Option<String>,

    /// HTTPS proxy URL (overrides `HTTPS_PROXY` env var).
    #[serde(default)]
    pub https: Option<String>,

    /// Comma-separated list of hosts that bypass the proxy
    /// (overrides `NO_PROXY` env var).
    #[serde(default)]
    pub no_proxy: Option<String>,

    /// Path to an additional CA certificate file to inject into containers.
    #[serde(default)]
    pub ca_cert: Option<String>,

    /// Port for the cella-agent forward proxy inside containers.
    /// Only used when blocking rules are active.
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            http: None,
            https: None,
            no_proxy: None,
            ca_cert: None,
            proxy_port: default_proxy_port(),
        }
    }
}

/// A network blocking rule matching a domain and optional paths.
#[derive(Debug, Clone, Deserialize)]
pub struct NetworkRule {
    /// Domain glob pattern (e.g., `*.example.com`, `exact.domain.com`).
    pub domain: String,

    /// Optional path glob patterns (e.g., `["/api/*", "/internal/**"]`).
    /// If empty, the rule applies to all paths on the domain.
    #[serde(default)]
    pub paths: Vec<String>,

    /// Whether this rule blocks or allows traffic.
    pub action: RuleAction,
}

/// Action to take when a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Block,
    Allow,
}

const fn default_true() -> bool {
    true
}

const fn default_proxy_port() -> u16 {
    18080
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_full_config() {
        let toml_str = r#"
mode = "denylist"

[proxy]
enabled = true
http = "http://proxy.corp:3128"
https = "http://proxy.corp:3128"
no_proxy = "localhost,.internal"
proxy_port = 19090

[[rules]]
domain = "*.production.example.com"
action = "block"

[[rules]]
domain = "api.example.com"
paths = ["/v1/admin/*", "/internal/*"]
action = "block"

[[rules]]
domain = "registry.npmjs.org"
action = "allow"
"#;
        let config: NetworkConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mode, NetworkMode::Denylist);
        assert!(config.proxy.enabled);
        assert_eq!(config.proxy.http.as_deref(), Some("http://proxy.corp:3128"));
        assert_eq!(config.proxy.proxy_port, 19090);
        assert_eq!(config.rules.len(), 3);
        assert_eq!(config.rules[0].domain, "*.production.example.com");
        assert_eq!(config.rules[0].action, RuleAction::Block);
        assert!(config.rules[0].paths.is_empty());
        assert_eq!(config.rules[1].paths.len(), 2);
        assert_eq!(config.rules[2].action, RuleAction::Allow);
    }

    #[test]
    fn deserialize_minimal_config() {
        let toml_str = r#"
[[rules]]
domain = "*.prod.internal"
action = "block"
"#;
        let config: NetworkConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mode, NetworkMode::Denylist);
        assert!(config.proxy.enabled);
        assert_eq!(config.proxy.proxy_port, 18080);
        assert_eq!(config.rules.len(), 1);
    }

    #[test]
    fn deserialize_allowlist_mode() {
        let toml_str = r#"
mode = "allowlist"

[[rules]]
domain = "registry.npmjs.org"
action = "allow"

[[rules]]
domain = "github.com"
action = "allow"
"#;
        let config: NetworkConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.mode, NetworkMode::Allowlist);
        assert_eq!(config.rules.len(), 2);
    }

    #[test]
    fn default_config() {
        let config = NetworkConfig::default();
        assert_eq!(config.mode, NetworkMode::Denylist);
        assert!(config.proxy.enabled);
        assert!(!config.has_rules());
    }

    #[test]
    fn proxy_disabled() {
        let toml_str = r"
[proxy]
enabled = false
";
        let config: NetworkConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.proxy.enabled);
    }
}
