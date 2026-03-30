//! Proxy environment variable forwarding for containers.
//!
//! Detects host proxy env vars and builds the env var set for injection
//! into containers, respecting cella network config overrides.

use cella_network::config::ProxyConfig;
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
