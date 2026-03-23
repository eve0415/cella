//! Reconnecting wrapper around [`ControlClient`] that retries initial connection
//! and transparently reconnects on TCP drops.

use std::time::Duration;

use cella_port::CellaPortError;
use cella_port::protocol::{AgentMessage, DaemonMessage};
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
                Ok(client) => {
                    info!("Connected to daemon at {addr}");
                    return Self {
                        addr: addr.to_string(),
                        container_name: container_name.to_string(),
                        auth_token: auth_token.to_string(),
                        inner: Some(client),
                        reconnected: false,
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
                };
            }

            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }

    /// Returns `true` if the underlying connection is currently established.
    pub const fn is_connected(&self) -> bool {
        self.inner.is_some()
    }

    /// Returns and clears the `reconnected` flag.
    ///
    /// When a reconnection succeeds the flag is set to `true`. Callers should
    /// poll this to decide whether cached state (e.g. the set of known
    /// listeners) needs to be re-reported to the daemon.
    pub fn take_reconnected(&mut self) -> bool {
        std::mem::take(&mut self.reconnected)
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
    async fn try_reconnect(&mut self) -> Result<(), CellaPortError> {
        match ControlClient::connect(&self.addr, &self.container_name, &self.auth_token).await {
            Ok(client) => {
                info!("Reconnected to daemon at {}", self.addr);
                self.inner = Some(client);
                self.reconnected = true;
                Ok(())
            }
            Err(e) => {
                warn!("Reconnect failed: {e}");
                Err(CellaPortError::ControlSocket {
                    message: format!("reconnect to {} failed: {e}", self.addr),
                })
            }
        }
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
        };

        let msg = AgentMessage::Health {
            uptime_secs: 0,
            ports_detected: 0,
        };
        let result = client.send(&msg).await;
        assert!(result.is_err());
    }
}
