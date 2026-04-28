//! TCP proxy implementation for forwarding host ports to container ports.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use cella_protocol::DaemonMessage;
use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::control_server::ContainerHandle;
use crate::tunnel::TunnelBroker;

/// Commands for the proxy coordinator.
pub enum ProxyCommand {
    /// Start a new TCP proxy.
    Start {
        host_port: u16,
        target: ProxyStartTarget,
        /// Receives `Ok(())` if proxy bound successfully, or the bind error.
        result_tx: Option<tokio::sync::oneshot::Sender<Result<(), io::Error>>>,
    },
    /// Stop a running TCP proxy.
    Stop { host_port: u16 },
}

/// Target specification for starting a proxy.
#[derive(Debug, Clone)]
pub enum ProxyStartTarget {
    /// Connect directly to an IP:port (OrbStack, Linux native).
    DirectIp { ip: String, port: u16 },
    /// Tunnel through the agent via a reverse TCP connection.
    AgentTunnel { container_name: String, port: u16 },
}

/// Handle to a running TCP proxy task.
pub struct ProxyHandle {
    handle: tokio::task::JoinHandle<()>,
}

impl ProxyHandle {
    /// Abort the proxy task.
    pub fn abort(self) {
        self.handle.abort();
    }
}

/// Shared state needed by the proxy coordinator for tunnel mode.
pub struct ProxyCoordinatorContext {
    pub tunnel_broker: Arc<TunnelBroker>,
    pub container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
}

/// Start a direct-IP TCP proxy from `host_port` to the given IP:port.
async fn start_direct_proxy(
    host_port: u16,
    ip: String,
    port: u16,
) -> Result<ProxyHandle, io::Error> {
    let listener = TcpListener::bind(("127.0.0.1", host_port)).await?;
    debug!("Direct proxy listening on 127.0.0.1:{host_port} -> {ip}:{port}");

    let handle = tokio::spawn(async move {
        loop {
            let (mut inbound, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Proxy accept error on port {host_port}: {e}");
                    continue;
                }
            };

            let ip = ip.clone();
            debug!("Proxy connection from {peer} on port {host_port}");

            tokio::spawn(async move {
                match TcpStream::connect((ip.as_str(), port)).await {
                    Ok(mut outbound) => {
                        let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                    Err(e) => {
                        warn!("Proxy connect to {ip}:{port} failed: {e}");
                        let _ = inbound.shutdown().await;
                    }
                }
            });
        }
    });

    Ok(ProxyHandle { handle })
}

/// Start an agent-tunnel TCP proxy from `host_port` through the agent.
async fn start_tunnel_proxy(
    host_port: u16,
    container_name: String,
    target_port: u16,
    broker: Arc<TunnelBroker>,
    container_handles: Arc<Mutex<HashMap<String, ContainerHandle>>>,
) -> Result<ProxyHandle, io::Error> {
    let listener = TcpListener::bind(("127.0.0.1", host_port)).await?;
    debug!(
        "Tunnel proxy listening on 127.0.0.1:{host_port} -> agent:{container_name}:{target_port}"
    );

    let handle = tokio::spawn(async move {
        loop {
            let (mut inbound, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Tunnel proxy accept error on port {host_port}: {e}");
                    continue;
                }
            };

            let broker = broker.clone();
            let handles = container_handles.clone();
            let name = container_name.clone();
            debug!("Tunnel proxy connection from {peer} on port {host_port}");

            tokio::spawn(async move {
                if let Err(e) = handle_tunnel_proxy_connection(
                    &mut inbound,
                    &name,
                    target_port,
                    &broker,
                    &handles,
                )
                .await
                {
                    debug!("Tunnel proxy connection failed: {e}");
                    let _ = inbound.shutdown().await;
                }
            });
        }
    });

    Ok(ProxyHandle { handle })
}

async fn handle_tunnel_proxy_connection(
    inbound: &mut TcpStream,
    container_name: &str,
    target_port: u16,
    broker: &TunnelBroker,
    container_handles: &Arc<Mutex<HashMap<String, ContainerHandle>>>,
) -> Result<(), io::Error> {
    let (connection_id, rx) = broker.request_tunnel().await;

    let agent_tx = {
        let handles = container_handles.lock().await;
        handles.get(container_name).and_then(|h| h.agent_tx.clone())
    };

    let Some(agent_tx) = agent_tx else {
        broker.cancel(connection_id).await;
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "agent not connected",
        ));
    };

    if agent_tx
        .send(DaemonMessage::TunnelRequest {
            connection_id,
            target_port,
        })
        .await
        .is_err()
    {
        broker.cancel(connection_id).await;
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "agent channel closed",
        ));
    }

    let tunnel_stream = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;

    let mut tunnel = match tunnel_stream {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => {
            broker.cancel(connection_id).await;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "tunnel delivery failed",
            ));
        }
        Err(_) => {
            broker.cancel(connection_id).await;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "tunnel request timed out",
            ));
        }
    };

    let _ = copy_bidirectional(inbound, &mut tunnel).await;
    Ok(())
}

