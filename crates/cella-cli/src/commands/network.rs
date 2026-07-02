//! `cella network` subcommands for inspecting proxy and blocking state.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::picker;

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
    /// View the proxy log from a running container.
    Log {
        /// Follow log output (streams new entries in real-time).
        #[arg(short, long)]
        follow: bool,

        /// Number of lines to show from the end of the log.
        #[arg(long, default_value = "100")]
        tail: u32,

        /// Explicit workspace folder path (defaults to current directory).
        #[arg(long)]
        workspace_folder: Option<PathBuf>,

        #[command(flatten)]
        backend: crate::backend::BackendArgs,
    },
}

impl NetworkArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.command {
            NetworkCommand::Status => execute_status(),
            NetworkCommand::Test { url } => execute_test(&url),
            NetworkCommand::Log {
                follow,
                tail,
                workspace_folder,
                backend,
            } => execute_log(follow, tail, workspace_folder.as_deref(), &backend).await,
        }
    }
}

fn execute_status() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workspace_root = std::env::current_dir()?;
    let resolved = cella_config::devcontainer::resolve::config(&workspace_root, None).ok();
    let settings = cella_config::CellaConfig::load(&workspace_root, resolved.as_ref())?;
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

fn execute_test(url: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let workspace_root = std::env::current_dir()?;
    let resolved = cella_config::devcontainer::resolve::config(&workspace_root, None).ok();
    let settings = cella_config::CellaConfig::load(&workspace_root, resolved.as_ref())?;
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

async fn execute_log(
    follow: bool,
    tail: u32,
    workspace_folder: Option<&std::path::Path>,
    backend: &crate::backend::BackendArgs,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = backend.resolve_client().await?;
    let cwd = super::resolve_workspace_folder(workspace_folder)?;

    let container = match client.as_ref().find_container(&cwd).await? {
        Some(c) => c,
        None if workspace_folder.is_none() => {
            let containers = client.as_ref().list_cella_containers(false).await?;
            picker::resolve_container_interactive(
                &containers,
                None,
                "Select a container for proxy log:",
                None,
            )?
        }
        None => return Err("No cella container found for this workspace".into()),
    };

    let cmd = if follow {
        vec![
            "tail".to_string(),
            "-f".to_string(),
            "-n".to_string(),
            tail.to_string(),
            "/tmp/.cella/proxy.log".to_string(),
        ]
    } else {
        vec![
            "tail".to_string(),
            "-n".to_string(),
            tail.to_string(),
            "/tmp/.cella/proxy.log".to_string(),
        ]
    };

    let opts = cella_backend::ExecOptions {
        cmd,
        user: None,
        env: None,
        working_dir: None,
    };

    if follow {
        client
            .as_ref()
            .exec_stream(
                &container.id,
                &opts,
                Box::new(std::io::stdout()),
                Box::new(std::io::stderr()),
            )
            .await?;
    } else {
        let result = client.as_ref().exec_command(&container.id, &opts).await?;
        if !result.stdout.is_empty() {
            print!("{}", result.stdout);
        }
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
        }
    }

    Ok(())
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

fn dirs_home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home)
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

    #[test]
    fn parse_https_root_path() {
        let (domain, path) = parse_url_parts("https://example.com/");
        assert_eq!(domain, "example.com");
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_http_no_path() {
        let (domain, path) = parse_url_parts("http://example.com");
        assert_eq!(domain, "example.com");
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_domain_with_port_no_path() {
        let (domain, path) = parse_url_parts("example.com:8080");
        assert_eq!(domain, "example.com");
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_deep_path() {
        let (domain, path) = parse_url_parts("https://api.example.com/v2/users/123");
        assert_eq!(domain, "api.example.com");
        assert_eq!(path, "/v2/users/123");
    }

    #[test]
    fn parse_url_with_query_string() {
        let (domain, path) = parse_url_parts("https://example.com/search?q=test");
        assert_eq!(domain, "example.com");
        assert_eq!(path, "/search?q=test");
    }

    #[test]
    fn dirs_home_returns_path() {
        let home = dirs_home();
        assert!(!home.as_os_str().is_empty());
    }

    #[test]
    fn parse_url_with_fragment() {
        let (domain, path) = parse_url_parts("https://example.com/page#section");
        assert_eq!(domain, "example.com");
        assert_eq!(path, "/page#section");
    }

    #[test]
    fn parse_bare_domain_with_path() {
        let (domain, path) = parse_url_parts("api.internal/v1/resource");
        assert_eq!(domain, "api.internal");
        assert_eq!(path, "/v1/resource");
    }

    #[test]
    fn parse_domain_with_subdomain_and_port() {
        let (domain, path) = parse_url_parts("https://api.staging.example.com:9443/health");
        assert_eq!(domain, "api.staging.example.com");
        assert_eq!(path, "/health");
    }

    #[test]
    fn parse_localhost_url() {
        let (domain, path) = parse_url_parts("http://localhost:3000/api");
        assert_eq!(domain, "localhost");
        assert_eq!(path, "/api");
    }

    #[test]
    fn parse_ip_address_url() {
        let (domain, path) = parse_url_parts("http://192.168.1.1:8080/status");
        assert_eq!(domain, "192.168.1.1");
        assert_eq!(path, "/status");
    }
}
