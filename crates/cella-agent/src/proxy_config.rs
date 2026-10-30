//! Agent-side proxy configuration.
//!
//! Deserializes the proxy config passed via `--proxy-config` JSON argument
//! and builds the rule matcher for request evaluation.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use cella_network::config::{NetworkConfig, NetworkMode, NetworkRule, ResponseSpec, RuleAction};
use cella_network::rules::RuleMatcher;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tracing::warn;

use crate::respond::PreparedRules;

/// A credential-protected domain route.
#[derive(Debug, Clone)]
pub struct CredentialRoute {
    pub domain: String,
    pub provider_id: String,
}

/// Runtime configuration for the forward proxy.
pub struct AgentProxyConfig {
    /// Port to listen on.
    pub listen_port: u16,

    /// Rule matcher for evaluating requests.
    pub matcher: RuleMatcher,

    /// The matcher's configured responses and response headers, parsed once.
    pub prepared_rules: PreparedRules,

    /// How long a `respond` rule reads the request body before answering;
    /// past it the body is left unread and the connection closes.
    pub respond_drain_timeout: Duration,

    /// Upstream proxy URL (if chaining through a corporate proxy).
    pub upstream_proxy: Option<String>,

    /// PEM-encoded CA certificate for MITM TLS interception.
    pub ca_cert_pem: Option<String>,

    /// PEM-encoded CA private key for MITM TLS interception.
    pub ca_key_pem: Option<String>,

    /// Parsed CA certificate DER (for upstream TLS trust).
    pub ca_cert_der: Option<CertificateDer<'static>>,

    /// Log file for blocked requests.
    log_file: Mutex<Option<std::fs::File>>,

    /// Domains already warned about missing MITM (prevents log spam).
    warned_no_mitm: Mutex<HashSet<String>>,

    /// Credential-protected domain routes for daemon-side injection.
    pub credential_routes: Vec<CredentialRoute>,

    /// Daemon address for credential proxy connections.
    pub daemon_addr: Option<String>,

    /// Daemon auth token for credential proxy connections.
    pub daemon_token: Option<String>,

    /// Container name (for credential proxy handshake).
    pub container_name: Option<String>,

    /// Per-container nonce for credential tunnel authentication.
    pub container_nonce: Option<String>,

    /// Lazy-initialized multiplexed credential tunnel client.
    pub credential_mux:
        tokio::sync::Mutex<Option<std::sync::Arc<crate::credential_mux::CredentialMuxClient>>>,
}

impl AgentProxyConfig {
    /// Create proxy config from the serialized JSON passed by cella-cli.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let raw: ProxyConfigJson =
            serde_json::from_str(json).map_err(|e| format!("invalid proxy config: {e}"))?;

        let net_config = NetworkConfig {
            mode: match raw.mode.as_str() {
                "allowlist" => NetworkMode::Allowlist,
                _ => NetworkMode::Denylist,
            },
            rules: raw.rules.into_iter().map(RuleJson::into_rule).collect(),
            ..Default::default()
        };

        let (matcher, payloads) = RuleMatcher::with_payloads(net_config);
        let prepared_rules = PreparedRules::new(payloads);

        // Open log file.
        let log_file = open_log_file(raw.log_path.as_deref());

        let ca_cert_der = raw
            .ca_cert_pem
            .as_deref()
            .and_then(|pem| CertificateDer::pem_slice_iter(pem.as_bytes()).find_map(Result::ok));

        let credential_routes = raw
            .credential_routes
            .into_iter()
            .map(|r| CredentialRoute {
                domain: r.domain,
                provider_id: r.provider_id,
            })
            .collect();

