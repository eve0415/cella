//! Agent-side proxy configuration.
//!
//! Deserializes the proxy config passed via `--proxy-config` JSON argument
//! and builds the rule matcher for request evaluation.

use std::collections::HashSet;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cella_network::config::{NetworkConfig, NetworkMode, NetworkRule, RuleAction};
use cella_network::rules::RuleMatcher;
use rustls::pki_types::CertificateDer;

/// Runtime configuration for the forward proxy.
pub struct AgentProxyConfig {
    /// Port to listen on.
    pub listen_port: u16,

    /// Rule matcher for evaluating requests.
    pub matcher: RuleMatcher,

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
            rules: raw
                .rules
                .into_iter()
                .map(|r| NetworkRule {
                    domain: r.domain,
                    paths: r.paths,
                    action: if r.action == "allow" {
                        RuleAction::Allow
                    } else {
                        RuleAction::Block
                    },
                })
                .collect(),
            ..Default::default()
        };

        let matcher = RuleMatcher::new(&net_config);

        // Open log file.
        let log_file = open_log_file(raw.log_path.as_deref());

        let ca_cert_der = raw.ca_cert_pem.as_deref().and_then(|pem| {
            let mut reader = BufReader::new(pem.as_bytes());
            rustls_pemfile::certs(&mut reader).find_map(Result::ok)
        });

        Ok(Self {
            listen_port: raw.listen_port,
            matcher,
            upstream_proxy: raw.upstream_proxy,
            ca_cert_pem: raw.ca_cert_pem,
            ca_key_pem: raw.ca_key_pem,
            ca_cert_der,
            log_file: Mutex::new(log_file),
            warned_no_mitm: Mutex::new(HashSet::new()),
        })
    }

    /// Log a blocked request to the proxy log file.
    pub fn log_blocked(&self, domain: &str, path: &str, reason: &str) {
        let Ok(mut guard) = self.log_file.lock() else {
            return;
        };
        let Some(ref mut file) = *guard else {
            return;
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "{timestamp}\tBLOCKED\t{domain}\t{path}\t{reason}");
    }

    /// Log an error (e.g. TLS handshake failure) to the proxy log file.
    pub fn log_error(&self, domain: &str, error: &str) {
        let Ok(mut guard) = self.log_file.lock() else {
            return;
        };
        let Some(ref mut file) = *guard else {
            return;
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "{timestamp}\tERROR\t{domain}\t{error}");
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
}

/// A rule in the JSON config.
#[derive(serde::Deserialize)]
struct RuleJson {
    domain: String,
    #[serde(default)]
    paths: Vec<String>,
    action: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_json(mode: &str, rules: &str, extras: &str) -> String {
        format!(r#"{{"listen_port":8080,"mode":"{mode}","rules":[{rules}]{extras}}}"#,)
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
        assert!(!v.allowed);

        // Other domains should be allowed in denylist mode.
        let v = config.matcher.evaluate("good.com", "/");
        assert!(v.allowed);
    }

    #[test]
    fn from_json_allowlist_mode() {
        let json = make_json("allowlist", r#"{"domain":"good.com","action":"allow"}"#, "");
        let config = AgentProxyConfig::from_json(&json).unwrap();

        let v = config.matcher.evaluate("good.com", "/");
        assert!(v.allowed);

        // Non-allowed domains should be blocked in allowlist mode.
        let v = config.matcher.evaluate("other.com", "/");
        assert!(!v.allowed);
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
        assert!(v.allowed);
    }

    #[test]
    fn from_json_multiple_rules() {
        let rules = r#"{"domain":"evil.com","action":"block"},{"domain":"bad.org","paths":["/secret"],"action":"block"}"#;
        let json = make_json("denylist", rules, "");
        let config = AgentProxyConfig::from_json(&json).unwrap();

        let v = config.matcher.evaluate("evil.com", "/");
        assert!(!v.allowed);
        let v = config.matcher.evaluate("bad.org", "/secret");
        assert!(!v.allowed);
        let v = config.matcher.evaluate("bad.org", "/public");
        assert!(v.allowed);
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
        assert!(!v.allowed);
        let v = config.matcher.evaluate("good.com", "/");
        assert!(v.allowed);
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
    fn warn_no_mitm_once_deduplicates() {
        let json = make_json("denylist", "", "");
        let config = AgentProxyConfig::from_json(&json).unwrap();
        assert!(config.warn_no_mitm_once("example.com"));
        assert!(!config.warn_no_mitm_once("example.com"));
        assert!(config.warn_no_mitm_once("other.com"));
    }
}
