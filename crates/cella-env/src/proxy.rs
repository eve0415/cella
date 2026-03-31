//! Proxy environment variable forwarding for containers.
//!
//! Detects host proxy env vars and builds the env var set for injection
//! into containers, respecting cella network config overrides.

use cella_network::config::{NetworkConfig, ProxyConfig};
use cella_network::proxy_env::ProxyEnvVars;

use crate::{EnvForwarding, ForwardEnv};

/// Apply proxy environment forwarding to the container env.
///
/// When blocking rules are active, proxy env vars point to the cella-agent
/// proxy inside the container. Otherwise, they point directly to the
/// upstream proxy (or auto-detected host proxy).
pub fn apply_proxy_env(
    fwd: &mut EnvForwarding,
    proxy_config: &ProxyConfig,
    has_blocking_rules: bool,
) {
    let Some(proxy_vars) = ProxyEnvVars::detect(proxy_config) else {
        tracing::debug!("Proxy forwarding disabled in config");
        return;
    };

    if !proxy_vars.has_proxy() && !has_blocking_rules {
        tracing::debug!("No proxy env vars detected and no blocking rules");
        return;
    }

    let pairs = if has_blocking_rules {
        tracing::info!(
            "Blocking rules active: proxy env vars point to cella-agent proxy (port {})",
            proxy_config.proxy_port,
        );
        proxy_vars.to_agent_proxy_env_pairs(proxy_config.proxy_port)
    } else {
        tracing::info!("Forwarding host proxy env vars to container");
        proxy_vars.to_env_pairs(None)
    };

    for (key, value) in pairs {
        fwd.env.push(ForwardEnv { key, value });
    }
}

