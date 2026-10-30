//! Network configuration types for proxy and blocking rules.

use std::collections::BTreeMap;

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

    /// Returns `true` if the agent needs TLS interception (and so the MITM
    /// CA) for this config: some rule inspects paths, synthesizes responses,
    /// or injects response headers, or credential protection is on.
    pub fn needs_mitm(&self, credentials_protect: bool) -> bool {
        credentials_protect || self.rules.iter().any(NetworkRule::needs_mitm)
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

/// A network rule matching a domain and optional paths.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetworkRule {
    /// Domain glob pattern (e.g., `*.example.com`, `exact.domain.com`).
    pub domain: String,

    /// Optional path glob patterns (e.g., `["/api/*", "/internal/**"]`).
    /// If empty, the rule applies to all paths on the domain.
    #[serde(default)]
    pub paths: Vec<String>,

    /// Whether this rule blocks, allows, or answers matching traffic.
    pub action: RuleAction,

    /// Responses served in order by a `respond` rule; the last one repeats.
    #[serde(default)]
    pub responses: Vec<ResponseSpec>,

    /// Headers set on upstream responses forwarded by an `allow` rule.
    #[serde(default)]
    pub response_headers: BTreeMap<String, String>,
}

impl NetworkRule {
    /// Human-readable rule identity for diagnostics: the domain, followed by
    /// its path patterns when it has any.
    pub fn label(&self) -> String {
        rule_label(&self.domain, &self.paths)
    }

    /// Whether applying this rule needs the request decrypted: it inspects
    /// paths, answers with a configured response, or sets response headers.
    pub fn needs_mitm(&self) -> bool {
        !self.paths.is_empty()
            || self.action == RuleAction::Respond
            || !self.response_headers.is_empty()
    }
}

/// Format a rule's identity as `domain` or `domain [path, path]`.
///
/// Rules are identified by what the user wrote rather than by index, because
/// indices are post-merge and would not match the user's file.
pub fn rule_label(domain: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        domain.to_string()
    } else {
        format!("{domain} [{}]", paths.join(", "))
    }
}

/// Action to take when a rule matches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    #[default]
    Block,
    Allow,
    /// Answer with a configured response without contacting upstream.
    Respond,
}

impl RuleAction {
    /// The lowercase name used in config files, the agent protocol and CLI
    /// output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Allow => "allow",
            Self::Respond => "respond",
        }
    }
}

/// A response synthesized by a `respond` rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResponseSpec {
    /// HTTP status code (`200..=599`).
    pub status: u16,

    /// Response headers, sent as written.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    /// Response body; empty when absent.
    #[serde(default)]
    pub body: String,
}

/// Whether HTTP forbids a body for `status` (204, 205 and 304).
#[must_use]
pub const fn status_forbids_body(status: u16) -> bool {
    matches!(status, 204 | 205 | 304)
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
    fn status_forbids_body_for_bodyless_statuses() {
        for status in [204, 205, 304] {
            assert!(status_forbids_body(status), "{status}");
        }
        for status in [200, 404] {
            assert!(!status_forbids_body(status), "{status}");
        }
    }

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
    fn deserialize_respond_rule() {
        let toml_str = r#"
[[rules]]
domain = "api.anthropic.com"
paths = ["/v1/messages"]
action = "respond"

[[rules.responses]]
status = 529
headers = { content-type = "application/json", retry-after = "0" }
body = '{"type":"error"}'

[[rules.responses]]
status = 200

[[rules]]
domain = "api.example.com"
action = "allow"
response_headers = { x-test-run = "1" }
"#;
        let config: NetworkConfig = toml::from_str(toml_str).unwrap();
        let respond = &config.rules[0];
        assert_eq!(respond.action, RuleAction::Respond);
        assert_eq!(respond.responses.len(), 2);
        assert_eq!(respond.responses[0].status, 529);
        assert_eq!(respond.responses[0].headers["retry-after"], "0");
        assert_eq!(respond.responses[0].body, r#"{"type":"error"}"#);
        assert_eq!(respond.responses[1].status, 200);
        assert!(respond.responses[1].headers.is_empty());
        assert!(respond.responses[1].body.is_empty());
        assert_eq!(config.rules[1].response_headers["x-test-run"], "1");
        assert!(config.rules[1].responses.is_empty());
        config.validate().unwrap();
    }

    #[test]
    fn rule_action_as_str_names_every_variant() {
        assert_eq!(RuleAction::Block.as_str(), "block");
        assert_eq!(RuleAction::Allow.as_str(), "allow");
        assert_eq!(RuleAction::Respond.as_str(), "respond");
    }

    fn domain_rule(action: RuleAction) -> NetworkRule {
        NetworkRule {
            domain: "api.example.com".to_string(),
            action,
            ..Default::default()
        }
    }

    fn with_rules(rules: Vec<NetworkRule>) -> NetworkConfig {
        NetworkConfig {
            rules,
            ..Default::default()
        }
    }

    #[test]
    fn needs_mitm_for_domain_only_respond_rule() {
        let config = with_rules(vec![domain_rule(RuleAction::Respond)]);
        assert!(config.needs_mitm(false));
    }

    #[test]
    fn needs_mitm_for_allow_rule_with_only_response_headers() {
        let mut rule = domain_rule(RuleAction::Allow);
        rule.response_headers
            .insert("x-test-run".to_string(), "1".to_string());
        assert!(with_rules(vec![rule]).needs_mitm(false));
    }

    #[test]
    fn needs_mitm_for_path_rule_or_credential_protection() {
        let mut rule = domain_rule(RuleAction::Block);
        rule.paths.push("/admin/*".to_string());
        assert!(with_rules(vec![rule]).needs_mitm(false));
        assert!(with_rules(vec![]).needs_mitm(true));
    }

    #[test]
    fn no_mitm_for_domain_only_block_or_allow_rules() {
        let config = with_rules(vec![
            domain_rule(RuleAction::Block),
            domain_rule(RuleAction::Allow),
        ]);
        assert!(!config.needs_mitm(false));
    }

    #[test]
    fn rule_label_names_domain_and_paths() {
        let mut rule = domain_rule(RuleAction::Block);
        assert_eq!(rule.label(), "api.example.com");
        rule.paths = vec!["/a".to_string(), "/b/*".to_string()];
        assert_eq!(rule.label(), "api.example.com [/a, /b/*]");
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

/// Whether network blocking rules are enforced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkRulePolicy {
    Enforce,
    Skip,
}
