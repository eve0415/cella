//! Network and proxy configuration settings.
//!
//! Wraps `cella_network::NetworkConfig` for TOML deserialization
//! within the cella settings system.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use cella_network::NetworkRulePolicy;
use serde::Deserialize;

use crate::cella_config::CellaConfigError;

/// Largest `body_file` inlined into the proxy config.
const MAX_BODY_FILE_BYTES: u64 = 1024 * 1024;

/// Network settings section of `cella.toml`.
///
/// Maps to `[network]` in the TOML config.
/// Thin wrapper around `cella_network::NetworkConfig` to keep
/// the settings crate's deserialization self-contained.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Blocking mode: "denylist" or "allowlist".
    /// `None` when the user didn't set `[network].mode` in their TOML config,
    /// so it doesn't override devcontainer.json's mode during merge.
    #[serde(default)]
    pub mode: Option<NetworkMode>,

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
#[serde(deny_unknown_fields)]
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

    /// Path to additional CA certificate. A leading `~` expands to the home
    /// directory.
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

/// A network rule.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkRule {
    /// Domain glob pattern.
    pub domain: String,

    /// Optional path glob patterns.
    #[serde(default)]
    pub paths: Vec<String>,

    /// Block, allow, or respond action.
    pub action: RuleAction,

    /// Responses served in order by a `respond` rule.
    #[serde(default)]
    pub responses: Vec<ResponseSpec>,

    /// Headers added to upstream responses by an `allow` rule.
    #[serde(default)]
    pub response_headers: BTreeMap<String, String>,
}

/// Rule action.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    #[default]
    Block,
    Allow,
    Respond,
}

/// A configured `respond` response, before `body_file` is read.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseSpec {
    /// HTTP status code.
    pub status: u16,

    /// Response headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    /// Inline response body.
    #[serde(default)]
    pub body: Option<String>,

    /// File whose content becomes the response body. Made absolute against
    /// the declaring config file's directory at load time.
    #[serde(default)]
    pub body_file: Option<PathBuf>,
}

const fn default_true() -> bool {
    true
}

const fn default_proxy_port() -> u16 {
    18080
}

impl Network {
    /// The mode explicitly set by the user, if any.
    pub fn mode_override(&self) -> Option<cella_network::NetworkMode> {
        self.mode.map(|m| match m {
            NetworkMode::Denylist => cella_network::NetworkMode::Denylist,
            NetworkMode::Allowlist => cella_network::NetworkMode::Allowlist,
        })
    }

    /// Convert to `cella_network::NetworkConfig` for use by the rule engine.
    ///
    /// When `mode` is `None` (not explicitly set), defaults to `Denylist`.
    /// Under [`NetworkRulePolicy::Skip`] the rules are dropped before any
    /// `body_file` is read, so `--no-network-rules` works with a broken
    /// fixture; otherwise each `body_file` is inlined and the result is
    /// validated.
    ///
    /// # Errors
    ///
    /// Returns `CellaConfigError` if a `body_file` cannot be used or a rule
    /// fails [`cella_network::NetworkConfig::validate`].
    pub fn to_network_config(
        &self,
        policy: NetworkRulePolicy,
    ) -> Result<cella_network::NetworkConfig, CellaConfigError> {
        let (mode, rules) = match policy {
            NetworkRulePolicy::Skip => (cella_network::NetworkMode::Denylist, Vec::new()),
            NetworkRulePolicy::Enforce => (
                self.mode_override()
                    .unwrap_or(cella_network::NetworkMode::Denylist),
                self.rules
                    .iter()
                    .map(convert_rule)
                    .collect::<Result<_, _>>()?,
            ),
        };
        let config = cella_network::NetworkConfig {
            mode,
            proxy: cella_network::ProxyConfig {
                enabled: self.proxy.enabled,
                http: self.proxy.http.clone(),
                https: self.proxy.https.clone(),
                no_proxy: self.proxy.no_proxy.clone(),
                ca_cert: self.proxy.ca_cert.as_deref().map(|p| {
                    cella_env::paths::expand_tilde(p)
                        .to_string_lossy()
                        .into_owned()
                }),
                proxy_port: self.proxy.proxy_port,
            },
            rules,
        };
        config.validate()?;
        Ok(config)
    }
}

