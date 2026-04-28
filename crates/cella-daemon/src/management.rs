//! Management server: handles CLI requests on `~/.cella/daemon.sock`.
//!
//! Accepts `ManagementRequest` messages and manages container registration,
//! port/status queries, and the unified TCP control server.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cella_protocol::{
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
    pub ssh_proxy_manager: crate::ssh_proxy::SharedSshProxyManager,
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
            cella_bin: crate::control_server::resolve_cella_binary(),
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
        ManagementRequest::RegisterContainer(data) => {
            let reg = ContainerRegistration {
                container_id: data.container_id,
                container_name: data.container_name,
                container_ip: data.container_ip,
                ports_attributes: data.ports_attributes,
                other_ports_attributes: data.other_ports_attributes,
                forward_ports: data.forward_ports,
                backend_kind: data.backend_kind,
                docker_host: data.docker_host,
            };
            handle_register(reg, &ctx.port_manager, container_handles, &ctx.proxy_cmd_tx).await
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
        ManagementRequest::UpdateContainerIp {
            container_id,
            container_ip,
        } => {
            let found = ctx
                .port_manager
                .lock()
                .await
                .update_container_ip(&container_id, container_ip);
            if found {
                info!("Updated container IP for {container_id}");
                ManagementResponse::ContainerIpUpdated { container_id }
            } else {
                warn!("UpdateContainerIp: unknown container {container_id}");
                ManagementResponse::Error {
                    message: format!("unknown container: {container_id}"),
                }
            }
        }
        ManagementRequest::Ping => ManagementResponse::Pong,
        ManagementRequest::RegisterSshAgentProxy {
            workspace,
            upstream_socket,
        } => {
            handle_register_ssh_agent_proxy(&workspace, &upstream_socket, &ctx.ssh_proxy_manager)
                .await
        }
        ManagementRequest::ReleaseSshAgentProxy { workspace } => {
            handle_release_ssh_agent_proxy(&workspace, &ctx.ssh_proxy_manager).await
        }
        ManagementRequest::Shutdown => {
            let pid = std::process::id();
            info!("Shutdown requested, sending signal");
            let _ = ctx.shutdown_tx.send(true);
            ManagementResponse::ShuttingDown { pid }
        }
    }
}

async fn handle_register_ssh_agent_proxy(
    workspace: &str,
    upstream_socket: &str,
    manager: &crate::ssh_proxy::SharedSshProxyManager,
) -> ManagementResponse {
    let workspace_path = std::path::PathBuf::from(workspace);
    let upstream_path = std::path::PathBuf::from(upstream_socket);
    let result: Result<(u16, usize), CellaDaemonError> = {
        let mut mgr = manager.lock().await;
        match mgr.register(workspace_path.clone(), upstream_path).await {
            Ok(port) => Ok((port, mgr.refcount_for(&workspace_path))),
            Err(e) => Err(e),
        }
    };
    match result {
        Ok((bridge_port, refcount)) => {
            info!(
                "Registered ssh-agent bridge for {workspace} (refcount={refcount}, port={bridge_port})"
            );
            ManagementResponse::SshAgentProxyRegistered {
                bridge_port,
                refcount,
            }
        }
        Err(e) => {
            warn!("ssh-agent bridge register failed for {workspace}: {e}");
            ManagementResponse::Error {
                message: format!("ssh-agent bridge register failed: {e}"),
            }
        }
    }
}

async fn handle_release_ssh_agent_proxy(
    workspace: &str,
    manager: &crate::ssh_proxy::SharedSshProxyManager,
) -> ManagementResponse {
    let workspace_path = std::path::PathBuf::from(workspace);
    let torn_down = manager.lock().await.release(&workspace_path);
    if torn_down {
        info!("Released ssh-agent bridge for {workspace} (torn down)");
    }
    ManagementResponse::SshAgentProxyReleased { torn_down }
}

/// Data for a container registration request.
struct ContainerRegistration {
    container_id: String,
    container_name: String,
    container_ip: Option<String>,
    ports_attributes: Vec<PortAttributes>,
    other_ports_attributes: Option<PortAttributes>,
    forward_ports: Vec<u16>,
    backend_kind: Option<String>,
    docker_host: Option<String>,
}

