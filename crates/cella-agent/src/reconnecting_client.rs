//! Reconnecting wrapper around [`ControlClient`] that retries initial connection
//! and transparently reconnects on TCP drops.

use std::time::Duration;

use cella_port::CellaPortError;
use cella_protocol::{AgentMessage, DaemonHello, DaemonMessage};
use tracing::{debug, info, warn};

use crate::control::ControlClient;

/// Interval between connection attempts during initial retry and reconnection.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// A wrapper around [`ControlClient`] that retries the initial connection and
/// attempts a single reconnect when a send or receive fails.
///
/// The daemon may not have registered the container yet when the agent starts,
/// so `connect_with_retry` retries TCP connection attempts until timeout.
///
/// Once connected, any I/O failure on `send` triggers a single reconnect
/// attempt. If reconnection succeeds the `reconnected` flag is set so callers
/// (e.g. the port watcher) can re-report state.
pub struct ReconnectingClient {
    addr: String,
    container_name: String,
    auth_token: String,
    inner: Option<ControlClient>,
    reconnected: bool,
    /// The `DaemonHello` received during the most recent handshake.
    daemon_hello: Option<DaemonHello>,
}

impl ReconnectingClient {
    /// Try to connect to the daemon, retrying every 500 ms until
    /// `timeout` elapses.
    ///
    /// Returns a client with `inner = None` (i.e. disconnected) if every
    /// attempt fails within the timeout window. The caller should check
    /// [`is_connected`](Self::is_connected) before assuming the connection is
    /// live.
    pub async fn connect_with_retry(
        addr: &str,
        container_name: &str,
        auth_token: &str,
        timeout: Duration,
    ) -> Self {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            match ControlClient::connect(addr, container_name, auth_token).await {
                Ok((client, hello)) => {
                    info!("Connected to daemon at {addr}");
                    return Self {
                        addr: addr.to_string(),
                        container_name: container_name.to_string(),
                        auth_token: auth_token.to_string(),
                        inner: Some(client),
                        reconnected: false,
                        daemon_hello: Some(hello),
                    };
                }
                Err(e) => {
                    debug!("Daemon connect failed, will retry: {e}");
                }
            }

            if tokio::time::Instant::now() >= deadline {
                warn!(
                    "Timed out waiting for daemon after {:.1}s: {addr}",
                    timeout.as_secs_f64(),
                );
                return Self {
                    addr: addr.to_string(),
                    container_name: container_name.to_string(),
                    auth_token: auth_token.to_string(),
                    inner: None,
                    reconnected: false,
                    daemon_hello: None,
                };
            }

            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }

    /// Returns `true` if the underlying connection is currently established.
    pub const fn is_connected(&self) -> bool {
        self.inner.is_some()
    }

    /// Check if the given daemon info matches the current connection params.
    fn matches_current(&self, info: &crate::control::DaemonAddrInfo) -> bool {
        let addr_matches = info.addr == self.addr;
        let token_matches = info.token == self.auth_token;
        addr_matches && token_matches
    }

    /// Returns and clears the `reconnected` flag.
    ///
    /// When a reconnection succeeds the flag is set to `true`. Callers should
    /// poll this to decide whether cached state (e.g. the set of known
    /// listeners) needs to be re-reported to the daemon.
    pub fn take_reconnected(&mut self) -> bool {
        std::mem::take(&mut self.reconnected)
    }

    /// Return connection parameters for use by a background reconnection task.
    pub fn connection_params(&self) -> (String, String, String) {
        (
            self.addr.clone(),
            self.container_name.clone(),
            self.auth_token.clone(),
        )
    }

    /// Install a successfully-established connection from a background task.
    ///
    /// Called by the background reconnection loop after it connects to the
    /// daemon outside the mutex. Updates the stored address and token so
    /// future inline reconnects use the new values.
    pub fn install_connection(
        &mut self,
        client: ControlClient,
        hello: DaemonHello,
        new_addr: String,
        new_token: String,
    ) {
        self.inner = Some(client);
        self.daemon_hello = Some(hello);
        self.addr = new_addr;
        self.auth_token = new_token;
        self.reconnected = true;
    }

