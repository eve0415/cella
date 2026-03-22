//! TCP control server: receives messages from in-container agents.
//!
//! A single TCP listener (bound at daemon startup) accepts connections from all
//! containers.  Each agent identifies itself via `AgentHello.container_name` and
//! is validated against the daemon's auth token.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cella_port::protocol::{
    AgentHello, AgentMessage, DaemonHello, DaemonMessage, PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::CellaDaemonError;
use crate::browser::BrowserHandler;
use crate::credential::invoke_git_credential;
use crate::port_manager::PortManager;
use crate::proxy::ProxyCommand;

/// Tracks whether an agent has actually connected and sent messages.
pub struct AgentConnectionState {
    pub connected: AtomicBool,
    pub last_seen_secs: AtomicU64,
}

impl AgentConnectionState {
    pub const fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            last_seen_secs: AtomicU64::new(0),
        }
    }
}

impl Default for AgentConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle for a registered container (no per-container server task).
pub struct ContainerHandle {
    pub container_id: String,
    pub agent_state: Arc<AgentConnectionState>,
}

/// Run the unified TCP control server.
///
/// Accepts agent connections, validates auth tokens, looks up container names
/// in `container_handles`, and routes messages to existing handlers.
#[allow(clippy::too_many_arguments, clippy::implicit_hasher)]
pub async fn run_control_server(
    listener: TcpListener,
    auth_token: String,
    last_activity: Arc<AtomicU64>,
    port_manager: Arc<Mutex<PortManager>>,
    browser_handler: Arc<BrowserHandler>,
    container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
    proxy_cmd_tx: tokio::sync::mpsc::Sender<ProxyCommand>,
    is_orbstack: bool,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, peer)) => {
                        last_activity.store(current_time_secs(), Ordering::Relaxed);
                        debug!("Agent TCP connection from {peer}");
                        let pm = port_manager.clone();
                        let bh = browser_handler.clone();
                        let handles = container_handles.clone();
                        let ptx = proxy_cmd_tx.clone();
                        let token = auth_token.clone();
                        let orbstack = is_orbstack;
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_agent_connection(stream, &token, pm, bh, handles, ptx, orbstack)
                                    .await
                            {
                                warn!("Agent connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Control server accept error: {e}");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("Control server shutting down");
                    break;
                }
            }
        }
    }
}

