//! `cella network` subcommands for inspecting proxy and blocking state.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct NetworkArgs {
    #[command(subcommand)]
    command: NetworkCommand,
}

#[derive(Subcommand)]
enum NetworkCommand {
    /// Show network proxy and blocking status.
    Status,
    /// Test whether a URL would be blocked or allowed.
    Test {
        /// The URL to test (e.g., `https://api.prod.internal/v1/data`).
        url: String,
    },
    /// Tail the proxy blocked-request log from a running container.
    Log,
}

impl NetworkArgs {
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            NetworkCommand::Status => execute_status(),
            NetworkCommand::Test { url } => execute_test(&url),
            NetworkCommand::Log => {
                execute_log();
                Ok(())
            }
        }
    }
}

fn execute_status() -> Result<(), Box<dyn std::error::Error>> {
    let workspace_root = std::env::current_dir()?;
    let settings = cella_config::Settings::load(&workspace_root);
    let net_config = settings.network.to_network_config();

    if !net_config.proxy.enabled {
        println!("Proxy: disabled");
        return Ok(());
    }

    let proxy_vars = cella_network::proxy_env::ProxyEnvVars::detect(&net_config.proxy);
    let has_proxy = proxy_vars
        .as_ref()
        .is_some_and(cella_network::ProxyEnvVars::has_proxy);

    if net_config.has_rules() {
        println!("Proxy: active (localhost:{})", net_config.proxy.proxy_port);
    } else if has_proxy {
        println!("Proxy: forwarding (passthrough)");
    } else {
        println!("Proxy: inactive (no proxy detected, no rules)");
    }

    if let Some(ref vars) = proxy_vars {
        if let Some(ref http) = vars.http_proxy {
            println!("Upstream HTTP: {http}");
        }
        if let Some(ref https) = vars.https_proxy {
            println!("Upstream HTTPS: {https}");
        }
    }

    // CA status.
    let ca_dir = dirs_home().join(".cella/proxy/ca.pem");
    if ca_dir.exists() {
        println!("CA: auto-generated (~/.cella/proxy/ca.pem)");
    } else {
        println!("CA: not generated");
    }

    // Rules.
    let mode_str = match net_config.mode {
        cella_network::NetworkMode::Denylist => "denylist",
        cella_network::NetworkMode::Allowlist => "allowlist",
    };
    println!("Mode: {mode_str} ({} rules)", net_config.rules.len());

    for rule in &net_config.rules {
        let action = match rule.action {
            cella_network::RuleAction::Block => "block",
            cella_network::RuleAction::Allow => "allow",
        };
        if rule.paths.is_empty() {
            println!("  {action}: {}", rule.domain);
        } else {
            println!("  {action}: {} [{}]", rule.domain, rule.paths.join(", "));
        }
    }

    Ok(())
}

fn execute_test(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_root = std::env::current_dir()?;
    let settings = cella_config::Settings::load(&workspace_root);
    let net_config = settings.network.to_network_config();

    if !net_config.has_rules() {
        println!("No blocking rules configured. All traffic is allowed.");
        return Ok(());
    }

    // Parse URL to extract domain and path.
    let (domain, path) = parse_url_parts(url);

    let matcher = cella_network::RuleMatcher::new(&net_config);
    let verdict = matcher.evaluate(&domain, &path);

    if verdict.allowed {
        println!("\u{2713} ALLOWED: {url}");
    } else {
        println!("\u{2717} BLOCKED: {url}");
    }
    println!("  {}", verdict.reason);

    Ok(())
}

fn execute_log() {
    // This runs on the host — it needs to exec into the container to read the log.
    // For now, print instructions.
    println!("To view the proxy log, run inside the container:");
    println!("  cat /tmp/.cella/proxy.log");
    println!();
    println!("Or from the host:");
    println!("  cella exec -- cat /tmp/.cella/proxy.log");
}

/// Parse a URL into (domain, path).
fn parse_url_parts(url: &str) -> (String, String) {
    // Strip scheme.
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    // Split host and path.
    let (host_port, path) = rest
        .find('/')
        .map_or((rest, "/"), |idx| (&rest[..idx], &rest[idx..]));

    // Strip port from host.
    let host = host_port
        .rfind(':')
        .map_or(host_port, |idx| &host_port[..idx]);

    (host.to_string(), path.to_string())
}

fn dirs_home() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    std::path::PathBuf::from(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_url() {
        let (domain, path) = parse_url_parts("https://api.prod.internal/v1/data");
        assert_eq!(domain, "api.prod.internal");
        assert_eq!(path, "/v1/data");
    }

    #[test]
    fn parse_http_url_with_port() {
        let (domain, path) = parse_url_parts("http://example.com:8080/api");
        assert_eq!(domain, "example.com");
        assert_eq!(path, "/api");
    }

    #[test]
    fn parse_bare_domain() {
        let (domain, path) = parse_url_parts("example.com");
        assert_eq!(domain, "example.com");
        assert_eq!(path, "/");
    }
}
