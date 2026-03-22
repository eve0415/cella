//! Management server: handles CLI requests on `~/.cella/daemon.sock`.
//!
//! Accepts `ManagementRequest` messages and manages container registration,
//! port/status queries, and the unified TCP control server.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cella_port::protocol::{
    ContainerSummary, ForwardedPortDetail, ManagementRequest, ManagementResponse, PortAttributes,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::CellaDaemonError;
use crate::browser::BrowserHandler;
use crate::control_server::{AgentConnectionState, ContainerHandle, current_time_secs};
use crate::port_manager::PortManager;
use crate::proxy::ProxyCommand;

/// Run the management server on the given Unix socket.
///
/// Also spawns the unified TCP control server for agent connections.
///
/// # Errors
///
/// Returns error if socket binding fails.
#[allow(clippy::too_many_arguments)]
pub async fn run_management_server(
    socket_path: &Path,
    last_activity: Arc<AtomicU64>,
    port_manager: Arc<Mutex<PortManager>>,
    browser_handler: Arc<BrowserHandler>,
    proxy_cmd_tx: mpsc::Sender<ProxyCommand>,
    start_time: std::time::Instant,
    is_orbstack: bool,
    daemon_started_at: u64,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    control_listener: tokio::net::TcpListener,
    auth_token: String,
    control_port: u16,
) -> Result<(), CellaDaemonError> {
    // Clean up stale socket
    let _ = std::fs::remove_file(socket_path);

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CellaDaemonError::Socket {
            message: format!("failed to create directory {}: {e}", parent.display()),
        })?;
    }

    let listener = UnixListener::bind(socket_path).map_err(|e| CellaDaemonError::Socket {
        message: format!(
            "failed to bind management socket {}: {e}",
            socket_path.display()
        ),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(socket_path, perms);
    }

    info!("Management server listening on {}", socket_path.display());

    let container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Spawn the unified TCP control server for agent connections
    let ctrl_shutdown_rx = shutdown_tx.subscribe();
    {
        let pm = port_manager.clone();
        let bh = browser_handler.clone();
        let handles = container_handles.clone();
        let activity = last_activity.clone();
        let ptx = proxy_cmd_tx.clone();
        let token = auth_token.clone();
        tokio::spawn(async move {
            crate::control_server::run_control_server(
                control_listener,
                token,
                activity,
                pm,
                bh,
                handles,
                ptx,
                is_orbstack,
                ctrl_shutdown_rx,
            )
            .await;
        });
    }

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        last_activity.store(current_time_secs(), Ordering::Relaxed);
                        let pm = port_manager.clone();
                        let bh = browser_handler.clone();
                        let handles = container_handles.clone();
                        let proxy_tx = proxy_cmd_tx.clone();
                        let start = start_time;
                        let orbstack = is_orbstack;
                        let started_at = daemon_started_at;
                        let stx = shutdown_tx.clone();
                        let token = auth_token.clone();
                        let cport = control_port;

                        tokio::spawn(async move {
                            if let Err(e) = handle_management_connection(
                                stream, pm, bh, handles, proxy_tx, start, orbstack,
                                started_at, stx, &token, cport,
                            )
                            .await
                            {
                                warn!("Management connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Management server accept error: {e}");
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("Shutdown signal received, stopping management server");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Handle a single management connection (newline-delimited JSON).
#[allow(clippy::too_many_arguments)]
async fn handle_management_connection(
    stream: tokio::net::UnixStream,
    port_manager: Arc<Mutex<PortManager>>,
    browser_handler: Arc<BrowserHandler>,
    container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
    proxy_cmd_tx: mpsc::Sender<ProxyCommand>,
    start_time: std::time::Instant,
    is_orbstack: bool,
    daemon_started_at: u64,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    auth_token: &str,
    control_port: u16,
) -> Result<(), CellaDaemonError> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| CellaDaemonError::Socket {
                message: format!("management read error: {e}"),
            })?;

        if n == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: ManagementRequest =
            serde_json::from_str(trimmed).map_err(|e| CellaDaemonError::Protocol {
                message: format!("invalid management request: {e}"),
            })?;

        let response = handle_management_request(
            req,
            &port_manager,
            &browser_handler,
            &container_handles,
            &proxy_cmd_tx,
            start_time,
            is_orbstack,
            daemon_started_at,
            &shutdown_tx,
            auth_token,
            control_port,
        )
        .await;

        let mut json =
            serde_json::to_string(&response).map_err(|e| CellaDaemonError::Protocol {
                message: format!("serialize error: {e}"),
            })?;
        json.push('\n');
        writer
            .write_all(json.as_bytes())
            .await
            .map_err(|e| CellaDaemonError::Socket {
                message: format!("management write error: {e}"),
            })?;
        writer.flush().await.map_err(|e| CellaDaemonError::Socket {
            message: format!("management flush error: {e}"),
        })?;
    }

    Ok(())
}

/// Route a management request to the appropriate handler.
#[allow(clippy::too_many_arguments)]
async fn handle_management_request(
    req: ManagementRequest,
    port_manager: &Arc<Mutex<PortManager>>,
    _browser_handler: &Arc<BrowserHandler>,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
    _proxy_cmd_tx: &mpsc::Sender<ProxyCommand>,
    start_time: std::time::Instant,
    is_orbstack: bool,
    daemon_started_at: u64,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
    auth_token: &str,
    control_port: u16,
) -> ManagementResponse {
    match req {
        ManagementRequest::RegisterContainer {
            container_id,
            container_name,
            container_ip,
            ports_attributes,
            other_ports_attributes,
            forward_ports,
        } => {
            handle_register(
                &container_id,
                &container_name,
                container_ip,
                ports_attributes,
                other_ports_attributes,
                &forward_ports,
                port_manager,
                container_handles,
            )
            .await
        }
        ManagementRequest::DeregisterContainer { container_name } => {
            handle_deregister(&container_name, port_manager, container_handles).await
        }
        ManagementRequest::QueryPorts => handle_query_ports(port_manager).await,
        ManagementRequest::QueryStatus => {
            handle_query_status(
                port_manager,
                container_handles,
                start_time,
                is_orbstack,
                daemon_started_at,
                auth_token,
                control_port,
            )
            .await
        }
        ManagementRequest::Ping => ManagementResponse::Pong,
        ManagementRequest::Shutdown => {
            let pid = std::process::id();
            info!("Shutdown requested, sending signal");
            let _ = shutdown_tx.send(true);
            ManagementResponse::ShuttingDown { pid }
        }
    }
}

/// Handle container registration.
#[allow(clippy::similar_names)]
async fn handle_register(
    container_id: &str,
    container_name: &str,
    container_ip: Option<String>,
    ports_attributes: Vec<PortAttributes>,
    other_ports_attributes: Option<PortAttributes>,
    forward_ports: &[u16],
    port_manager: &Arc<Mutex<PortManager>>,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
) -> ManagementResponse {
    // Register with port manager
    {
        let mut pm = port_manager.lock().await;
        pm.register_container(
            container_id,
            container_name,
            container_ip,
            ports_attributes,
            other_ports_attributes,
        );

        // Pre-allocate host ports for forwardPorts
        for &fwd_port in forward_ports {
            pm.handle_port_open(
                container_id,
                fwd_port,
                cella_port::protocol::PortProtocol::Tcp,
                None,
            );
        }
    }

    // Store handle for agent connection tracking
    {
        let mut handles = container_handles.lock().await;
        handles.insert(
            container_name.to_string(),
            ContainerHandle {
                container_id: container_id.to_string(),
                agent_state: Arc::new(AgentConnectionState::new()),
            },
        );
    }

    info!("Registered container {container_name}");

    ManagementResponse::ContainerRegistered {
        container_name: container_name.to_string(),
    }
}

/// Handle container deregistration.
async fn handle_deregister(
    container_name: &str,
    port_manager: &Arc<Mutex<PortManager>>,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
) -> ManagementResponse {
    let mut ports_released = 0;

    // Remove from container handles
    let removed = {
        let mut handles = container_handles.lock().await;
        handles.remove(container_name)
    };

    if let Some(handle) = removed {
        // Unregister from port manager
        let mut pm = port_manager.lock().await;
        let ports_before = pm.all_forwarded_ports().len();
        pm.unregister_container(&handle.container_id);
        let ports_after = pm.all_forwarded_ports().len();
        drop(pm);
        ports_released = ports_before.saturating_sub(ports_after);
    }

    info!("Deregistered container {container_name} ({ports_released} ports released)");

    ManagementResponse::ContainerDeregistered {
        container_name: container_name.to_string(),
        ports_released,
    }
}

/// Handle port query.
async fn handle_query_ports(port_manager: &Arc<Mutex<PortManager>>) -> ManagementResponse {
    let ports = {
        let pm = port_manager.lock().await;
        pm.all_forwarded_ports()
            .into_iter()
            .map(|p| ForwardedPortDetail {
                container_name: p.container_name.clone(),
                container_port: p.container_port,
                host_port: p.host_port,
                protocol: p.protocol,
                process: p.process.clone(),
                url: p.url(),
            })
            .collect()
    };

    ManagementResponse::Ports { ports }
}

/// Handle status query.
async fn handle_query_status(
    port_manager: &Arc<Mutex<PortManager>>,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
    start_time: std::time::Instant,
    is_orbstack: bool,
    daemon_started_at: u64,
    auth_token: &str,
    control_port: u16,
) -> ManagementResponse {
    let handles = container_handles.lock().await;
    let pm = port_manager.lock().await;

    let containers: Vec<ContainerSummary> = handles
        .iter()
        .map(|(name, handle)| {
            let port_count = pm
                .all_forwarded_ports()
                .iter()
                .filter(|p| p.container_name == *name)
                .count();
            ContainerSummary {
                container_name: name.clone(),
                container_id: handle.container_id.clone(),
                forwarded_port_count: port_count,
                agent_connected: handle.agent_state.connected.load(Ordering::Relaxed),
                last_seen_secs: handle.agent_state.last_seen_secs.load(Ordering::Relaxed),
            }
        })
        .collect();
    let container_count = containers.len();
    drop(pm);
    drop(handles);

    ManagementResponse::Status {
        pid: std::process::id(),
        uptime_secs: start_time.elapsed().as_secs(),
        container_count,
        containers,
        is_orbstack,
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        daemon_started_at,
        control_port,
        control_token: auth_token.to_string(),
    }
}

/// Send a management request to the daemon and receive the response.
///
/// Used by CLI commands to communicate with the running daemon.
///
/// # Errors
///
/// Returns error if the daemon is unreachable or the response is invalid.
pub async fn send_management_request(
    socket_path: &Path,
    request: &ManagementRequest,
) -> Result<ManagementResponse, CellaDaemonError> {
    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!(
                "failed to connect to management socket {}: {e}",
                socket_path.display()
            ),
        })?;

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    let mut json = serde_json::to_string(request).map_err(|e| CellaDaemonError::Protocol {
        message: format!("serialize request: {e}"),
    })?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("write request: {e}"),
        })?;
    writer.flush().await.map_err(|e| CellaDaemonError::Socket {
        message: format!("flush request: {e}"),
    })?;

    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .await
        .map_err(|e| CellaDaemonError::Socket {
            message: format!("read response: {e}"),
        })?;

    serde_json::from_str(response_line.trim()).map_err(|e| CellaDaemonError::Protocol {
        message: format!("parse response: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn management_ping_pong() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmt.sock");
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        let bh = Arc::new(BrowserHandler::new());
        let (proxy_tx, _proxy_rx) = mpsc::channel(16);
        let start = std::time::Instant::now();

        // Create a TCP listener for the control server
        let ctrl_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ctrl_port = ctrl_listener.local_addr().unwrap().port();

        let sock = socket_path.clone();
        let server_handle = tokio::spawn({
            let pm = pm.clone();
            let bh = bh.clone();
            let activity = Arc::new(AtomicU64::new(0));
            async move {
                let (stx, srx) = tokio::sync::watch::channel(false);
                let _ = run_management_server(
                    &sock,
                    activity,
                    pm,
                    bh,
                    proxy_tx,
                    start,
                    false,
                    0,
                    stx,
                    srx,
                    ctrl_listener,
                    "test-token".to_string(),
                    ctrl_port,
                )
                .await;
            }
        });

        // Wait for socket to appear
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let resp = send_management_request(&socket_path, &ManagementRequest::Ping)
            .await
            .unwrap();
        assert!(matches!(resp, ManagementResponse::Pong));

        server_handle.abort();
    }

    #[tokio::test]
    async fn management_register_and_query() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmt.sock");
        let pm = Arc::new(Mutex::new(PortManager::new(false)));
        let bh = Arc::new(BrowserHandler::new());
        let (proxy_tx, _proxy_rx) = mpsc::channel(16);
        let start = std::time::Instant::now();

        let ctrl_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ctrl_port = ctrl_listener.local_addr().unwrap().port();

        let sock = socket_path.clone();
        let server_handle = tokio::spawn({
            let pm = pm.clone();
            let bh = bh.clone();
            let activity = Arc::new(AtomicU64::new(0));
            async move {
                let (stx, srx) = tokio::sync::watch::channel(false);
                let _ = run_management_server(
                    &sock,
                    activity,
                    pm,
                    bh,
                    proxy_tx,
                    start,
                    false,
                    0,
                    stx,
                    srx,
                    ctrl_listener,
                    "test-token".to_string(),
                    ctrl_port,
                )
                .await;
            }
        });

        // Wait for socket
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Register container
        let resp = send_management_request(
            &socket_path,
            &ManagementRequest::RegisterContainer {
                container_id: "abc123".to_string(),
                container_name: "test-container".to_string(),
                container_ip: Some("172.20.0.5".to_string()),
                ports_attributes: vec![],
                other_ports_attributes: None,
                forward_ports: vec![],
            },
        )
        .await
        .unwrap();

        assert!(
            matches!(resp, ManagementResponse::ContainerRegistered { container_name, .. } if container_name == "test-container")
        );

        // Query status
        let resp = send_management_request(&socket_path, &ManagementRequest::QueryStatus)
            .await
            .unwrap();

        if let ManagementResponse::Status {
            container_count,
            control_port: cp,
            ..
        } = resp
        {
            assert_eq!(container_count, 1);
            assert_eq!(cp, ctrl_port);
        } else {
            panic!("Expected Status response");
        }

        // Query ports (none forwarded yet)
        let resp = send_management_request(&socket_path, &ManagementRequest::QueryPorts)
            .await
            .unwrap();

        assert!(matches!(resp, ManagementResponse::Ports { ports } if ports.is_empty()));

        // Deregister
        let resp = send_management_request(
            &socket_path,
            &ManagementRequest::DeregisterContainer {
                container_name: "test-container".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(
            matches!(resp, ManagementResponse::ContainerDeregistered { container_name, .. } if container_name == "test-container")
        );

        server_handle.abort();
    }
}
