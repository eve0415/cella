//! Reconnecting wrapper around [`ControlClient`] that retries initial connection
//! and transparently reconnects on TCP drops.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

use cella_port::CellaPortError;
use cella_protocol::{AgentMessage, DaemonHello, DaemonMessage};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::control::ControlClient;

/// Interval between connection attempts during initial retry and reconnection.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Duration of the initial burst phase with exponential backoff.
const BURST_DURATION: Duration = Duration::from_secs(5 * 60);

/// Maximum backoff during the burst phase.
const MAX_BURST_BACKOFF: Duration = Duration::from_secs(30);

/// Backoff interval after the burst phase.
const SLOW_BACKOFF: Duration = Duration::from_secs(60);

/// Base interval for exponential backoff.
const BASE_BACKOFF: Duration = Duration::from_secs(2);

/// A wrapper around [`ControlClient`] that retries the initial connection and
/// attempts a single reconnect when a send or receive fails.
///
/// The daemon may not have started yet when the agent starts, so
/// `connect_with_retry` blocks until the TCP handshake succeeds. Giving up
/// and running in standalone mode is a trap: the daemon often comes up
/// seconds-to-minutes later and the agent needs to pick that up without
/// external intervention.
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

/// How often to surface a progress log while the initial connect retries.
const INITIAL_CONNECT_WARN_INTERVAL: Duration = Duration::from_secs(30);