fn convert_rule(rule: &NetworkRule) -> Result<cella_network::NetworkRule, CellaConfigError> {
    let responses = rule
        .responses
        .iter()
        .enumerate()
        .map(|(index, response)| resolve_response(rule, index, response))
        .collect::<Result<_, _>>()?;
    Ok(cella_network::NetworkRule {
        domain: rule.domain.clone(),
        paths: rule.paths.clone(),
        action: match rule.action {
            RuleAction::Block => cella_network::RuleAction::Block,
            RuleAction::Allow => cella_network::RuleAction::Allow,
            RuleAction::Respond => cella_network::RuleAction::Respond,
        },
        responses,
        response_headers: rule.response_headers.clone(),
    })
}

/// Resolve one response's body, enforcing the checks that need `body_file`.
fn resolve_response(
    rule: &NetworkRule,
    index: usize,
    response: &ResponseSpec,
) -> Result<cella_network::ResponseSpec, CellaConfigError> {
    let rule_label = || cella_network::config::rule_label(&rule.domain, &rule.paths);
    let field = || format!("responses[{index}]");
    let body = match (&response.body, &response.body_file) {
        (Some(_), Some(_)) => {
            return Err(CellaConfigError::ResponseBodyConflict {
                rule: rule_label(),
                field: field(),
            });
        }
        (_, Some(path)) if cella_network::status_forbids_body(response.status) => {
            return Err(CellaConfigError::ResponseBodyFileNotAllowed {
                rule: rule_label(),
                field: field(),
                status: response.status,
                path: path.clone(),
            });
        }
        (_, Some(path)) => {
            read_body_file(path).map_err(|problem| CellaConfigError::ResponseBodyFile {
                rule: rule_label(),
                field: field(),
                path: path.clone(),
                problem,
            })?
        }
        (body, None) => body.clone().unwrap_or_default(),
    };
    Ok(cella_network::ResponseSpec {
        status: response.status,
        headers: response.headers.clone(),
        body,
    })
}