/// Handle container registration.
async fn handle_register(
    reg: ContainerRegistration,
    port_manager: &Arc<Mutex<PortManager>>,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
    proxy_cmd_tx: &mpsc::Sender<ProxyCommand>,
) -> ManagementResponse {
    // Register with port manager
    {
        let mut pm = port_manager.lock().await;
        let released = pm.register_container(
            &reg.container_id,
            &reg.container_name,
            reg.container_ip,
            reg.ports_attributes,
            reg.other_ports_attributes,
        );

        // Stop coordinator-owned proxies for ports released by re-registration.
        for hp in released {
            let _ = proxy_cmd_tx
                .send(ProxyCommand::Stop { host_port: hp })
                .await;
        }

        // Pre-allocate host ports for forwardPorts
        for &fwd_port in &reg.forward_ports {
            pm.handle_port_open(
                &reg.container_id,
                fwd_port,
                cella_protocol::PortProtocol::Tcp,
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
                backend_kind: reg.backend_kind,
                docker_host: reg.docker_host,
                agent_tx: None,
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
                agent_version: handle
                    .agent_state
                    .agent_version
                    .lock()
                    .ok()
                    .and_then(|v| v.clone()),
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
        let tmp = tempfile::tempdir().unwrap();
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
            ssh_proxy_manager: crate::ssh_proxy::new_shared(tmp.keep(), "test-token".to_string()),
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
            &ManagementRequest::RegisterContainer(Box::new(
                cella_protocol::ContainerRegistrationData {
                    container_id: "abc123".to_string(),
                    container_name: "test-container".to_string(),
                    container_ip: Some("172.20.0.5".to_string()),
                    ports_attributes: vec![],
                    other_ports_attributes: None,
                    forward_ports: vec![],
                    shutdown_action: None,
                    backend_kind: Some("docker".to_string()),
                    docker_host: None,
                },
            )),
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

    /// Stand up an echo server on `path` to mimic an upstream ssh-agent.
    fn spawn_echo_upstream(path: &Path) -> tokio::task::JoinHandle<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixListener;
        let listener = UnixListener::bind(path).expect("bind upstream");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        if stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        })
    }

    /// Spawn a management server bound to `socket_path`, returning a join
    /// handle to abort. Blocks until the socket file appears so the caller
    /// can immediately send requests.
    async fn spawn_management_server(socket_path: &Path) -> tokio::task::JoinHandle<()> {
        let ctrl_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ctrl_port = ctrl_listener.local_addr().unwrap().port();
        let sock = socket_path.to_path_buf();
        let handle = tokio::spawn({
            let (ctx, srx) = test_management_context(ctrl_port);
            async move {
                let _ = run_management_server(&sock, ctx, srx, ctrl_listener).await;
            }
        });
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        handle
    }

    #[tokio::test]
    async fn ssh_agent_proxy_register_returns_socket_with_refcount_1() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmt.sock");
        let upstream_path = dir.path().join("upstream.sock");
        let _upstream = spawn_echo_upstream(&upstream_path);
        let server = spawn_management_server(&socket_path).await;

        let resp = send_management_request(
            &socket_path,
            &ManagementRequest::RegisterSshAgentProxy {
                workspace: "/Users/me/proj".to_string(),
                upstream_socket: upstream_path.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();

        match resp {
            ManagementResponse::SshAgentProxyRegistered {
                bridge_port,
                refcount,
            } => {
                assert_eq!(refcount, 1);
                // Returned port must accept TCP connections from the
                // would-be in-container agent.
                let _client = tokio::net::TcpStream::connect(("127.0.0.1", bridge_port))
                    .await
                    .unwrap();
            }
            other => panic!("expected SshAgentProxyRegistered, got {other:?}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn ssh_agent_proxy_second_register_reuses_port_and_bumps_refcount() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmt.sock");
        let upstream_path = dir.path().join("upstream.sock");
        let _upstream = spawn_echo_upstream(&upstream_path);
        let server = spawn_management_server(&socket_path).await;

        let workspace = "/Users/me/proj".to_string();
        let upstream = upstream_path.to_string_lossy().into_owned();

        let req = ManagementRequest::RegisterSshAgentProxy {
            workspace: workspace.clone(),
            upstream_socket: upstream,
        };
        let first = send_management_request(&socket_path, &req).await.unwrap();
        let second = send_management_request(&socket_path, &req).await.unwrap();

        let (p1, _) = match first {
            ManagementResponse::SshAgentProxyRegistered {
                bridge_port,
                refcount,
            } => (bridge_port, refcount),
            other => panic!("expected SshAgentProxyRegistered, got {other:?}"),
        };
        match second {
            ManagementResponse::SshAgentProxyRegistered {
                bridge_port: p2,
                refcount,
            } => {
                assert_eq!(refcount, 2);
                assert_eq!(p2, p1);
            }
            other => panic!("expected SshAgentProxyRegistered, got {other:?}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn ssh_agent_proxy_tcp_bridge_forwards_bytes_to_upstream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmt.sock");
        let upstream_path = dir.path().join("upstream.sock");
        let _upstream = spawn_echo_upstream(&upstream_path);
        let server = spawn_management_server(&socket_path).await;

        let resp = send_management_request(
            &socket_path,
            &ManagementRequest::RegisterSshAgentProxy {
                workspace: "/Users/me/proj".to_string(),
                upstream_socket: upstream_path.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();
        let bridge_port = match resp {
            ManagementResponse::SshAgentProxyRegistered { bridge_port, .. } => bridge_port,
            other => panic!("expected SshAgentProxyRegistered, got {other:?}"),
        };

        // The TCP bridge requires an auth-token handshake on the first
        // line — `test_management_context` uses "test-token".
        let mut client = TcpStream::connect(("127.0.0.1", bridge_port))
            .await
            .unwrap();
        client.write_all(b"test-token\n").await.unwrap();
        client.write_all(b"hi").await.unwrap();
        let mut buf = [0u8; 2];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");

        server.abort();
    }

    #[tokio::test]
    async fn ssh_agent_proxy_release_decrements_then_tears_down() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmt.sock");
        let upstream_path = dir.path().join("upstream.sock");
        let _upstream = spawn_echo_upstream(&upstream_path);
        let server = spawn_management_server(&socket_path).await;

        let workspace = "/Users/me/proj".to_string();
        let upstream = upstream_path.to_string_lossy().into_owned();
        let register = ManagementRequest::RegisterSshAgentProxy {
            workspace: workspace.clone(),
            upstream_socket: upstream,
        };
        let release = ManagementRequest::ReleaseSshAgentProxy {
            workspace: workspace.clone(),
        };

        send_management_request(&socket_path, &register)
            .await
            .unwrap();
        send_management_request(&socket_path, &register)
            .await
            .unwrap();

        let resp = send_management_request(&socket_path, &release)
            .await
            .unwrap();
        assert!(matches!(
            resp,
            ManagementResponse::SshAgentProxyReleased { torn_down: false }
        ));
        let resp = send_management_request(&socket_path, &release)
            .await
            .unwrap();
        assert!(matches!(
            resp,
            ManagementResponse::SshAgentProxyReleased { torn_down: true }
        ));

        server.abort();
    }

    #[tokio::test]
    async fn ssh_agent_proxy_release_unknown_workspace_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("mgmt.sock");
        let server = spawn_management_server(&socket_path).await;

        let resp = send_management_request(
            &socket_path,
            &ManagementRequest::ReleaseSshAgentProxy {
                workspace: "/never/registered".to_string(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            resp,
            ManagementResponse::SshAgentProxyReleased { torn_down: false }
        ));

        server.abort();
    }

    #[cfg(feature = "integration-tests")]
    #[tokio::test]
    async fn query_status_reports_connected_after_bare_handshake() {
        use tokio::io::AsyncWriteExt;

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

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        send_management_request(
            &socket_path,
            &ManagementRequest::RegisterContainer(Box::new(
                cella_protocol::ContainerRegistrationData {
                    container_id: "hs-container-id".to_string(),
                    container_name: "hs-container".to_string(),
                    container_ip: Some("172.20.0.5".to_string()),
                    ports_attributes: vec![],
                    other_ports_attributes: None,
                    forward_ports: vec![],
                    shutdown_action: None,
                    backend_kind: Some("docker".to_string()),
                    docker_host: None,
                },
            )),
        )
        .await
        .unwrap();

        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", ctrl_port))
            .await
            .unwrap();
        let hello = cella_protocol::AgentHello {
            protocol_version: cella_protocol::PROTOCOL_VERSION,
            agent_version: "0.0.28".to_string(),
            container_name: "hs-container".to_string(),
            auth_token: "test-token".to_string(),
        };
        let mut json = serde_json::to_string(&hello).unwrap();
        json.push('\n');
        stream.write_all(json.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let start = std::time::Instant::now();
        let mut observed = None;
        while start.elapsed() < std::time::Duration::from_secs(1) {
            let resp = send_management_request(&socket_path, &ManagementRequest::QueryStatus)
                .await
                .unwrap();
            if let ManagementResponse::Status { containers, .. } = resp
                && let Some(c) = containers
                    .into_iter()
                    .find(|c| c.container_name == "hs-container")
                && c.agent_connected
            {
                observed = Some(c);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let summary =
            observed.expect("QueryStatus should show agent_connected=true within 1s of handshake");
        assert!(summary.agent_connected);
        assert_eq!(summary.agent_version.as_deref(), Some("0.0.28"));

        drop(stream);
        server_handle.abort();
    }
}
