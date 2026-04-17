//! cella-agent: in-container agent for port detection, proxying, and credential forwarding.
//!
//! This binary runs inside dev containers started by cella. It:
//! - Polls /proc/net/tcp for new listeners and reports them to the host daemon
//! - Proxies localhost-bound apps to 0.0.0.0 so they're reachable from outside
//! - Handles BROWSER env var interception for opening URLs on the host
//! - Forwards git credential requests to the host daemon
//!
//! When invoked as `cella` (via symlink), enters CLI mode for in-container
//! worktree management commands that delegate to the host daemon.

mod browser;
mod cli;
mod control;
mod credential;
mod forward_proxy;
mod mitm;
mod plugin_sync;
mod port_proxy;
mod port_watcher;
mod proxy_config;
mod reconnecting_client;
mod state;

use std::time::Duration;

use tracing::{error, info};

/// Agent CLI arguments (parsed manually to avoid clap dep for smaller binary).
#[cfg_attr(test, derive(Debug))]
struct AgentArgs {
    command: AgentCommand,
}

#[cfg_attr(test, derive(Debug))]
enum AgentCommand {
    /// Run the agent daemon (port watching + credential helper).
    Daemon {
        poll_interval_ms: u64,
        proxy_config_json: Option<String>,
    },
    /// Open a URL in the host browser.
    BrowserOpen { url: String },
    /// Handle a git credential request.
    Credential { operation: String },
}

fn parse_args() -> Result<AgentArgs, String> {
    let args: Vec<String> = std::env::args().collect();
    parse_args_from(&args)
}

fn parse_args_from(args: &[String]) -> Result<AgentArgs, String> {
    if args.len() < 2 {
        return Err(format!(
            "Usage: {} <daemon|browser-open|credential> [options]",
            args.first().map_or("cella-agent", String::as_str)
        ));
    }

    match args[1].as_str() {
        "daemon" => {
            let mut poll_interval_ms = 1000u64;
            let mut proxy_config_json = None;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--poll-interval" => {
                        i += 1;
                        poll_interval_ms = args
                            .get(i)
                            .ok_or("missing --poll-interval value")?
                            .parse()
                            .map_err(|_| "invalid --poll-interval value")?;
                    }
                    "--proxy-config" => {
                        i += 1;
                        proxy_config_json =
                            Some(args.get(i).ok_or("missing --proxy-config value")?.clone());
                    }
                    other => return Err(format!("unknown flag: {other}")),
                }
                i += 1;
            }

            Ok(AgentArgs {
                command: AgentCommand::Daemon {
                    poll_interval_ms,
                    proxy_config_json,
                },
            })
        }
        "browser-open" => {
            let url = args.get(2).ok_or("missing URL argument")?.clone();
            Ok(AgentArgs {
                command: AgentCommand::BrowserOpen { url },
            })
        }
        "credential" => {
            let operation = args.get(2).ok_or("missing operation argument")?.clone();
            Ok(AgentArgs {
                command: AgentCommand::Credential { operation },
            })
        }
        other => Err(format!("unknown command: {other}")),
    }
}

/// Check if this binary was invoked as `cella` (CLI mode) vs `cella-agent`.
fn is_cli_mode() -> bool {
    let exe = std::env::args().next().unwrap_or_default();
    let stem = std::path::Path::new(&exe)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Invoked as "cella" but not "cella-agent" or "cella-browser"
    stem == "cella"
}