/// Handle a single agent TCP connection (newline-delimited JSON).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_agent_connection(
    stream: tokio::net::TcpStream,
    auth_token: &str,
    port_manager: Arc<Mutex<PortManager>>,
    browser_handler: Arc<BrowserHandler>,
    container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
    proxy_cmd_tx: tokio::sync::mpsc::Sender<ProxyCommand>,
    is_orbstack: bool,
) -> Result<(), CellaDaemonError> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // --- Hello handshake ---
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("hello read error: {e}"),
        })?;
    if n == 0 {
        return Ok(());
    }

    let Ok(agent_hello) = serde_json::from_str::<AgentHello>(line.trim()) else {
        let reject = DaemonHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            error: Some("Hello required as first message".to_string()),
        };
        let mut json = serde_json::to_string(&reject).unwrap_or_default();
        json.push('\n');
        let _ = writer.write_all(json.as_bytes()).await;
        let _ = writer.flush().await;
        return Ok(());
    };

    if agent_hello.protocol_version != PROTOCOL_VERSION {
        let reject = DaemonHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            error: Some(format!(
                "protocol version mismatch: agent={} daemon={}",
                agent_hello.protocol_version, PROTOCOL_VERSION
            )),
        };
        let mut json = serde_json::to_string(&reject).unwrap_or_default();
        json.push('\n');
        let _ = writer.write_all(json.as_bytes()).await;
        let _ = writer.flush().await;
        return Ok(());
    }

    // Validate auth token
    if agent_hello.auth_token != auth_token {
        let reject = DaemonHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            error: Some("invalid auth token".to_string()),
        };
        let mut json = serde_json::to_string(&reject).unwrap_or_default();
        json.push('\n');
        let _ = writer.write_all(json.as_bytes()).await;
        let _ = writer.flush().await;
        return Ok(());
    }

    // Look up container by name
    let container_name = agent_hello.container_name.clone();
    let (container_id, agent_state) = {
        let handles = container_handles.lock().await;
        if let Some(h) = handles.get(&container_name) {
            (h.container_id.clone(), h.agent_state.clone())
        } else {
            let reject = DaemonHello {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                error: Some(format!("unknown container: {container_name}")),
            };
            let mut json = serde_json::to_string(&reject).unwrap_or_default();
            json.push('\n');
            let _ = writer.write_all(json.as_bytes()).await;
            let _ = writer.flush().await;
            return Ok(());
        }
    };

    // Retrieve container IP for TCP proxy creation
    let container_ip = {
        let pm = port_manager.lock().await;
        pm.container_ip(&container_id).map(String::from)
    };

    if agent_hello.agent_version != env!("CARGO_PKG_VERSION") {
        warn!(
            "Agent version mismatch: agent={} daemon={}",
            agent_hello.agent_version,
            env!("CARGO_PKG_VERSION")
        );
    }

    // Send DaemonHello
    {
        let hello = DaemonHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            error: None,
        };
        let mut json = serde_json::to_string(&hello).map_err(|e| CellaDaemonError::Protocol {
            message: format!("hello serialize error: {e}"),
        })?;
        json.push('\n');
        writer
            .write_all(json.as_bytes())
            .await
            .map_err(|e| CellaDaemonError::Socket {
                message: format!("hello write error: {e}"),
            })?;
        writer.flush().await.map_err(|e| CellaDaemonError::Socket {
            message: format!("hello flush error: {e}"),
        })?;
    }

    info!("Agent connected for container {container_name}");

    // --- Message loop ---
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| CellaDaemonError::Socket {
                message: format!("read error: {e}"),
            })?;

        if n == 0 {
            break; // Connection closed
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: AgentMessage =
            serde_json::from_str(trimmed).map_err(|e| CellaDaemonError::Protocol {
                message: format!("invalid agent message: {e}"),
            })?;

        let response = handle_agent_message(
            msg,
            &port_manager,
            &browser_handler,
            Some(&container_id),
            Some(&proxy_cmd_tx),
            container_ip.as_deref(),
            &agent_state,
            is_orbstack,
        )
        .await;

        if let Some(resp) = response {
            let mut json =
                serde_json::to_string(&resp).map_err(|e| CellaDaemonError::Protocol {
                    message: format!("serialize error: {e}"),
                })?;
            json.push('\n');
            writer
                .write_all(json.as_bytes())
                .await
                .map_err(|e| CellaDaemonError::Socket {
                    message: format!("write error: {e}"),
                })?;
            writer.flush().await.map_err(|e| CellaDaemonError::Socket {
                message: format!("flush error: {e}"),
            })?;
        }
    }

    Ok(())
}