/// Read a `body_file`, rejecting files over 1 MiB or not valid UTF-8.
///
/// Reads at most one byte past the limit, so an oversized (or endless) file is
/// never loaded whole.
fn read_body_file(path: &Path) -> Result<String, BodyFileProblem> {
    let file = std::fs::File::open(path).map_err(BodyFileProblem::Read)?;
    let mut buf = Vec::new();
    file.take(MAX_BODY_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(BodyFileProblem::Read)?;
    if u64::try_from(buf.len()).map_or(true, |len| len > MAX_BODY_FILE_BYTES) {
        return Err(BodyFileProblem::TooLarge);
    }
    String::from_utf8(buf).map_err(|_| BodyFileProblem::NotUtf8)
}

/// Why a `body_file` could not be inlined.
#[derive(Debug, thiserror::Error)]
pub enum BodyFileProblem {
    #[error("cannot read it: {0}")]
    Read(#[source] std::io::Error),
    #[error("it is larger than 1 MiB")]
    TooLarge,
    #[error("it is not valid UTF-8")]
    NotUtf8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_network_settings() {
        let network = Network::default();
        assert!(network.mode.is_none());
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
        assert_eq!(network.mode, Some(NetworkMode::Denylist));
        assert_eq!(network.proxy.http.as_deref(), Some("http://proxy:3128"));
        assert_eq!(network.proxy.proxy_port, 19090);
        assert_eq!(network.rules.len(), 2);
        assert_eq!(network.rules[0].domain, "*.prod.internal");
        assert_eq!(network.rules[0].action, RuleAction::Block);
    }

    #[test]
    fn to_network_config_expands_tilde_in_ca_cert() {
        let Some(home) = cella_env::paths::home_dir() else {
            return;
        };
        let network = Network {
            proxy: ProxySettings {
                ca_cert: Some("~/certs/ca.pem".to_string()),
                ..ProxySettings::default()
            },
            ..Network::default()
        };
        let config = network
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap();
        assert_eq!(
            config.proxy.ca_cert.as_deref(),
            Some(home.join("certs/ca.pem").to_string_lossy().as_ref())
        );
    }

    #[test]
    fn to_network_config_keeps_absolute_ca_cert() {
        let network = Network {
            proxy: ProxySettings {
                ca_cert: Some("/etc/ssl/ca.pem".to_string()),
                ..ProxySettings::default()
            },
            ..Network::default()
        };
        let config = network
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap();
        assert_eq!(config.proxy.ca_cert.as_deref(), Some("/etc/ssl/ca.pem"));
    }

    #[test]
    fn convert_to_network_config() {
        let network = Network {
            mode: Some(NetworkMode::Allowlist),
            proxy: ProxySettings {
                enabled: true,
                http: Some("http://proxy:3128".to_string()),
                ..Default::default()
            },
            rules: vec![NetworkRule {
                domain: "*.example.com".to_string(),
                paths: vec!["/api/*".to_string()],
                action: RuleAction::Allow,
                ..Default::default()
            }],
        };

        let config = network
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap();
        assert_eq!(config.mode, cella_network::NetworkMode::Allowlist);
        assert_eq!(config.proxy.http.as_deref(), Some("http://proxy:3128"));
        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].action, cella_network::RuleAction::Allow);
    }

    fn respond_network(response: ResponseSpec) -> Network {
        Network {
            rules: vec![NetworkRule {
                domain: "api.example.com".to_string(),
                paths: vec!["/v1/messages".to_string()],
                action: RuleAction::Respond,
                responses: vec![response],
                ..Default::default()
            }],
            ..Network::default()
        }
    }

    fn file_response(status: u16, path: &Path) -> ResponseSpec {
        ResponseSpec {
            status,
            body_file: Some(path.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn deserialize_respond_rule_and_response_headers() {
        let toml_str = r#"
[[rules]]
domain = "api.example.com"
action = "respond"

[[rules.responses]]
status = 529
headers = { content-type = "application/json" }
body = "{}"

[[rules.responses]]
status = 200
body_file = "fixtures/message.json"

[[rules]]
domain = "api.example.com"
action = "allow"
response_headers = { x-test-run = "1" }
"#;
        let network: Network = toml::from_str(toml_str).unwrap();
        assert_eq!(network.rules[0].action, RuleAction::Respond);
        assert_eq!(network.rules[0].responses[0].body.as_deref(), Some("{}"));
        assert_eq!(
            network.rules[0].responses[1].body_file.as_deref(),
            Some(Path::new("fixtures/message.json"))
        );
        assert_eq!(network.rules[1].response_headers["x-test-run"], "1");
    }

    #[test]
    fn unknown_response_field_is_rejected() {
        let toml_str = r#"
[[rules]]
domain = "api.example.com"
action = "respond"

[[rules.responses]]
status = 200
bodyfile = "x"
"#;
        assert!(toml::from_str::<Network>(toml_str).is_err());
    }

    #[test]
    fn body_file_content_is_inlined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("message.json");
        std::fs::write(&path, r#"{"ok":true}"#).unwrap();
        let config = respond_network(file_response(200, &path))
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap();
        assert_eq!(config.rules[0].action, cella_network::RuleAction::Respond);
        assert_eq!(config.rules[0].responses[0].body, r#"{"ok":true}"#);
    }

    #[test]
    fn oversized_body_file_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("big.json");
        let limit = usize::try_from(MAX_BODY_FILE_BYTES).unwrap();
        std::fs::write(&path, vec![b'a'; limit]).unwrap();
        respond_network(file_response(200, &path))
            .to_network_config(NetworkRulePolicy::Enforce)
            .expect("a file of exactly 1 MiB is accepted");
        std::fs::write(&path, vec![b'a'; limit + 1]).unwrap();
        let err = respond_network(file_response(200, &path))
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap_err();
        assert!(
            matches!(
                err,
                CellaConfigError::ResponseBodyFile {
                    problem: BodyFileProblem::TooLarge,
                    ..
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn non_utf8_body_file_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("bin.dat");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let err = respond_network(file_response(200, &path))
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap_err();
        assert!(
            matches!(
                err,
                CellaConfigError::ResponseBodyFile {
                    problem: BodyFileProblem::NotUtf8,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn missing_body_file_names_path_as_written() {
        let path = Path::new("${localWorkspaceFolder}/fixtures/missing.json");
        let err = respond_network(file_response(200, path))
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap_err();
        assert!(
            matches!(
                err,
                CellaConfigError::ResponseBodyFile {
                    problem: BodyFileProblem::Read(_),
                    ..
                }
            ),
            "{err}"
        );
        let message = err.to_string();
        assert!(message.contains("${localWorkspaceFolder}/fixtures/missing.json"));
        assert!(message.contains("api.example.com [/v1/messages]"));
        assert!(message.contains("responses[0]"));
    }

    #[test]
    fn body_and_body_file_together_are_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("message.json");
        std::fs::write(&path, "{}").unwrap();
        let mut response = file_response(200, &path);
        response.body = Some("{}".to_string());
        let err = respond_network(response)
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap_err();
        assert!(
            matches!(err, CellaConfigError::ResponseBodyConflict { .. }),
            "{err}"
        );
    }

    #[test]
    fn empty_body_file_on_no_content_status_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("empty");
        std::fs::write(&path, "").unwrap();
        for status in [204, 205, 304] {
            let err = respond_network(file_response(status, &path))
                .to_network_config(NetworkRulePolicy::Enforce)
                .unwrap_err();
            assert!(
                matches!(err, CellaConfigError::ResponseBodyFileNotAllowed { .. }),
                "status {status}: {err}"
            );
        }
    }

    #[test]
    fn invalid_rule_fails_network_validation() {
        let network = Network {
            rules: vec![NetworkRule {
                domain: "api.example.com".to_string(),
                action: RuleAction::Respond,
                ..Default::default()
            }],
            ..Network::default()
        };
        let err = network
            .to_network_config(NetworkRulePolicy::Enforce)
            .unwrap_err();
        assert!(
            matches!(
                err,
                CellaConfigError::Network(
                    cella_network::NetworkConfigError::MissingResponses { .. }
                )
            ),
            "{err}"
        );
    }

    #[test]
    fn skip_policy_ignores_broken_body_file() {
        let mut network = respond_network(file_response(
            200,
            Path::new("/nonexistent/cella-test/missing.json"),
        ));
        network.mode = Some(NetworkMode::Allowlist);
        let config = network.to_network_config(NetworkRulePolicy::Skip).unwrap();
        assert!(config.rules.is_empty());
        assert_eq!(config.mode, cella_network::NetworkMode::Denylist);
    }

    /// `--no-network-rules` is applied only here, so the proxy forwarding
    /// built from the result must carry no rules and fail open.
    #[test]
    fn skip_policy_leaves_proxy_forwarding_without_rules() {
        let network = Network {
            mode: Some(NetworkMode::Allowlist),
            rules: vec![NetworkRule {
                domain: "allowed.example.com".to_string(),
                action: RuleAction::Allow,
                ..Default::default()
            }],
            ..Network::default()
        };

        let without_protect = cella_env::ProxyForwardingConfig::resolve(
            network.to_network_config(NetworkRulePolicy::Skip).unwrap(),
            false,
            true,
        );
        assert!(
            !without_protect.has_blocking_rules,
            "skipped rules must not route traffic through the proxy"
        );

        let with_protect = cella_env::ProxyForwardingConfig::resolve(
            network.to_network_config(NetworkRulePolicy::Skip).unwrap(),
            true,
            true,
        );
        assert!(
            with_protect.has_blocking_rules,
            "credential protection still needs the proxy"
        );
        let agent_config = with_protect
            .full_config
            .expect("credential protection still requires an agent proxy config");
        assert!(agent_config.rules.is_empty());
        assert_eq!(
            agent_config.mode,
            cella_network::NetworkMode::Denylist,
            "denylist with no rules permits unmatched traffic"
        );
    }
}