        Ok(Self {
            listen_port: raw.listen_port,
            matcher,
            prepared_rules,
            respond_drain_timeout: Duration::from_secs(10),
            upstream_proxy: raw.upstream_proxy,
            ca_cert_pem: raw.ca_cert_pem,
            ca_key_pem: raw.ca_key_pem,
            ca_cert_der,
            log_file: Mutex::new(log_file),
            warned_no_mitm: Mutex::new(HashSet::new()),
            credential_routes,
            daemon_addr: raw.daemon_addr,
            daemon_token: raw.daemon_token,
            container_name: raw.container_name,
            container_nonce: raw.container_nonce,
            credential_mux: tokio::sync::Mutex::new(None),
        })
    }

    /// Find a credential route for a given domain.
    pub fn credential_route_for_domain(&self, domain: &str) -> Option<&CredentialRoute> {
        self.credential_routes.iter().find(|r| r.domain == domain)
    }

    /// Log an allowed request to the proxy log file.
    pub fn log_allowed(&self, domain: &str, path: &str) {
        self.write_log_line(format_args!("ALLOWED\t{domain}\t{path}"));
    }

    /// Log a blocked request to the proxy log file.
    pub fn log_blocked(&self, domain: &str, path: &str, reason: &str) {
        self.write_log_line(format_args!("BLOCKED\t{domain}\t{path}\t{reason}"));
    }

    /// Log a request answered by a `respond` rule to the proxy log file.
    pub fn log_responded(&self, domain: &str, path: &str, status: u16, rule: &str) {
        self.write_log_line(format_args!(
            "RESPONDED\t{domain}\t{path}\t{status}\t{rule}"
        ));
    }

    /// Log an error (e.g. TLS handshake failure) to the proxy log file.
    pub fn log_error(&self, domain: &str, error: &str) {
        self.write_log_line(format_args!("ERROR\t{domain}\t{error}"));
    }

    /// Append `{timestamp}\t{entry}` to the proxy log file, if one is open.
    fn write_log_line(&self, entry: std::fmt::Arguments<'_>) {
        let Ok(mut guard) = self.log_file.lock() else {
            return;
        };
        let Some(ref mut file) = *guard else {
            return;
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = writeln!(file, "{timestamp}\t{entry}");
    }

    /// Returns `true` the first time a domain is seen (caller should log the warning).
    pub fn warn_no_mitm_once(&self, domain: &str) -> bool {
        let Ok(mut set) = self.warned_no_mitm.lock() else {
            return false;
        };
        set.insert(domain.to_string())
    }
}

