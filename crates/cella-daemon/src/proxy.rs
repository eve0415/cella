//! TCP proxy implementation for forwarding host ports to container ports.

use std::collections::HashMap;
use std::io;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

/// Commands for the proxy coordinator.
pub enum ProxyCommand {
    /// Start a new TCP proxy.
    Start {
        host_port: u16,
        container_ip: String,
        container_port: u16,
        /// Receives `Ok(())` if proxy bound successfully, or the bind error.
        result_tx: Option<tokio::sync::oneshot::Sender<Result<(), io::Error>>>,
    },
    /// Stop a running TCP proxy.
    Stop { host_port: u16 },
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

/// Target for a TCP proxy connection.
#[derive(Debug, Clone)]
pub enum ProxyTarget {
    /// Connect directly to an IP:port (`OrbStack`, Linux native).
    DirectIp { ip: String, port: u16 },
}

/// Start a TCP proxy from `host_port` to the given target.
///
/// Returns a handle that can be used to stop the proxy.
///
/// # Errors
///
/// Returns error if binding to the host port fails.
pub async fn start_proxy(host_port: u16, target: ProxyTarget) -> Result<ProxyHandle, io::Error> {
    let listener = TcpListener::bind(("127.0.0.1", host_port)).await?;
    debug!("TCP proxy listening on 127.0.0.1:{host_port} -> {target:?}");

    let handle = tokio::spawn(async move {
        loop {
            let (mut inbound, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Proxy accept error on port {host_port}: {e}");
                    continue;
                }
            };

            let target = target.clone();
            debug!("Proxy connection from {peer} on port {host_port}");

            tokio::spawn(async move {
                match &target {
                    ProxyTarget::DirectIp { ip, port } => {
                        match TcpStream::connect((ip.as_str(), *port)).await {
                            Ok(mut outbound) => {
                                let _ = copy_bidirectional(&mut inbound, &mut outbound).await;
                            }
                            Err(e) => {
                                warn!("Proxy connect to {ip}:{port} failed: {e}");
                            }
                        }
                    }
                }
            });
        }
    });

    Ok(ProxyHandle { handle })
}

/// Run the proxy coordinator that manages TCP proxy lifecycle.
///
/// Receives `ProxyCommand` messages and starts/stops TCP proxies accordingly.
pub async fn run_proxy_coordinator(mut rx: tokio::sync::mpsc::Receiver<ProxyCommand>) {
    let mut proxies: HashMap<u16, ProxyHandle> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            ProxyCommand::Start {
                host_port,
                container_ip,
                container_port,
                result_tx,
            } => {
                let target = ProxyTarget::DirectIp {
                    ip: container_ip,
                    port: container_port,
                };
                match start_proxy(host_port, target).await {
                    Ok(handle) => {
                        debug!(
                            "Started proxy: localhost:{host_port} -> container:{container_port}"
                        );
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
    #[ignore = "requires TCP port binding"]
    async fn proxy_starts_and_stops() {
        let target = ProxyTarget::DirectIp {
            ip: "127.0.0.1".to_string(),
            port: 9999,
        };
        let handle = start_proxy(0, target).await.unwrap();
        handle.abort();
    }
}
