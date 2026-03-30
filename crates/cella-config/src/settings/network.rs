//! Network and proxy configuration settings.
//!
//! Wraps `cella_network::NetworkConfig` for TOML deserialization
//! within the cella settings system.

use serde::Deserialize;

/// Network settings section of `cella.toml`.
///
/// Maps to `[network]` in the TOML config.
/// Thin wrapper around `cella_network::NetworkConfig` to keep
/// the settings crate's deserialization self-contained.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Network {
    /// Blocking mode: "denylist" or "allowlist".
    #[serde(default)]
    pub mode: NetworkMode,

    /// Proxy configuration.
    #[serde(default)]
    pub proxy: ProxySettings,

    /// Network blocking rules.
    #[serde(default)]
    pub rules: Vec<NetworkRule>,
}

/// Blocking mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    #[default]
    Denylist,
    Allowlist,
}

/// Proxy settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ProxySettings {
    /// Whether proxy forwarding is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// HTTP proxy URL override.
    #[serde(default)]
    pub http: Option<String>,

    /// HTTPS proxy URL override.
    #[serde(default)]
    pub https: Option<String>,

    /// `NO_PROXY` override.
    #[serde(default)]
    pub no_proxy: Option<String>,

    /// Path to additional CA certificate.
    #[serde(default)]
    pub ca_cert: Option<String>,

    /// Cella-agent proxy listen port.
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
}

impl Default for ProxySettings {
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

/// A network blocking rule.
#[derive(Debug, Clone, Deserialize)]
pub struct NetworkRule {
    /// Domain glob pattern.
    pub domain: String,

    /// Optional path glob patterns.
    #[serde(default)]
    pub paths: Vec<String>,

    /// Block or allow action.
    pub action: RuleAction,
}

/// Rule action.
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

impl Network {
    /// Convert to `cella_network::NetworkConfig` for use by the rule engine.
    pub fn to_network_config(&self) -> cella_network::NetworkConfig {
        cella_network::NetworkConfig {
            mode: match self.mode {
                NetworkMode::Denylist => cella_network::NetworkMode::Denylist,
                NetworkMode::Allowlist => cella_network::NetworkMode::Allowlist,
            },
            proxy: cella_network::ProxyConfig {
                enabled: self.proxy.enabled,
                http: self.proxy.http.clone(),
                https: self.proxy.https.clone(),
                no_proxy: self.proxy.no_proxy.clone(),
                ca_cert: self.proxy.ca_cert.clone(),
                proxy_port: self.proxy.proxy_port,
            },
            rules: self
                .rules
                .iter()
                .map(|r| cella_network::NetworkRule {
                    domain: r.domain.clone(),
                    paths: r.paths.clone(),
                    action: match r.action {
                        RuleAction::Block => cella_network::RuleAction::Block,
                        RuleAction::Allow => cella_network::RuleAction::Allow,
                    },
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_network_settings() {
        let network = Network::default();
        assert_eq!(network.mode, NetworkMode::Denylist);
        assert!(network.proxy.enabled);
        assert_eq!(network.proxy.proxy_port, 18080);
        assert!(network.rules.is_empty());
    }

    #[test]
    fn deserialize_network_section() {
        let toml_str = r#"
mode = "denylist"

[proxy]
http = "http://proxy:3128"
proxy_port = 19090

[[rules]]
domain = "*.prod.internal"
action = "block"

[[rules]]
domain = "api.example.com"
paths = ["/admin/**"]
action = "block"
"#;
        let network: Network = toml::from_str(toml_str).unwrap();
        assert_eq!(network.mode, NetworkMode::Denylist);
        assert_eq!(network.proxy.http.as_deref(), Some("http://proxy:3128"));
        assert_eq!(network.proxy.proxy_port, 19090);
        assert_eq!(network.rules.len(), 2);
        assert_eq!(network.rules[0].domain, "*.prod.internal");
        assert_eq!(network.rules[0].action, RuleAction::Block);
    }

    #[test]
    fn convert_to_network_config() {
        let network = Network {
            mode: NetworkMode::Allowlist,
            proxy: ProxySettings {
                enabled: true,
                http: Some("http://proxy:3128".to_string()),
                ..Default::default()
            },
            rules: vec![NetworkRule {
                domain: "*.example.com".to_string(),
                paths: vec!["/api/*".to_string()],
                action: RuleAction::Allow,
            }],
        };

        let config = network.to_network_config();
        assert_eq!(config.mode, cella_network::NetworkMode::Allowlist);
        assert_eq!(config.proxy.http.as_deref(), Some("http://proxy:3128"));
        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].action, cella_network::RuleAction::Allow);
    }
}