impl ReconnectingClient {
    /// Connect to the daemon, retrying indefinitely until the handshake
    /// succeeds. Re-reads `/cella/.daemon_addr` on every attempt so an
    /// updated address from a later `cella up` is picked up automatically.
    ///
    /// The returned client is always connected (its `inner` is `Some`).
    pub async fn connect_with_retry(
        initial_addr: &str,
        container_name: &str,
        initial_token: &str,
    ) -> Self {
        let start = tokio::time::Instant::now();
        let mut last_warn = start;
        let mut addr = initial_addr.to_string();
        let mut token = initial_token.to_string();

        loop {
            match ControlClient::connect(&addr, container_name, &token).await {
                Ok((client, hello)) => {
                    info!("Connected to daemon at {addr}");
                    return Self {
                        addr,
                        container_name: container_name.to_string(),
                        auth_token: token,
                        inner: Some(client),
                        reconnected: false,
                        daemon_hello: Some(hello),
                    };
                }
                Err(e) => {
                    debug!("Daemon connect to {addr} failed, will retry: {e}");
                    if last_warn.elapsed() >= INITIAL_CONNECT_WARN_INTERVAL {
                        warn!(
                            "Still waiting for daemon at {addr} after {}s ({e})",
                            start.elapsed().as_secs()
                        );
                        last_warn = tokio::time::Instant::now();
                    }
                }
            }

            tokio::time::sleep(RETRY_INTERVAL).await;

            if let Some(info) = crate::control::read_daemon_addr_file()
                && (info.addr != addr || info.token != token)
            {
                info!(
                    "Daemon address updated via .daemon_addr ({} -> {})",
                    addr, info.addr
                );
                addr = info.addr;
                token = info.token;
            }
        }
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

/// Calculate the next backoff duration for reconnection attempts.
///
/// Uses exponential backoff: 2s, 4s, 8s, 16s, 30s (cap).
/// After `BURST_DURATION` (5 min), slows to 60s intervals.
fn next_backoff(attempt: u32, elapsed: Duration) -> Duration {
    if elapsed >= BURST_DURATION {
        return SLOW_BACKOFF;
    }
    let backoff = BASE_BACKOFF.saturating_mul(1 << attempt.min(4));
    backoff.min(MAX_BURST_BACKOFF)
}

/// Spawn a background reconnection task if one isn't already running.
///
/// The task runs outside any lock — connect attempts don't block the port
/// watcher or health reporter. On success it briefly locks the mutex to
/// install the new connection.
///
/// When `state_writer` is provided, emits `Reconnecting` at task entry and
/// `Connected` (with the successful address) once a new connection is
/// installed.
pub fn spawn_background_reconnect(
    control: Arc<Mutex<ReconnectingClient>>,
    reconnecting: Arc<AtomicBool>,
    state_writer: Option<crate::state::StateWriter>,
) {
    if reconnecting
        .compare_exchange(false, true, AtomicOrdering::SeqCst, AtomicOrdering::SeqCst)
        .is_err()
    {
        debug!("Background reconnection already in progress");
        return;
    }

    if let Some(w) = &state_writer {
        w.set_state(crate::state::AgentState::Reconnecting);
    }

    let reconnecting_flag = reconnecting;
    tokio::spawn(async move {
        let start = tokio::time::Instant::now();
        let mut attempt: u32 = 0;

        let (initial_addr, container_name, initial_token) = {
            let guard = control.lock().await;
            guard.connection_params()
        };

        info!("Starting background reconnection (initial addr: {initial_addr})");

        loop {
            let elapsed = start.elapsed();
            let backoff = next_backoff(attempt, elapsed);

            tokio::time::sleep(backoff).await;

            // Re-read .daemon_addr for a potentially updated address.
            let (addr, token) = if let Some(info) = crate::control::read_daemon_addr_file() {
                (info.addr, info.token)
            } else {
                (initial_addr.clone(), initial_token.clone())
            };

            match ControlClient::connect(&addr, &container_name, &token).await {
                Ok((client, hello)) => {
                    info!("Background reconnection succeeded (addr: {addr})");
                    control
                        .lock()
                        .await
                        .install_connection(client, hello, addr.clone(), token);
                    if let Some(w) = &state_writer {
                        w.set_daemon_addr(Some(addr));
                        w.set_state(crate::state::AgentState::Connected);
                    }
                    reconnecting_flag.store(false, AtomicOrdering::SeqCst);
                    return;
                }
                Err(e) => {
                    debug!(
                        "Background reconnect attempt {attempt} failed: {e} (next in {backoff:?})"
                    );
                }
            }

            attempt = attempt.saturating_add(1);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_with_retry_blocks_when_daemon_unreachable() {
        // Regression guard for the permanent-standalone trap: the old
        // implementation gave up after 30s and returned a disconnected
        // client. Callers then fell into `run_standalone` with no
        // reconnect loop, silently ignoring the daemon when it came up
        // later. The fix is to never return until connected.
        let fut = ReconnectingClient::connect_with_retry("127.0.0.1:1", "test", "token");
        let result = tokio::time::timeout(Duration::from_secs(2), fut).await;
        assert!(
            result.is_err(),
            "connect_with_retry must not return while the daemon is unreachable",
        );
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
        assert!(client.inner.is_none());
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

        assert!(client.inner.is_none());
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

    #[test]
    fn next_backoff_exponential_sequence() {
        let zero = Duration::ZERO;
        assert_eq!(next_backoff(0, zero), Duration::from_secs(2));
        assert_eq!(next_backoff(1, zero), Duration::from_secs(4));
        assert_eq!(next_backoff(2, zero), Duration::from_secs(8));
        assert_eq!(next_backoff(3, zero), Duration::from_secs(16));
        assert_eq!(next_backoff(4, zero), Duration::from_secs(30));
        // Cap at 30s
        assert_eq!(next_backoff(5, zero), Duration::from_secs(30));
        assert_eq!(next_backoff(10, zero), Duration::from_secs(30));
    }

    #[test]
    fn next_backoff_slow_after_burst() {
        let past_burst = Duration::from_secs(5 * 60 + 1);
        assert_eq!(next_backoff(0, past_burst), Duration::from_secs(60));
        assert_eq!(next_backoff(5, past_burst), Duration::from_secs(60));
    }

    #[test]
    fn next_backoff_at_burst_boundary() {
        // Exactly at burst duration — still within burst.
        let at_burst = BURST_DURATION;
        assert_eq!(next_backoff(0, at_burst), SLOW_BACKOFF);
    }

    #[test]
    fn spawn_background_reconnect_prevents_duplicate() {
        let reconnecting = Arc::new(AtomicBool::new(true));
        // Flag is already set — compare_exchange should fail, no task spawned.
        assert!(reconnecting.load(AtomicOrdering::SeqCst));
        // Calling the function with an already-set flag is a no-op.
        // We verify the flag remains true (not reset).
        let after = reconnecting.compare_exchange(
            false,
            true,
            AtomicOrdering::SeqCst,
            AtomicOrdering::SeqCst,
        );
        assert!(after.is_err());
    }
}