/// Route an agent message to the appropriate handler.
#[allow(clippy::too_many_arguments, clippy::similar_names)]
pub async fn handle_agent_message(
    msg: AgentMessage,
    port_manager: &Arc<Mutex<PortManager>>,
    browser_handler: &Arc<BrowserHandler>,
    container_id: Option<&str>,
    proxy_cmd_tx: Option<&tokio::sync::mpsc::Sender<ProxyCommand>>,
    container_ip: Option<&str>,
    agent_state: &Arc<AgentConnectionState>,
    _is_orbstack: bool,
) -> Option<DaemonMessage> {
    agent_state.connected.store(true, Ordering::Relaxed);
    agent_state
        .last_seen_secs
        .store(current_time_secs(), Ordering::Relaxed);

    match msg {
        AgentMessage::PortOpen {
            port,
            protocol,
            process,
            bind,
            proxy_port,
        } => {
            let cid = container_id.unwrap_or("unknown").to_string();
            debug!(
                "Port open: {port}/{protocol} (process: {process:?}, bind: {bind:?}, proxy_port: {proxy_port:?}) from {cid}"
            );
            let host_port = {
                let mut pm = port_manager.lock().await;
                pm.handle_port_open(&cid, port, protocol, process)
            };

            // Use agent's proxy port if available (for localhost-bound services),
            // otherwise connect directly to the app port.
            let target_port = proxy_port.unwrap_or(port);

            // Start TCP proxy on host for localhost access.
            if let (Some(hp), Some(tx), Some(ip)) = (host_port, proxy_cmd_tx, container_ip) {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                let _ = tx
                    .send(ProxyCommand::Start {
                        host_port: hp,
                        container_ip: ip.to_string(),
                        container_port: target_port,
                        result_tx: Some(result_tx),
                    })
                    .await;

                // If proxy bind fails (TOCTOU: port grabbed between allocation
                // check and actual bind), roll back so the port isn't reported
                // as forwarded.
                match result_rx.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!(
                            "Proxy bind failed for port {hp} (container port {port}): {e}. \
                             Rolling back allocation."
                        );
                        let mut pm = port_manager.lock().await;
                        pm.handle_port_closed(&cid, port, protocol);
                        return None;
                    }
                    Err(_) => {
                        warn!("Proxy coordinator dropped result channel for port {hp}");
                    }
                }
            }

            // Notify agent of the port mapping so it can expose it inside the container.
            host_port.map(|hp| DaemonMessage::PortMapping {
                container_port: port,
                host_port: hp,
            })
        }
        AgentMessage::PortClosed { port, protocol } => {
            let cid = container_id.unwrap_or("unknown").to_string();
            debug!("Port closed: {port}/{protocol} from {cid}");
            let host_port = {
                let mut pm = port_manager.lock().await;
                pm.handle_port_closed(&cid, port, protocol)
            };

            // Stop TCP proxy
            if let (Some(hp), Some(tx)) = (host_port, proxy_cmd_tx) {
                let _ = tx.send(ProxyCommand::Stop { host_port: hp }).await;
            }

            None
        }
        AgentMessage::BrowserOpen { url } => {
            let rewritten = if let Some(cid) = container_id {
                rewrite_browser_url(&url, port_manager, cid).await
            } else {
                url.clone()
            };
            if rewritten != url {
                info!("Browser open request: {url} -> {rewritten}");
            } else {
                info!("Browser open request: {url}");
            }
            // Wait for the proxy to be ready before opening the browser.
            // The proxy may have just been started by a preceding PortOpen.
            if let Some(port) = extract_port(&rewritten) {
                wait_for_proxy_ready(port).await;
            }
            browser_handler.open_url(&rewritten);
            None
        }
        AgentMessage::CredentialRequest {
            id,
            operation,
            fields,
        } => {
            debug!("Credential request: op={operation} id={id}");
            let result =
                tokio::task::spawn_blocking(move || invoke_git_credential(&operation, &fields))
                    .await;

            match result {
                Ok(Ok(response_fields)) => Some(DaemonMessage::CredentialResponse {
                    id,
                    fields: response_fields,
                }),
                Ok(Err(e)) => {
                    warn!("Git credential error: {e}");
                    Some(DaemonMessage::CredentialResponse {
                        id,
                        fields: std::collections::HashMap::new(),
                    })
                }
                Err(e) => {
                    warn!("Credential task join error: {e}");
                    Some(DaemonMessage::CredentialResponse {
                        id,
                        fields: std::collections::HashMap::new(),
                    })
                }
            }
        }
        AgentMessage::Health {
            uptime_secs,
            ports_detected,
        } => {
            debug!("Agent health: uptime={uptime_secs}s ports={ports_detected}");
            None
        }
    }
}

/// Extract the port number from a URL like `http://localhost:3000/path`.
fn extract_port(url: &str) -> Option<u16> {
    let rest = url.split_once("://")?.1;
    let host_port = match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    };
    host_port.rsplit_once(':')?.1.parse().ok()
}

/// Wait for a TCP proxy to accept connections, polling up to 2 seconds.
async fn wait_for_proxy_ready(port: u16) {
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    debug!("Proxy readiness timeout for port {port}, opening browser anyway");
}

