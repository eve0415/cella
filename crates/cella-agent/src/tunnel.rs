//! Reverse tunnel handler: responds to daemon `TunnelRequest` messages by opening
//! a TCP connection back to the daemon and relaying to a local service.

use cella_protocol::TunnelHandshake;
use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::TcpStream;
use tracing::{debug, warn};

/// Configuration for creating reverse tunnels back to the daemon.
#[derive(Clone)]
pub struct TunnelConfig {
    pub daemon_addr: String,
    pub auth_token: String,
}

/// Handle a single `TunnelRequest` by connecting back to the daemon and relaying
/// to the local service.
///
/// `target_host` overrides the hostname to connect to inside the container.
/// `None` uses `localhost` (the existing numeric-port path — byte-identical
/// behaviour).
pub async fn handle_tunnel_request(
    connection_id: u64,
    target_port: u16,
    target_host: Option<String>,
    config: &TunnelConfig,
) {
    let host = target_host.as_deref().unwrap_or("localhost");
    debug!("Tunnel request: connection_id={connection_id} target={host}:{target_port}");

    let tunnel_result = TcpStream::connect(&config.daemon_addr).await;
    let mut tunnel = match tunnel_result {
        Ok(s) => s,
        Err(e) => {
            warn!("Tunnel connect to daemon failed: {e}");
            return;
        }
    };

    let hs = TunnelHandshake {
        auth_token: config.auth_token.clone(),
        connection_id,
    };
    let mut json = match serde_json::to_string(&hs) {
        Ok(j) => j,
        Err(e) => {
            warn!("Tunnel handshake serialize failed: {e}");
            return;
        }
    };
    json.push('\n');
    if let Err(e) = tunnel.write_all(json.as_bytes()).await {
        warn!("Tunnel handshake write failed: {e}");
        return;
    }
    if let Err(e) = tunnel.flush().await {
        warn!("Tunnel handshake flush failed: {e}");
        return;
    }

    let local_result = TcpStream::connect((host, target_port)).await;
    let mut local = match local_result {
        Ok(s) => s,
        Err(e) => {
            warn!("Tunnel local connect to {host}:{target_port} failed: {e}");
            return;
        }
    };

    debug!("Tunnel {connection_id}: connected to {host}:{target_port}");
    let _ = copy_bidirectional(&mut tunnel, &mut local).await;
    debug!("Tunnel connection {connection_id} closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clone_config(c: &TunnelConfig) -> TunnelConfig {
        c.clone()
    }

    #[test]
    fn tunnel_config_clone() {
        let config = TunnelConfig {
            daemon_addr: "127.0.0.1:5000".to_string(),
            auth_token: "secret".to_string(),
        };
        let cloned = clone_config(&config);
        assert_eq!(cloned.daemon_addr, "127.0.0.1:5000");
        assert_eq!(cloned.auth_token, "secret");
    }

    #[tokio::test]
    async fn localhost_connects_to_ipv6_only_listener() {
        use tokio::net::TcpListener;
        let Ok(listener) = TcpListener::bind(("::1", 0)).await else {
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let connect = TcpStream::connect(("localhost", port)).await;
        assert!(connect.is_ok(), "localhost must resolve to ::1");
    }

    #[tokio::test]
    async fn handle_tunnel_request_fails_gracefully_on_unreachable_daemon() {
        let config = TunnelConfig {
            daemon_addr: "127.0.0.1:1".to_string(),
            auth_token: "token".to_string(),
        };
        // Should not panic — just logs a warning.
        handle_tunnel_request(1, 3000, None, &config).await;
    }

    #[tokio::test]
    async fn handle_tunnel_request_with_target_host_fails_gracefully() {
        let config = TunnelConfig {
            daemon_addr: "127.0.0.1:1".to_string(),
            auth_token: "token".to_string(),
        };
        // target_host path should also fail gracefully on unreachable daemon.
        handle_tunnel_request(2, 5432, Some("db".to_string()), &config).await;
    }
}