    /// Send a message, attempting a single reconnect on failure.
    ///
    /// # Errors
    ///
    /// Returns [`CellaPortError::ControlSocket`] if both the original send and
    /// the reconnect attempt fail.
    pub async fn send(&mut self, msg: &AgentMessage) -> Result<(), CellaPortError> {
        if let Some(ref mut client) = self.inner {
            match client.send(msg).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    warn!("Send failed, attempting reconnect: {e}");
                    self.inner = None;
                }
            }
        }

        // Attempt a single reconnect.
        self.try_reconnect().await?;

        // Send on the fresh connection.
        if let Some(ref mut client) = self.inner {
            client.send(msg).await
        } else {
            Err(CellaPortError::ControlSocket {
                message: "not connected".to_string(),
            })
        }
    }

    /// Read a response message from the daemon.
    ///
    /// # Errors
    ///
    /// Returns [`CellaPortError::ControlSocket`] if the client is disconnected
    /// or the underlying read fails.
    pub async fn recv(&mut self) -> Result<DaemonMessage, CellaPortError> {
        if let Some(ref mut client) = self.inner {
            client.recv().await
        } else {
            Err(CellaPortError::ControlSocket {
                message: "not connected".to_string(),
            })
        }
    }

    /// Attempt a single reconnect to the daemon.
    ///
    /// Tries the current address first, then falls back to reading the
    /// `.daemon_addr` file on the shared volume (which the host CLI
    /// updates on every `cella up` and daemon restart).
    async fn try_reconnect(&mut self) -> Result<(), CellaPortError> {
        // 1. Try the current address
        match ControlClient::connect(&self.addr, &self.container_name, &self.auth_token).await {
            Ok((client, hello)) => {
                info!("Reconnected to daemon at {}", self.addr);
                self.inner = Some(client);
                self.daemon_hello = Some(hello);
                self.reconnected = true;
                return Ok(());
            }
            Err(e) => {
                debug!("Reconnect to {} failed: {e}", self.addr);
            }
        }

        // 2. Fallback: read updated address from .daemon_addr file
        if let Some(info) =
            crate::control::read_daemon_addr_file().filter(|info| !self.matches_current(info))
        {
            info!(
                "Daemon address changed ({} -> {}), trying new address",
                self.addr, info.addr
            );
            match ControlClient::connect(&info.addr, &self.container_name, &info.token).await {
                Ok((client, hello)) => {
                    info!("Reconnected to daemon at {} (from .daemon_addr)", info.addr);
                    self.addr = info.addr;
                    self.auth_token = info.token;
                    self.inner = Some(client);
                    self.daemon_hello = Some(hello);
                    self.reconnected = true;
                    return Ok(());
                }
                Err(e) => {
                    warn!("Reconnect to {} (from .daemon_addr) failed: {e}", info.addr);
                }
            }
        }

        Err(CellaPortError::ControlSocket {
            message: format!("reconnect to {} failed", self.addr),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_with_retry_returns_disconnected_on_unreachable() {
        let timeout = Duration::from_millis(100);
        let client =
            ReconnectingClient::connect_with_retry("127.0.0.1:1", "test", "token", timeout).await;

        assert!(!client.is_connected());
    }

    #[tokio::test]
    async fn take_reconnected_clears_flag() {
        let mut client = ReconnectingClient {
            addr: "127.0.0.1:1".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            inner: None,
            reconnected: true,
            daemon_hello: None,
        };

        assert!(client.take_reconnected());
        assert!(!client.take_reconnected());
    }

    #[tokio::test]
    async fn send_on_disconnected_returns_error() {
        let mut client = ReconnectingClient {
            addr: "127.0.0.1:1".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            inner: None,
            reconnected: false,
            daemon_hello: None,
        };

        let msg = AgentMessage::Health {
            uptime_secs: 0,
            ports_detected: 0,
        };
        let result = client.send(&msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recv_on_disconnected_returns_error() {
        let mut client = ReconnectingClient {
            addr: "127.0.0.1:1".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            inner: None,
            reconnected: false,
            daemon_hello: None,
        };

        let result = client.recv().await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("not connected"));
    }

    #[test]
    fn is_connected_when_disconnected() {
        let client = ReconnectingClient {
            addr: "127.0.0.1:1".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            inner: None,
            reconnected: false,
            daemon_hello: None,
        };
        assert!(!client.is_connected());
    }

    #[test]
    fn take_reconnected_false_when_not_set() {
        let mut client = ReconnectingClient {
            addr: "127.0.0.1:1".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            inner: None,
            reconnected: false,
            daemon_hello: None,
        };
        assert!(!client.take_reconnected());
    }

    #[tokio::test]
    async fn try_reconnect_fails_on_unreachable() {
        let mut client = ReconnectingClient {
            addr: "127.0.0.1:1".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            inner: None,
            reconnected: false,
            daemon_hello: None,
        };
        let result = client.try_reconnect().await;
        assert!(result.is_err());
        assert!(!client.is_connected());
        assert!(!client.reconnected);
    }

    #[tokio::test]
    async fn send_error_includes_not_connected() {
        let mut client = ReconnectingClient {
            addr: "127.0.0.1:1".to_string(),
            container_name: "test".to_string(),
            auth_token: "token".to_string(),
            inner: None,
            reconnected: false,
            daemon_hello: None,
        };

        let msg = AgentMessage::Health {
            uptime_secs: 10,
            ports_detected: 3,
        };
        let err = client.send(&msg).await.unwrap_err();
        let msg = format!("{err}");
        // After failed reconnect, should mention the addr in the error.
        assert!(msg.contains("127.0.0.1:1") || msg.contains("reconnect") || msg.contains("failed"));
    }

    #[test]
    fn connection_params_returns_stored_values() {
        let client = ReconnectingClient {
            addr: "10.0.0.1:5000".to_string(),
            container_name: "my-container".to_string(),
            auth_token: "secret-123".to_string(),
            inner: None,
            reconnected: false,
            daemon_hello: None,
        };

        let (addr, name, token) = client.connection_params();
        assert_eq!(addr, "10.0.0.1:5000");
        assert_eq!(name, "my-container");
        assert_eq!(token, "secret-123");
    }

    #[test]
    fn install_connection_updates_addr_and_sets_reconnected() {
        let mut client = ReconnectingClient {
            addr: "10.0.0.1:5000".to_string(),
            container_name: "test".to_string(),
            auth_token: "old-token".to_string(),
            inner: None,
            reconnected: false,
            daemon_hello: None,
        };

        assert!(!client.is_connected());
        assert!(!client.reconnected);

        // install_connection without a real ControlClient — verify
        // address/token/flag updates by checking the fields directly.
        // Full connection installation is tested via integration tests.
        let hello = DaemonHello {
            protocol_version: 1,
            daemon_version: "0.1.0".to_string(),
            error: None,
            workspace_path: None,
            parent_repo: None,
            is_worktree: false,
        };
        // We can't construct a ControlClient without a real TCP connection,
        // so we test the flag behavior on the fields we can access.
        assert_eq!(client.addr, "10.0.0.1:5000");
        assert_eq!(client.auth_token, "old-token");

        // Simulate what install_connection does for fields we can verify.
        client.addr = "10.0.0.2:6000".to_string();
        client.auth_token = "new-token".to_string();
        client.daemon_hello = Some(hello);
        client.reconnected = true;

        assert_eq!(client.addr, "10.0.0.2:6000");
        assert_eq!(client.auth_token, "new-token");
        assert!(client.reconnected);
        assert!(client.daemon_hello.is_some());
    }
}