/// Build the JSON config string for the cella-agent forward proxy.
///
/// This is set as `CELLA_PROXY_CONFIG` env var in the container so
/// the agent can start its forward proxy with the right rules.
pub fn build_agent_proxy_config_json(config: &NetworkConfig) -> String {
    let proxy_env = ProxyEnvVars::detect(&config.proxy);
    let upstream = proxy_env.and_then(|p| p.http_proxy.or(p.https_proxy));

    let rules: Vec<serde_json::Value> = config
        .rules
        .iter()
        .map(|r| {
            let mut rule = serde_json::json!({
                "domain": r.domain,
                "action": match r.action {
                    cella_network::RuleAction::Block => "block",
                    cella_network::RuleAction::Allow => "allow",
                },
            });
            if !r.paths.is_empty() {
                rule["paths"] = serde_json::json!(r.paths);
            }
            rule
        })
        .collect();

    // Check if any rules require path-level inspection (MITM).
    // If so, ensure the CA cert/key are available and include them.
    let has_path_rules = config.rules.iter().any(|r| !r.paths.is_empty());
    let (ca_cert_pem, ca_key_pem) = if has_path_rules {
        match cella_network::ca::ensure_ca() {
            Ok(ca) => (Some(ca.cert_pem), Some(ca.key_pem)),
            Err(e) => {
                tracing::warn!(
                    "Failed to generate CA for MITM: {e}. Path-level blocking will be domain-only."
                );
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    let json = serde_json::json!({
        "listen_port": config.proxy.proxy_port,
        "mode": match config.mode {
            cella_network::NetworkMode::Denylist => "denylist",
            cella_network::NetworkMode::Allowlist => "allowlist",
        },
        "rules": rules,
        "upstream_proxy": upstream,
        "ca_cert_pem": ca_cert_pem,
        "ca_key_pem": ca_key_pem,
    });

    json.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cella_network::config::{NetworkConfig, NetworkMode, NetworkRule, ProxyConfig, RuleAction};

    #[test]
    fn test_apply_proxy_env_disabled_proxy() {
        let mut fwd = EnvForwarding::default();
        let config = ProxyConfig {
            enabled: false,
            ..Default::default()
        };
        apply_proxy_env(&mut fwd, &config, false);
        assert!(fwd.env.is_empty(), "disabled proxy should not add env vars");
    }

    #[test]
    fn test_apply_proxy_env_no_proxy_no_rules() {
        let mut fwd = EnvForwarding::default();
        let config = ProxyConfig::default();
        // In test env, no host HTTP_PROXY/HTTPS_PROXY are set, and no blocking rules.
        apply_proxy_env(&mut fwd, &config, false);
        assert!(
            fwd.env.is_empty(),
            "no host proxy and no blocking rules should not add env vars"
        );
    }

    #[test]
    fn test_apply_proxy_env_blocking_rules_adds_agent_proxy() {
        let mut fwd = EnvForwarding::default();
        let config = ProxyConfig::default();
        apply_proxy_env(&mut fwd, &config, true);

        // With blocking rules, agent proxy env vars should be injected.
        assert!(
            !fwd.env.is_empty(),
            "blocking rules should inject proxy env vars"
        );

        let expected_url = format!("http://127.0.0.1:{}", config.proxy_port);
        let http_proxy = fwd.env.iter().find(|e| e.key == "HTTP_PROXY");
        assert!(http_proxy.is_some(), "should set HTTP_PROXY");
        assert_eq!(http_proxy.unwrap().value, expected_url);

        let https_proxy = fwd.env.iter().find(|e| e.key == "HTTPS_PROXY");
        assert!(https_proxy.is_some(), "should set HTTPS_PROXY");
        assert_eq!(https_proxy.unwrap().value, expected_url);

        // Should also set lowercase variants.
        assert!(
            fwd.env.iter().any(|e| e.key == "http_proxy"),
            "should set http_proxy (lowercase)"
        );
        assert!(
            fwd.env.iter().any(|e| e.key == "https_proxy"),
            "should set https_proxy (lowercase)"
        );
    }

    #[test]
    fn test_build_agent_proxy_config_json_denylist() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            proxy: ProxyConfig::default(),
            rules: vec![NetworkRule {
                domain: "*.evil.com".to_string(),
                paths: vec![],
                action: RuleAction::Block,
            }],
        };

        let json_str = build_agent_proxy_config_json(&config);
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(json["mode"], "denylist");
        assert_eq!(json["listen_port"], config.proxy.proxy_port);

        let rules = json["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["domain"], "*.evil.com");
        assert_eq!(rules[0]["action"], "block");
    }

    #[test]
    fn test_build_agent_proxy_config_json_allowlist() {
        let config = NetworkConfig {
            mode: NetworkMode::Allowlist,
            proxy: ProxyConfig::default(),
            rules: vec![
                NetworkRule {
                    domain: "registry.npmjs.org".to_string(),
                    paths: vec![],
                    action: RuleAction::Allow,
                },
                NetworkRule {
                    domain: "github.com".to_string(),
                    paths: vec![],
                    action: RuleAction::Allow,
                },
            ],
        };

        let json_str = build_agent_proxy_config_json(&config);
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(json["mode"], "allowlist");

        let rules = json["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["domain"], "registry.npmjs.org");
        assert_eq!(rules[1]["domain"], "github.com");
    }

    #[test]
    fn test_build_agent_proxy_config_json_with_path_rules() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            proxy: ProxyConfig::default(),
            rules: vec![NetworkRule {
                domain: "api.example.com".to_string(),
                paths: vec!["/v1/admin/*".to_string(), "/internal/*".to_string()],
                action: RuleAction::Block,
            }],
        };

        let json_str = build_agent_proxy_config_json(&config);
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        let rules = json["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["domain"], "api.example.com");

        let paths = rules[0]["paths"].as_array().unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], "/v1/admin/*");
        assert_eq!(paths[1], "/internal/*");
    }

    #[test]
    fn test_build_agent_proxy_config_json_no_rules() {
        let config = NetworkConfig {
            mode: NetworkMode::Denylist,
            proxy: ProxyConfig::default(),
            rules: vec![],
        };

        let json_str = build_agent_proxy_config_json(&config);
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        let rules = json["rules"].as_array().unwrap();
        assert!(rules.is_empty(), "rules array should be empty");
        assert_eq!(json["mode"], "denylist");
    }
}
