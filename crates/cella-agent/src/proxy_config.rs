//! Agent-side proxy configuration.
//!
//! Deserializes the proxy config passed via `--proxy-config` JSON argument
//! and builds the rule matcher for request evaluation.

use std::io::Write;
use std::sync::Mutex;

use cella_network::config::{NetworkConfig, NetworkMode, NetworkRule, RuleAction};
use cella_network::rules::RuleMatcher;

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

    /// Log file for blocked requests.
    log_file: Mutex<Option<std::fs::File>>,
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
        let log_file = open_log_file();

        Ok(Self {
            listen_port: raw.listen_port,
            matcher,
            upstream_proxy: raw.upstream_proxy,
            ca_cert_pem: raw.ca_cert_pem,
            ca_key_pem: raw.ca_key_pem,
            log_file: Mutex::new(log_file),
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
}

fn open_log_file() -> Option<std::fs::File> {
    let log_dir = "/tmp/.cella";
    let _ = std::fs::create_dir_all(log_dir);
    let path = format!("{log_dir}/proxy.log");
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
}

/// A rule in the JSON config.
#[derive(serde::Deserialize)]
struct RuleJson {
    domain: String,
    #[serde(default)]
    paths: Vec<String>,
    action: String,
}
