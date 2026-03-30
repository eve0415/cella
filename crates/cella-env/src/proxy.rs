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

    let json = serde_json::json!({
        "listen_port": config.proxy.proxy_port,
        "mode": match config.mode {
            cella_network::NetworkMode::Denylist => "denylist",
            cella_network::NetworkMode::Allowlist => "allowlist",
        },
        "rules": rules,
        "upstream_proxy": upstream,
    });

    json.to_string()
}