/// Run the proxy coordinator that manages TCP proxy lifecycle.
///
/// Receives `ProxyCommand` messages and starts/stops TCP proxies accordingly.
pub async fn run_proxy_coordinator(
    mut rx: tokio::sync::mpsc::Receiver<ProxyCommand>,
    ctx: Option<ProxyCoordinatorContext>,
) {
    let mut proxies: HashMap<u16, ProxyHandle> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            ProxyCommand::Start {
                host_port,
                target,
                result_tx,
            } => {
                let result = match target {
                    ProxyStartTarget::DirectIp { ip, port } => {
                        start_direct_proxy(host_port, ip, port).await
                    }
                    ProxyStartTarget::AgentTunnel {
                        container_name,
                        port,
                    } => {
                        if let Some(ref ctx) = ctx {
                            start_tunnel_proxy(
                                host_port,
                                container_name,
                                port,
                                ctx.tunnel_broker.clone(),
                                ctx.container_handles.clone(),
                            )
                            .await
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "tunnel proxy not available",
                            ))
                        }
                    }
                };
                match result {
                    Ok(handle) => {
                        debug!("Started proxy on localhost:{host_port}");
                        proxies.insert(host_port, handle);
                        if let Some(tx) = result_tx {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Err(e) => {
                        warn!("Failed to start proxy on port {host_port}: {e}");
                        if let Some(tx) = result_tx {
                            let _ = tx.send(Err(e));
                        }
                    }
                }
            }
            ProxyCommand::Stop { host_port } => {
                if let Some(handle) = proxies.remove(&host_port) {
                    handle.abort();
                    debug!("Stopped proxy on port {host_port}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_proxy_starts_and_stops() {
        let handle = start_direct_proxy(0, "127.0.0.1".to_string(), 9999)
            .await
            .unwrap();
        handle.abort();
    }

    // -- ProxyStartTarget --

    #[test]
    fn proxy_start_target_debug_contains_ip_and_port() {
        let target = ProxyStartTarget::DirectIp {
            ip: "10.0.0.5".into(),
            port: 8080,
        };
        let dbg = format!("{target:?}");
        assert!(dbg.contains("10.0.0.5"));
        assert!(dbg.contains("8080"));
    }

    #[test]
    fn proxy_start_target_clone() {
        let original = ProxyStartTarget::DirectIp {
            ip: "1.2.3.4".into(),
            port: 443,
        };
        let cloned = clone_target(&original);
        assert!(matches!(
            cloned,
            ProxyStartTarget::DirectIp { ref ip, port: 443 } if ip == "1.2.3.4"
        ));
    }

    fn clone_target(t: &ProxyStartTarget) -> ProxyStartTarget {
        t.clone()
    }

    #[test]
    fn proxy_start_target_agent_tunnel_debug() {
        let target = ProxyStartTarget::AgentTunnel {
            container_name: "cella-test".into(),
            port: 3000,
        };
        let dbg = format!("{target:?}");
        assert!(dbg.contains("cella-test"));
        assert!(dbg.contains("3000"));
    }

    // -- ProxyCommand construction --

    #[test]
    fn proxy_command_start_fields() {
        let cmd = ProxyCommand::Start {
            host_port: 3000,
            target: ProxyStartTarget::DirectIp {
                ip: "172.17.0.2".into(),
                port: 8080,
            },
            result_tx: None,
        };
        match cmd {
            ProxyCommand::Start {
                host_port, target, ..
            } => {
                assert_eq!(host_port, 3000);
                assert!(matches!(
                    target,
                    ProxyStartTarget::DirectIp { ref ip, port: 8080 } if ip == "172.17.0.2"
                ));
            }
            ProxyCommand::Stop { .. } => panic!("expected Start"),
        }
    }

    #[test]
    fn proxy_command_stop_fields() {
        let cmd = ProxyCommand::Stop { host_port: 5000 };
        match cmd {
            ProxyCommand::Stop { host_port } => assert_eq!(host_port, 5000),
            ProxyCommand::Start { .. } => panic!("expected Stop"),
        }
    }

    // -- ProxyHandle::abort --

    #[tokio::test]
    async fn proxy_handle_abort_is_idempotent() {
        let handle = start_direct_proxy(0, "127.0.0.1".into(), 1).await.unwrap();
        handle.abort();
    }

    // -- start_direct_proxy binding --

    #[tokio::test]
    async fn start_direct_proxy_port_zero_binds_random() {
        let handle = start_direct_proxy(0, "127.0.0.1".into(), 1234)
            .await
            .unwrap();
        handle.abort();
    }

    // -- run_proxy_coordinator --

    #[tokio::test]
    async fn coordinator_stop_unknown_port_is_harmless() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let coordinator = tokio::spawn(run_proxy_coordinator(rx, None));

        tx.send(ProxyCommand::Stop { host_port: 9999 })
            .await
            .unwrap();
        drop(tx);
        coordinator.await.unwrap();
    }

    #[tokio::test]
    async fn coordinator_start_and_stop_lifecycle() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let coordinator = tokio::spawn(run_proxy_coordinator(rx, None));

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tx.send(ProxyCommand::Start {
            host_port: 0,
            target: ProxyStartTarget::DirectIp {
                ip: "127.0.0.1".into(),
                port: 1,
            },
            result_tx: Some(result_tx),
        })
        .await
        .unwrap();

        result_rx.await.unwrap().unwrap();

        drop(tx);
        coordinator.await.unwrap();
    }
}