fn open_log_file(log_path: Option<&str>) -> Option<std::fs::File> {
    let path = log_path.map_or_else(|| PathBuf::from("/tmp/.cella/proxy.log"), PathBuf::from);
    if let Some(parent) = path.parent()
        && parent != Path::new("")
    {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// JSON structure passed via `--proxy-config`.
#[derive(serde::Deserialize)]
struct ProxyConfigJson {
    listen_port: u16,
    mode: String,
    rules: Vec<RuleJson>,
    upstream_proxy: Option<String>,
    ca_cert_pem: Option<String>,
    ca_key_pem: Option<String>,
    log_path: Option<String>,
    #[serde(default)]
    credential_routes: Vec<CredentialRouteJson>,
    #[serde(default)]
    daemon_addr: Option<String>,
    #[serde(default)]
    daemon_token: Option<String>,
    #[serde(default)]
    container_name: Option<String>,
    #[serde(default)]
    container_nonce: Option<String>,
}

/// A rule in the JSON config.
#[derive(serde::Deserialize)]
struct RuleJson {
    domain: String,
    #[serde(default)]
    paths: Vec<String>,
    action: String,
    #[serde(default)]
    responses: Vec<ResponseSpec>,
    #[serde(default)]
    response_headers: BTreeMap<String, String>,
}

impl RuleJson {
    /// Convert to a rule. An action this agent does not know blocks, so a
    /// newer host config fails closed.
    fn into_rule(self) -> NetworkRule {
        let action = match self.action.as_str() {
            "allow" => RuleAction::Allow,
            "block" => RuleAction::Block,
            "respond" => RuleAction::Respond,
            other => {
                warn!(
                    "Unknown network rule action {other:?} for {}; treating it as block",
                    self.domain
                );
                RuleAction::Block
            }
        };
        NetworkRule {
            domain: self.domain,
            paths: self.paths,
            action,
            responses: self.responses,
            response_headers: self.response_headers,
        }
    }
}

/// A credential route in the JSON config.
#[derive(serde::Deserialize)]
struct CredentialRouteJson {
    domain: String,
    provider_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use cella_network::rules::VerdictAction;

    fn make_json(mode: &str, rules: &str, extras: &str) -> String {
        format!(r#"{{"listen_port":8080,"mode":"{mode}","rules":[{rules}]{extras}}}"#)
    }

    #[test]
    fn from_json_denylist_mode() {
        let json = make_json("denylist", r#"{"domain":"evil.com","action":"block"}"#, "");
        let config = AgentProxyConfig::from_json(&json).unwrap();
        assert_eq!(config.listen_port, 8080);
        assert!(config.upstream_proxy.is_none());
        assert!(config.ca_cert_pem.is_none());
        assert!(config.ca_key_pem.is_none());

        // evil.com should be blocked in denylist mode.
        let v = config.matcher.evaluate("evil.com", "/");
        assert!(!v.is_allowed());

        // Other domains should be allowed in denylist mode.
        let v = config.matcher.evaluate("good.com", "/");
        assert!(v.is_allowed());
    }

    #[test]
    fn from_json_allowlist_mode() {
        let json = make_json("allowlist", r#"{"domain":"good.com","action":"allow"}"#, "");
        let config = AgentProxyConfig::from_json(&json).unwrap();

        let v = config.matcher.evaluate("good.com", "/");
        assert!(v.is_allowed());

        // Non-allowed domains should be blocked in allowlist mode.
        let v = config.matcher.evaluate("other.com", "/");
        assert!(!v.is_allowed());
    }

    #[test]
    fn from_json_with_upstream_proxy() {
        let json = make_json("denylist", "", r#","upstream_proxy":"http://proxy:3128""#);
        let config = AgentProxyConfig::from_json(&json).unwrap();
        assert_eq!(config.upstream_proxy.as_deref(), Some("http://proxy:3128"));
    }

    #[test]
    fn from_json_with_ca_materials() {
        let json = make_json(
            "denylist",
            "",
            r#","ca_cert_pem":"CERT_PEM","ca_key_pem":"KEY_PEM""#,
        );
        let config = AgentProxyConfig::from_json(&json).unwrap();
        assert_eq!(config.ca_cert_pem.as_deref(), Some("CERT_PEM"));
        assert_eq!(config.ca_key_pem.as_deref(), Some("KEY_PEM"));
        // Not actually PEM, so nothing is decoded into the DER slot.
        assert!(config.ca_cert_der.is_none());
    }

    #[test]
    fn from_json_decodes_ca_cert_der() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = cella_network::ca::ca_certificate_params()
            .self_signed(&key)
            .unwrap();

        let json = serde_json::json!({
            "listen_port": 8080,
            "mode": "denylist",
            "rules": [],
            "ca_cert_pem": cert.pem(),
        })
        .to_string();

        let config = AgentProxyConfig::from_json(&json).unwrap();
        assert_eq!(config.ca_cert_der.as_deref(), Some(cert.der().as_ref()));
    }

    #[test]
    fn from_json_skips_non_certificate_pem_sections() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = cella_network::ca::ca_certificate_params()
            .self_signed(&key)
            .unwrap();
        // The private key block comes first; the certificate must still be found.
        let bundle = format!("{}{}", key.serialize_pem(), cert.pem());

        let json = serde_json::json!({
            "listen_port": 8080,
            "mode": "denylist",
            "rules": [],
            "ca_cert_pem": bundle,
        })
        .to_string();

        let config = AgentProxyConfig::from_json(&json).unwrap();
        assert_eq!(config.ca_cert_der.as_deref(), Some(cert.der().as_ref()));
    }

    #[test]
    fn from_json_recovers_from_a_malformed_certificate_block() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = cella_network::ca::ca_certificate_params()
            .self_signed(&key)
            .unwrap();
        // A certificate section that fails to decode must be skipped, not fused on.
        let bundle = format!(
            "-----BEGIN CERTIFICATE-----\n!!!not base64!!!\n-----END CERTIFICATE-----\n{}",
            cert.pem()
        );

        let json = serde_json::json!({
            "listen_port": 8080,
            "mode": "denylist",
            "rules": [],
            "ca_cert_pem": bundle,
        })
        .to_string();

        let config = AgentProxyConfig::from_json(&json).unwrap();
        assert_eq!(config.ca_cert_der.as_deref(), Some(cert.der().as_ref()));
    }

    #[test]
    fn from_json_invalid_json() {
        let result = AgentProxyConfig::from_json("not json");
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.contains("invalid proxy config"));
    }

    #[test]
    fn from_json_empty_rules() {
        let json = make_json("denylist", "", "");
        let config = AgentProxyConfig::from_json(&json).unwrap();
        // With denylist and no rules, everything is allowed.
        let v = config.matcher.evaluate("anything.com", "/");
        assert!(v.is_allowed());
    }

    #[test]
    fn from_json_multiple_rules() {
        let rules = r#"{"domain":"evil.com","action":"block"},{"domain":"bad.org","paths":["/secret"],"action":"block"}"#;
        let json = make_json("denylist", rules, "");
        let config = AgentProxyConfig::from_json(&json).unwrap();

        let v = config.matcher.evaluate("evil.com", "/");
        assert!(!v.is_allowed());
        let v = config.matcher.evaluate("bad.org", "/secret");
        assert!(!v.is_allowed());
        let v = config.matcher.evaluate("bad.org", "/public");
        assert!(v.is_allowed());
    }

    #[test]
    fn from_json_unknown_mode_defaults_to_denylist() {
        let json = make_json(
            "unknown_mode",
            r#"{"domain":"evil.com","action":"block"}"#,
            "",
        );
        let config = AgentProxyConfig::from_json(&json).unwrap();
        // Should behave as denylist.
        let v = config.matcher.evaluate("evil.com", "/");
        assert!(!v.is_allowed());
        let v = config.matcher.evaluate("good.com", "/");
        assert!(v.is_allowed());
    }

    #[test]
    fn log_blocked_does_not_panic() {
        let json = make_json("denylist", "", "");
        let config = AgentProxyConfig::from_json(&json).unwrap();
        // Should not panic even if log file is available or not.
        config.log_blocked("evil.com", "/", "test reason");
    }

    #[test]
    fn from_json_with_custom_log_path_writes_blocked_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("proxy.log");
        let json = make_json(
            "denylist",
            "",
            &format!(r#","log_path":"{}""#, path.display()),
        );
        let config = AgentProxyConfig::from_json(&json).unwrap();

        config.log_blocked("blocked.example", "/secret", "matched deny rule");

        let log = std::fs::read_to_string(path).unwrap();
        assert!(log.contains("\tBLOCKED\tblocked.example\t/secret\tmatched deny rule"));
    }

    #[test]
    fn log_blocked_appends_to_existing_custom_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        std::fs::write(&path, "existing\n").unwrap();
        let json = make_json(
            "denylist",
            "",
            &format!(r#","log_path":"{}""#, path.display()),
        );
        let config = AgentProxyConfig::from_json(&json).unwrap();

        config.log_blocked("one.example", "/", "first");
        config.log_blocked("two.example", "/two", "second");

        let log = std::fs::read_to_string(path).unwrap();
        assert!(log.starts_with("existing\n"));
        assert!(log.contains("\tBLOCKED\tone.example\t/\tfirst"));
        assert!(log.contains("\tBLOCKED\ttwo.example\t/two\tsecond"));
    }

    #[test]
    fn log_error_writes_to_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let json = make_json(
            "denylist",
            "",
            &format!(r#","log_path":"{}""#, path.display()),
        );
        let config = AgentProxyConfig::from_json(&json).unwrap();

        config.log_error("example.com", "TLS handshake failed: test error");

        let log = std::fs::read_to_string(path).unwrap();
        assert!(log.contains("\tERROR\texample.com\tTLS handshake failed: test error"));
    }

    #[test]
    fn log_allowed_writes_to_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let json = make_json(
            "denylist",
            "",
            &format!(r#","log_path":"{}""#, path.display()),
        );
        let config = AgentProxyConfig::from_json(&json).unwrap();

        config.log_allowed("example.com", "/api/v1");

        let log = std::fs::read_to_string(path).unwrap();
        assert!(log.contains("\tALLOWED\texample.com\t/api/v1"));
    }

    #[test]
    fn from_json_unknown_action_blocks() {
        let json = make_json(
            "denylist",
            r#"{"domain":"new.com","action":"redirect"}"#,
            "",
        );
        let config = AgentProxyConfig::from_json(&json).unwrap();
        let v = config.matcher.evaluate("new.com", "/");
        assert!(matches!(v.action, VerdictAction::Block));
    }

    #[test]
    fn from_json_respond_rule_serves_its_responses() {
        let rules = r#"{"domain":"api.example.com","action":"respond","responses":[{"status":529,"headers":{"retry-after":"0"},"body":"busy"},{"status":200}]}"#;
        let json = make_json("denylist", rules, "");
        let config = AgentProxyConfig::from_json(&json).unwrap();
        let v = config.matcher.evaluate("api.example.com", "/v1/messages");
        assert!(
            matches!(v.action, VerdictAction::Respond(_)),
            "expected a respond verdict, got {:?}",
            v.action
        );

        let rule: RuleJson = serde_json::from_str(rules).unwrap();
        let [first, second] = rule.into_rule().responses.try_into().unwrap();
        assert_eq!(first.status, 529);
        assert_eq!(first.headers["retry-after"], "0");
        assert_eq!(first.body, "busy");
        assert_eq!(second.status, 200);
        assert!(second.headers.is_empty());
        assert!(second.body.is_empty());
    }

    #[test]
    fn from_json_allow_rule_carries_response_headers() {
        let rules = r#"{"domain":"api.example.com","action":"allow","response_headers":{"x-test-run":"1"}}"#;
        let json = make_json("allowlist", rules, "");
        let config = AgentProxyConfig::from_json(&json).unwrap();
        let v = config.matcher.evaluate("api.example.com", "/");
        assert!(
            matches!(
                v.action,
                VerdictAction::Allow {
                    response_headers: Some(_)
                }
            ),
            "expected allow with headers, got {:?}",
            v.action
        );

        let rule: RuleJson = serde_json::from_str(rules).unwrap();
        assert_eq!(
            rule.into_rule().response_headers,
            BTreeMap::from([("x-test-run".to_string(), "1".to_string())])
        );
    }

    #[test]
    fn log_responded_writes_tab_separated_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let json = make_json(
            "denylist",
            "",
            &format!(r#","log_path":"{}""#, path.display()),
        );
        let config = AgentProxyConfig::from_json(&json).unwrap();

        config.log_responded("api.example.com", "/v1/messages", 529, "rule display");

        let log = std::fs::read_to_string(path).unwrap();
        assert!(log.contains("\tRESPONDED\tapi.example.com\t/v1/messages\t529\trule display\n"));
    }

    #[test]
    fn warn_no_mitm_once_deduplicates() {
        let json = make_json("denylist", "", "");
        let config = AgentProxyConfig::from_json(&json).unwrap();
        assert!(config.warn_no_mitm_once("example.com"));
        assert!(!config.warn_no_mitm_once("example.com"));
        assert!(config.warn_no_mitm_once("other.com"));
    }
}
