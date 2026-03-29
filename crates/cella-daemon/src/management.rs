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

/// Shared context for the management server and its connection handlers.
pub(crate) struct ManagementContext {
    pub last_activity: Arc<AtomicU64>,
    pub port_manager: Arc<Mutex<PortManager>>,
    pub browser_handler: Arc<BrowserHandler>,
    pub proxy_cmd_tx: mpsc::Sender<ProxyCommand>,
    pub start_time: std::time::Instant,
    pub is_orbstack: bool,
    pub daemon_started_at: u64,
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub auth_token: String,
    pub control_port: u16,
}

/// Bind the management Unix socket, cleaning up stale sockets and setting permissions.
fn bind_management_socket(socket_path: &Path) -> Result<UnixListener, CellaDaemonError> {
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

    crate::shared::set_socket_permissions(socket_path);

    info!("Management server listening on {}", socket_path.display());
    Ok(listener)
}

/// Run the management server on the given Unix socket.
///
/// Also spawns the unified TCP control server for agent connections.
///
/// # Errors
///
/// Returns error if socket binding fails.
pub(crate) async fn run_management_server(
    socket_path: &Path,
    ctx: ManagementContext,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    control_listener: tokio::net::TcpListener,
) -> Result<(), CellaDaemonError> {
    let listener = bind_management_socket(socket_path)?;

    let container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let ctx = Arc::new(ctx);

    // Spawn the unified TCP control server for agent connections
    let ctrl_shutdown_rx = ctx.shutdown_tx.subscribe();
    {
        let activity = ctx.last_activity.clone();
        let ctrl_ctx = crate::control_server::ControlContext {
            auth_token: ctx.auth_token.clone(),
            port_manager: ctx.port_manager.clone(),
            browser_handler: ctx.browser_handler.clone(),
            container_handles: container_handles.clone(),
            proxy_cmd_tx: ctx.proxy_cmd_tx.clone(),
            task_manager: crate::task_manager::new_shared(),
        };
        tokio::spawn(async move {
            crate::control_server::run_control_server(
                control_listener,
                ctrl_ctx,
                activity,
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
                        ctx.last_activity.store(current_time_secs(), Ordering::Relaxed);
                        let ctx = ctx.clone();
                        let handles = container_handles.clone();

                        tokio::spawn(async move {
                            if let Err(e) = handle_management_connection(stream, &ctx, handles).await
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
async fn handle_management_connection(
    stream: tokio::net::UnixStream,
    ctx: &ManagementContext,
    container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
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

        let response = handle_management_request(req, ctx, &container_handles).await;

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
async fn handle_management_request(
    req: ManagementRequest,
    ctx: &ManagementContext,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
) -> ManagementResponse {
    match req {
        ManagementRequest::RegisterContainer {
            container_id,
            container_name,
            container_ip,
            ports_attributes,
            other_ports_attributes,
            forward_ports,
            shutdown_action: _,
        } => {
            let reg = ContainerRegistration {
                container_id,
                container_name,
                container_ip,
                ports_attributes,
                other_ports_attributes,
                forward_ports,
            };
            handle_register(reg, &ctx.port_manager, container_handles).await
        }
        ManagementRequest::DeregisterContainer { container_name } => {
            handle_deregister(&container_name, &ctx.port_manager, container_handles).await
        }
        ManagementRequest::QueryPorts => handle_query_ports(&ctx.port_manager).await,
        ManagementRequest::QueryStatus => {
            handle_query_status(
                &ctx.port_manager,
                container_handles,
                ctx.start_time,
                ctx.is_orbstack,
                ctx.daemon_started_at,
                &ctx.auth_token,
                ctx.control_port,
            )
            .await
        }
        ManagementRequest::Ping => ManagementResponse::Pong,
        ManagementRequest::Shutdown => {
            let pid = std::process::id();
            info!("Shutdown requested, sending signal");
            let _ = ctx.shutdown_tx.send(true);
            ManagementResponse::ShuttingDown { pid }
        }
    }
}

/// Data for a container registration request.
struct ContainerRegistration {
    container_id: String,
    container_name: String,
    container_ip: Option<String>,
    ports_attributes: Vec<PortAttributes>,
    other_ports_attributes: Option<PortAttributes>,
    forward_ports: Vec<u16>,
}

/// Handle container registration.
async fn handle_register(
    reg: ContainerRegistration,
    port_manager: &Arc<Mutex<PortManager>>,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
) -> ManagementResponse {
    // Register with port manager
    {
        let mut pm = port_manager.lock().await;
        pm.register_container(
            &reg.container_id,
            &reg.container_name,
            reg.container_ip,
            reg.ports_attributes,
            reg.other_ports_attributes,
        );

        // Pre-allocate host ports for forwardPorts
        for &fwd_port in &reg.forward_ports {
            pm.handle_port_open(
                &reg.container_id,
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
            reg.container_name.clone(),
            ContainerHandle {
                container_id: reg.container_id,
                agent_state: Arc::new(AgentConnectionState::new()),
            },
        );
    }

    info!("Registered container {}", reg.container_name);

    ManagementResponse::ContainerRegistered {
        container_name: reg.container_name,
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

    fn test_management_context(
        ctrl_port: u16,
    ) -> (ManagementContext, tokio::sync::watch::Receiver<bool>) {
        let (stx, srx) = tokio::sync::watch::channel(false);
        let ctx = ManagementContext {
            last_activity: Arc::new(AtomicU64::new(0)),
            port_manager: Arc::new(Mutex::new(PortManager::new(false))),
            browser_handler: Arc::new(BrowserHandler::new()),
            proxy_cmd_tx: mpsc::channel(16).0,
            start_time: std::time::Instant::now(),
            is_orbstack: false,
            daemon_started_at: 0,
            shutdown_tx: stx,
            auth_token: "test-token".to_string(),
            control_port: ctrl_port,
        };
        (ctx, srx)
    }

    #[tokio::test]
    async fn management_ping_pong() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmt.sock");

        // Create a TCP listener for the control server
        let ctrl_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ctrl_port = ctrl_listener.local_addr().unwrap().port();

        let sock = socket_path.clone();
        let server_handle = tokio::spawn({
            let (ctx, srx) = test_management_context(ctrl_port);
            async move {
                let _ = run_management_server(&sock, ctx, srx, ctrl_listener).await;
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

        let ctrl_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ctrl_port = ctrl_listener.local_addr().unwrap().port();

        let sock = socket_path.clone();
        let server_handle = tokio::spawn({
            let (ctx, srx) = test_management_context(ctrl_port);
            async move {
                let _ = run_management_server(&sock, ctx, srx, ctrl_listener).await;
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
                shutdown_action: None,
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
