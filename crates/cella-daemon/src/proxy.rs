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

/// Handle a proxy start command: bind the proxy and report the result.
async fn handle_proxy_start(
    host_port: u16,
    container_ip: String,
    container_port: u16,
    result_tx: Option<tokio::sync::oneshot::Sender<Result<(), io::Error>>>,
    proxies: &mut HashMap<u16, ProxyHandle>,
) {
    let target = ProxyTarget::DirectIp {
        ip: container_ip,
        port: container_port,
    };
    match start_proxy(host_port, target).await {
        Ok(handle) => {
            debug!("Started proxy: localhost:{host_port} -> container:{container_port}");
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
                handle_proxy_start(
                    host_port,
                    container_ip,
                    container_port,
                    result_tx,
                    &mut proxies,
                )
                .await;
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
    async fn proxy_starts_and_stops() {
        let target = ProxyTarget::DirectIp {
            ip: "127.0.0.1".to_string(),
            port: 9999,
        };
        let handle = start_proxy(0, target).await.unwrap();
        handle.abort();
    }

    // -- ProxyTarget --

    #[test]
    fn proxy_target_debug_contains_ip_and_port() {
        let target = ProxyTarget::DirectIp {
            ip: "10.0.0.5".into(),
            port: 8080,
        };
        let dbg = format!("{target:?}");
        assert!(dbg.contains("10.0.0.5"));
        assert!(dbg.contains("8080"));
    }

    #[test]
    fn proxy_target_clone() {
        let original = ProxyTarget::DirectIp {
            ip: "1.2.3.4".into(),
            port: 443,
        };
        // Use a function boundary to prevent clippy redundant_clone.
        let cloned = clone_target(&original);
        let ProxyTarget::DirectIp { ip, port } = &cloned;
        assert_eq!(ip, "1.2.3.4");
        assert_eq!(*port, 443);
    }

    fn clone_target(t: &ProxyTarget) -> ProxyTarget {
        t.clone()
    }

    // -- ProxyCommand construction --

    #[test]
    fn proxy_command_start_fields() {
        let cmd = ProxyCommand::Start {
            host_port: 3000,
            container_ip: "172.17.0.2".into(),
            container_port: 8080,
            result_tx: None,
        };
        match cmd {
            ProxyCommand::Start {
                host_port,
                container_ip,
                container_port,
                ..
            } => {
                assert_eq!(host_port, 3000);
                assert_eq!(container_ip, "172.17.0.2");
                assert_eq!(container_port, 8080);
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
        let target = ProxyTarget::DirectIp {
            ip: "127.0.0.1".into(),
            port: 1,
        };
        let handle = start_proxy(0, target).await.unwrap();
        // Aborting should not panic.
        handle.abort();
    }

    // -- start_proxy binding --

    #[tokio::test]
    async fn start_proxy_port_zero_binds_random() {
        let target = ProxyTarget::DirectIp {
            ip: "127.0.0.1".into(),
            port: 1234,
        };
        let handle = start_proxy(0, target).await.unwrap();
        handle.abort();
    }

    // -- run_proxy_coordinator --

    #[tokio::test]
    async fn coordinator_stop_unknown_port_is_harmless() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let coordinator = tokio::spawn(run_proxy_coordinator(rx));

        tx.send(ProxyCommand::Stop { host_port: 9999 })
            .await
            .unwrap();
        // Drop sender to shut down coordinator.
        drop(tx);
        coordinator.await.unwrap();
    }

    #[tokio::test]
    async fn coordinator_start_and_stop_lifecycle() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let coordinator = tokio::spawn(run_proxy_coordinator(rx));

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        tx.send(ProxyCommand::Start {
            host_port: 0, // random port
            container_ip: "127.0.0.1".into(),
            container_port: 1,
            result_tx: Some(result_tx),
        })
        .await
        .unwrap();

        // Should succeed.
        result_rx.await.unwrap().unwrap();

        drop(tx);
        coordinator.await.unwrap();
    }
}