/// Rewrite a browser URL to use the correct host port.
///
/// If the URL targets a container port that has been forwarded to a different
/// host port, this rewrites the URL to use the host port so it's reachable
/// from the host machine.
async fn rewrite_browser_url(
    url: &str,
    port_manager: &Arc<Mutex<PortManager>>,
    container_id: &str,
) -> String {
    // Try to parse as a URL with a port
    let Some((before_host, rest)) = url.split_once("://") else {
        return url.to_string();
    };

    // Extract host:port from the URL
    let (host_port_part, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };

    let (host, port_str) = match host_port_part.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => return url.to_string(),
    };

    let Ok(container_port) = port_str.parse::<u16>() else {
        return url.to_string();
    };

    // Only rewrite localhost/127.0.0.1 URLs
    if host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
        return url.to_string();
    }

    // Look up the forwarded host port
    let pm = port_manager.lock().await;
    let forwarded = pm.all_forwarded_ports();
    let mapping = forwarded
        .iter()
        .find(|p| p.container_id == container_id && p.container_port == container_port);

    if let Some(info) = mapping {
        if info.host_port != container_port {
            return format!("{before_host}://localhost:{}{path}", info.host_port);
        }
    }

    url.to_string()
}

/// Get the current time in seconds since the Unix epoch.
pub fn current_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use cella_port::protocol::PortProtocol;

    use super::*;

    /// Helper to set up a port manager with a forwarded port for testing.
    async fn pm_with_forwarded_port(container_port: u16) -> Arc<Mutex<PortManager>> {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "test", Some("172.20.0.5".to_string()), vec![], None);
            guard.handle_port_open("c1", container_port, PortProtocol::Tcp, None);
        }
        pm
    }

    #[tokio::test]
    async fn rewrite_url_no_remap() {
        let pm = pm_with_forwarded_port(3000).await;
        // Port 3000 maps to 3000 (no conflict) — no rewrite
        let result = rewrite_browser_url("http://localhost:3000/callback", &pm, "c1").await;
        assert_eq!(result, "http://localhost:3000/callback");
    }

    #[tokio::test]
    async fn rewrite_url_with_remap() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "test-a", Some("172.20.0.5".to_string()), vec![], None);
            guard.register_container("c2", "test-b", Some("172.20.0.6".to_string()), vec![], None);
            // c1 gets port 3000
            guard.handle_port_open("c1", 3000, PortProtocol::Tcp, None);
            // c2 also wants 3000 — gets remapped to 3001
            guard.handle_port_open("c2", 3000, PortProtocol::Tcp, None);
        }
        let result = rewrite_browser_url("http://localhost:3000/auth", &pm, "c2").await;
        assert_eq!(result, "http://localhost:3001/auth");
    }

    #[tokio::test]
    async fn rewrite_url_127_0_0_1() {
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        {
            let mut guard = pm.lock().await;
            guard.register_container("c1", "a", Some("172.20.0.5".to_string()), vec![], None);
            guard.register_container("c2", "b", Some("172.20.0.6".to_string()), vec![], None);
            guard.handle_port_open("c1", 8080, PortProtocol::Tcp, None);
            guard.handle_port_open("c2", 8080, PortProtocol::Tcp, None);
        }
        let result = rewrite_browser_url("http://127.0.0.1:8080", &pm, "c2").await;
        assert_eq!(result, "http://localhost:8081");
    }

    #[tokio::test]
    async fn rewrite_url_non_localhost_untouched() {
        let pm = pm_with_forwarded_port(3000).await;
        let result = rewrite_browser_url("http://example.com:3000/path", &pm, "c1").await;
        assert_eq!(result, "http://example.com:3000/path");
    }

    #[tokio::test]
    async fn rewrite_url_no_port_untouched() {
        let pm = pm_with_forwarded_port(3000).await;
        let result = rewrite_browser_url("https://github.com/login", &pm, "c1").await;
        assert_eq!(result, "https://github.com/login");
    }

    #[tokio::test]
    async fn rewrite_url_unknown_port_untouched() {
        let pm = pm_with_forwarded_port(3000).await;
        let result = rewrite_browser_url("http://localhost:9999/path", &pm, "c1").await;
        assert_eq!(result, "http://localhost:9999/path");
    }
}