#[tokio::main]
async fn main() {
    // Check if invoked as `cella` (CLI mode via symlink)
    if is_cli_mode() {
        let args: Vec<String> = std::env::args().collect();
        let command = cli::parse_cli_args(&args);
        if let Err(e) = cli::run(command).await {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        return;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CELLA_AGENT_LOG")
                .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("cella-agent: {e}");
            std::process::exit(1);
        }
    };

    match args.command {
        AgentCommand::Daemon {
            poll_interval_ms,
            proxy_config_json,
        } => {
            info!("cella-agent daemon starting (poll interval: {poll_interval_ms}ms)");
            run_daemon(poll_interval_ms, proxy_config_json).await;
        }
        AgentCommand::BrowserOpen { url } => {
            if let Err(e) = browser::send_browser_open(&url).await {
                error!("Failed to open browser: {e}");
                std::process::exit(1);
            }
        }
        AgentCommand::Credential { operation } => {
            if let Err(e) = credential::handle_credential(&operation).await {
                error!("Credential error: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Resolve and start the forward proxy if configured.
///
/// Checks `CELLA_NO_NETWORK_RULES`, then falls back to the CLI arg or
/// `CELLA_PROXY_CONFIG` env var.
async fn maybe_start_forward_proxy(proxy_config_json: Option<String>) {
    let rules_disabled = std::env::var("CELLA_NO_NETWORK_RULES")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let proxy_json = if rules_disabled {
        info!("Network rules disabled via CELLA_NO_NETWORK_RULES");
        None
    } else {
        proxy_config_json.or_else(|| {
            let val = std::env::var("CELLA_PROXY_CONFIG").ok()?;
            if val.starts_with('/') {
                match std::fs::read_to_string(&val) {
                    Ok(content) => Some(content),
                    Err(e) => {
                        tracing::warn!("Failed to read proxy config from {val}: {e}");
                        None
                    }
                }
            } else {
                Some(val)
            }
        })
    };
    if let Some(ref json) = proxy_json {
        match proxy_config::AgentProxyConfig::from_json(json) {
            Ok(config) => {
                let config = std::sync::Arc::new(config);
                match forward_proxy::start_forward_proxy(config).await {
                    Ok(_handle) => info!("Forward proxy started"),
                    Err(e) => error!("Failed to start forward proxy: {e}"),
                }
            }
            Err(e) => error!("Invalid proxy config: {e}"),
        }
    }
}

async fn run_daemon(poll_interval_ms: u64, proxy_config_json: Option<String>) {
    let poll_interval = Duration::from_millis(poll_interval_ms);

    maybe_start_forward_proxy(proxy_config_json).await;

    // Publish Disconnected early so an in-container `cella doctor` run before
    // the handshake completes can tell the process is alive and still trying.
    let state_writer = state::spawn_state_writer(
        std::path::PathBuf::from(state::DEFAULT_STATE_FILE),
        env!("CARGO_PKG_VERSION").to_string(),
        state::AgentState::Disconnected,
        Duration::from_secs(10),
    );

    // Read connection info: .daemon_addr file is authoritative (updated on
    // every `cella up`), env vars are fallback (may be stale after restart).
    let (addr, token) = if let Some(info) = control::read_daemon_addr_file() {
        info!("Using daemon address from .daemon_addr file");
        (info.addr, info.token)
    } else if let Ok(addr) = std::env::var("CELLA_DAEMON_ADDR") {
        let token = std::env::var("CELLA_DAEMON_TOKEN").unwrap_or_default();
        (addr, token)
    } else {
        info!("No daemon address available, running in standalone mode");
        port_watcher::run_standalone(poll_interval).await;
        return;
    };
    let container_name = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();

    // Block until the daemon accepts the handshake. Retries forever so a
    // late-starting daemon (e.g., the common case where `cella up` spawns
    // the container before the daemon finishes binding) recovers on its
    // own instead of falling into permanent standalone mode.
    let client =
        reconnecting_client::ReconnectingClient::connect_with_retry(&addr, &container_name, &token)
            .await;

    state_writer.set_daemon_addr(Some(addr.clone()));
    state_writer.set_state(state::AgentState::Connected);

    let control = std::sync::Arc::new(tokio::sync::Mutex::new(client));
    let reconnecting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let start = std::time::Instant::now();
    let ports_detected = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Spawn port watcher
    let ctrl = control.clone();
    let pd = ports_detected.clone();
    let rc = reconnecting.clone();
    let pm: port_watcher::PortMap =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let sw_for_watcher = state_writer.clone();
    let watcher_handle = tokio::spawn(async move {
        port_watcher::run(poll_interval, ctrl, pd, pm, rc, Some(sw_for_watcher)).await;
    });

    // Spawn plugin manifest sync watcher (reverse-rewrites paths back to host)
    let container_home = std::env::var("HOME").unwrap_or_default();
    if !container_home.is_empty() {
        tokio::spawn(plugin_sync::run(container_home));
    }

    // Spawn health reporter
    let ctrl = control.clone();
    let pd = ports_detected.clone();
    let rc = reconnecting.clone();
    let sw_for_health = state_writer.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let uptime = start.elapsed().as_secs();
            let msg = cella_protocol::AgentMessage::Health {
                uptime_secs: uptime,
                ports_detected: pd.load(std::sync::atomic::Ordering::Relaxed),
            };
            let mut c = ctrl.lock().await;
            if let Err(e) = c.send(&msg).await {
                tracing::warn!("Health report failed: {e}");
                drop(c);
                reconnecting_client::spawn_background_reconnect(
                    ctrl.clone(),
                    rc.clone(),
                    Some(sw_for_health.clone()),
                );
            }
        }
    });

    // Wait for watcher (runs forever)
    let _ = watcher_handle.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parse_daemon_defaults() {
        let a = parse_args_from(&args(&["cella-agent", "daemon"])).unwrap();
        assert!(matches!(
            a.command,
            AgentCommand::Daemon {
                poll_interval_ms: 1000,
                proxy_config_json: None
            }
        ));
    }

    #[test]
    fn parse_daemon_with_poll_interval() {
        let a =
            parse_args_from(&args(&["cella-agent", "daemon", "--poll-interval", "500"])).unwrap();
        assert!(matches!(
            a.command,
            AgentCommand::Daemon {
                poll_interval_ms: 500,
                proxy_config_json: None
            }
        ));
    }

    #[test]
    fn parse_daemon_with_proxy_config() {
        let json = r#"{"listen_port":8080}"#;
        let a = parse_args_from(&args(&["cella-agent", "daemon", "--proxy-config", json])).unwrap();
        assert!(
            matches!(a.command, AgentCommand::Daemon { poll_interval_ms: 1000, proxy_config_json: Some(ref j) } if j == json)
        );
    }

    #[test]
    fn parse_daemon_both_flags() {
        let json = r#"{"p":1}"#;
        let a = parse_args_from(&args(&[
            "cella-agent",
            "daemon",
            "--poll-interval",
            "250",
            "--proxy-config",
            json,
        ]))
        .unwrap();
        assert!(
            matches!(a.command, AgentCommand::Daemon { poll_interval_ms: 250, proxy_config_json: Some(ref j) } if j == json)
        );
    }

    #[test]
    fn parse_daemon_invalid_poll_interval() {
        let result = parse_args_from(&args(&["cella-agent", "daemon", "--poll-interval", "abc"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid --poll-interval"));
    }

    #[test]
    fn parse_daemon_missing_poll_interval_value() {
        let result = parse_args_from(&args(&["cella-agent", "daemon", "--poll-interval"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing --poll-interval"));
    }

    #[test]
    fn parse_daemon_missing_proxy_config_value() {
        let result = parse_args_from(&args(&["cella-agent", "daemon", "--proxy-config"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing --proxy-config"));
    }

    #[test]
    fn parse_daemon_unknown_flag() {
        let result = parse_args_from(&args(&["cella-agent", "daemon", "--bogus"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown flag"));
    }

    #[test]
    fn parse_browser_open() {
        let a = parse_args_from(&args(&[
            "cella-agent",
            "browser-open",
            "http://localhost:3000",
        ]))
        .unwrap();
        assert!(
            matches!(a.command, AgentCommand::BrowserOpen { url } if url == "http://localhost:3000")
        );
    }

    #[test]
    fn parse_browser_open_missing_url() {
        let result = parse_args_from(&args(&["cella-agent", "browser-open"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing URL"));
    }

    #[test]
    fn parse_credential() {
        let a = parse_args_from(&args(&["cella-agent", "credential", "get"])).unwrap();
        assert!(matches!(a.command, AgentCommand::Credential { operation } if operation == "get"));
    }

    #[test]
    fn parse_credential_store() {
        let a = parse_args_from(&args(&["cella-agent", "credential", "store"])).unwrap();
        assert!(
            matches!(a.command, AgentCommand::Credential { operation } if operation == "store")
        );
    }

    #[test]
    fn parse_credential_missing_operation() {
        let result = parse_args_from(&args(&["cella-agent", "credential"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing operation"));
    }

    #[test]
    fn parse_unknown_command() {
        let result = parse_args_from(&args(&["cella-agent", "foo"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown command: foo"));
    }

    #[test]
    fn parse_no_args_returns_usage_error() {
        let result = parse_args_from(&args(&["cella-agent"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Usage:"));
    }

    #[test]
    fn parse_empty_args_returns_error() {
        let result = parse_args_from(&[]);
        assert!(result.is_err());
    }
}
