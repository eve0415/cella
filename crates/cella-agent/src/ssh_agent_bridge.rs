//! In-container SSH-agent Unix-socket → daemon TCP bridge.
//!
//! Listens on a Unix socket at `target_socket` inside the container's own
//! filesystem (typically `/run/host-services/ssh-auth.sock`). For each
//! accepted connection, opens a TCP connection to `host_endpoint`
//! (usually `host.docker.internal:<bridge_port>`), writes the auth token
//! handshake, and bidirectionally copies bytes until either side closes.
//!
//! The daemon side of the bridge — on the macOS host — then opens a
//! fresh `UnixStream::connect($SSH_AUTH_SOCK)` per accepted TCP
//! connection and copies bytes through. Net effect: consumers inside
//! the container see a normal `SSH_AUTH_SOCK` Unix socket, the bytes
//! flow over loopback TCP to the host, and the host's real agent (e.g.
//! 1Password) signs on behalf of the container.
//!
//! ## Why this exists
//!
//! Earlier attempts bind-mounted a host-created Unix socket into the
//! container. On colima, virtiofs rejects `mkdir` for any host-side
//! socket path, and Docker's mkdir-source-if-missing fallback kills
//! the mount at container-create time. The Unix socket has to live
//! inside the container's own filesystem. TCP carries the bytes back
//! to the host without touching the virtiofs mount layer.

use std::io;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tracing::{debug, warn};

/// Run the bridge forever. On entry:
/// - Creates the parent directory of `target_socket` if needed.
/// - Removes any stale socket file at `target_socket`.
/// - Binds a `UnixListener` and loops accepting connections.
/// - Sets the socket mode to 0o666 so any user in the container can
///   connect (ssh-add, git, etc. may run as the remote user, not root).
///
/// Each accepted connection is handed to a spawned task that opens a
/// TCP connection to `host_endpoint`, sends `auth_token` + `\n`, then
/// runs `tokio::io::copy_bidirectional` until either side closes.
///
/// # Errors
///
/// Returns `Err` only for fatal startup errors (bind failure, the only
/// unrecoverable path). Runtime accept errors are logged and the loop
/// continues. Per-connection errors (TCP connect, auth write, copy)
/// are logged at `debug` inside the spawned task.
pub async fn run_bridge(
    target_socket: PathBuf,
    host_endpoint: String,
    auth_token: String,
) -> io::Result<()> {
    if let Some(parent) = target_socket.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        warn!(
            "ssh-agent bridge: create_dir_all {} failed: {e}",
            parent.display()
        );
    }

    // Remove any stale socket file (daemon restart, previous container,
    // etc.). Ignore "does not exist" — that's the normal fresh-start case.
    let _ = std::fs::remove_file(&target_socket);

    let listener = UnixListener::bind(&target_socket).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "ssh-agent bridge: bind {} failed: {e}",
                target_socket.display()
            ),
        )
    })?;

    set_socket_mode_0o666(&target_socket);

    debug!(
        target = %target_socket.display(),
        host_endpoint = %host_endpoint,
        "ssh-agent bridge: listening"
    );

    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                let endpoint = host_endpoint.clone();
                let token = auth_token.clone();
                tokio::spawn(async move {
                    bridge_one(client, endpoint, token).await;
                });
            }
            Err(e) => {
                warn!("ssh-agent bridge: accept failed: {e}");
                // Transient accept errors shouldn't kill the loop —
                // kernel momentarily runs out of fds, etc.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Bridge one accepted container client to a fresh TCP connection to
/// the host daemon. Sends the auth token handshake, then bidirectionally
/// copies bytes until either side closes. Errors are logged at `debug`
/// (these are routine — ssh-add closes after one query, etc.) and the
/// task exits cleanly.
async fn bridge_one(mut client: UnixStream, host_endpoint: String, auth_token: String) {
    let mut upstream = match TcpStream::connect(&host_endpoint).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                host_endpoint = %host_endpoint,
                "ssh-agent bridge: TCP connect to daemon failed: {e}"
            );
            return;
        }
    };

    // Auth handshake: single line, just the token plus a newline.
    // Daemon rejects the connection if the first line doesn't match.
    if let Err(e) = upstream.write_all(auth_token.as_bytes()).await {
        debug!("ssh-agent bridge: token write failed: {e}");
        return;
    }
    if let Err(e) = upstream.write_all(b"\n").await {
        debug!("ssh-agent bridge: newline write failed: {e}");
        return;
    }
    if let Err(e) = upstream.flush().await {
        debug!("ssh-agent bridge: flush after token failed: {e}");
        return;
    }

    if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        debug!("ssh-agent bridge: connection ended with: {e}");
    }
}

#[cfg(unix)]
fn set_socket_mode_0o666(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o666);
    if let Err(e) = std::fs::set_permissions(path, perms) {
        warn!(
            "ssh-agent bridge: set_permissions {} failed: {e}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn set_socket_mode_0o666(_path: &Path) {}

/// Parse `CELLA_SSH_AGENT_BRIDGE` and `CELLA_SSH_AGENT_TARGET` + auth
/// token from env. Returns `Some((target_socket, host_endpoint,
/// auth_token))` when all three are set, `None` otherwise.
///
/// The auth token comes from `CELLA_DAEMON_TOKEN` (the daemon uses the
/// same token for the control channel and the ssh-agent bridge).
#[must_use]
pub fn config_from_env() -> Option<(PathBuf, String, String)> {
    let host_endpoint = std::env::var("CELLA_SSH_AGENT_BRIDGE").ok()?;
    let target = std::env::var("CELLA_SSH_AGENT_TARGET").ok()?;
    let token = std::env::var("CELLA_DAEMON_TOKEN").ok()?;
    if host_endpoint.is_empty() || target.is_empty() || token.is_empty() {
        return None;
    }
    Some((PathBuf::from(target), host_endpoint, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::net::TcpListener;

    /// Spawn a mock daemon TCP listener that expects the auth handshake
    /// on the first line, then echoes bytes back. Returns the bound
    /// host:port and a handle to abort.
    async fn spawn_mock_daemon_tcp(expected_token: &str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let expected = expected_token.to_string();
        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let expected = expected.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut reader = BufReader::new(read);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.is_err() {
                        return;
                    }
                    if line.trim() != expected {
                        let _ = write.shutdown().await;
                        return;
                    }
                    // After auth, echo bytes until the client closes.
                    let mut inner = reader.into_inner();
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = inner.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        if write.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (format!("127.0.0.1:{port}"), handle)
    }

    #[tokio::test]
    async fn bridge_round_trips_bytes_after_auth_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("agent.sock");
        let (endpoint, daemon_task) = spawn_mock_daemon_tcp("the-token").await;

        let target_for_task = target.clone();
        let bridge_task = tokio::spawn(async move {
            let _ = run_bridge(target_for_task, endpoint, "the-token".to_string()).await;
        });
        // Wait for the socket to appear.
        for _ in 0..20 {
            if target.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let mut client = UnixStream::connect(&target).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        bridge_task.abort();
        daemon_task.abort();
    }

    #[tokio::test]
    async fn bridge_connection_closes_when_daemon_rejects_auth() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("agent.sock");
        // Daemon expects "good-token", bridge sends "bad-token" → daemon
        // shuts immediately, container client should see EOF instead of
        // a working bridge.
        let (endpoint, daemon_task) = spawn_mock_daemon_tcp("good-token").await;

        let target_for_task = target.clone();
        let bridge_task = tokio::spawn(async move {
            let _ = run_bridge(target_for_task, endpoint, "bad-token".to_string()).await;
        });
        for _ in 0..20 {
            if target.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let mut client = UnixStream::connect(&target).await.unwrap();
        // Write some bytes to force a round-trip to the daemon.
        let _ = client.write_all(b"ping").await;
        let _ = client.shutdown().await;
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("bridge must EOF within 2s after daemon rejects auth")
            .unwrap_or(0);
        assert_eq!(n, 0, "expected EOF, got {n} bytes");

        bridge_task.abort();
        daemon_task.abort();
    }

    #[tokio::test]
    async fn bridge_replaces_stale_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("agent.sock");
        std::fs::write(&target, b"junk").unwrap();
        assert!(target.exists());

        let (endpoint, daemon_task) = spawn_mock_daemon_tcp("tk").await;

        let target_for_task = target.clone();
        let bridge_task = tokio::spawn(async move {
            let _ = run_bridge(target_for_task, endpoint, "tk".to_string()).await;
        });
        for _ in 0..20 {
            if UnixStream::connect(&target).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // After bridge startup, the target is a listening socket (not
        // the stale junk file) — connecting must succeed.
        let _conn = UnixStream::connect(&target).await.expect("connect");

        bridge_task.abort();
        daemon_task.abort();
    }

    #[test]
    fn config_from_env_requires_all_three_vars() {
        // Test with all env vars unset (standard test environment).
        let a = std::env::var("CELLA_SSH_AGENT_BRIDGE").ok();
        let b = std::env::var("CELLA_SSH_AGENT_TARGET").ok();
        let c = std::env::var("CELLA_DAEMON_TOKEN").ok();
        if a.is_none() || b.is_none() || c.is_none() {
            assert!(config_from_env().is_none());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bridge_sets_socket_mode_0o666() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("agent.sock");
        let (endpoint, daemon_task) = spawn_mock_daemon_tcp("tk").await;

        let target_for_task = target.clone();
        let bridge_task = tokio::spawn(async move {
            let _ = run_bridge(target_for_task, endpoint, "tk".to_string()).await;
        });
        for _ in 0..20 {
            if target.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o666,
            "socket must be world-readable/writable so non-root container users can connect"
        );

        bridge_task.abort();
        daemon_task.abort();
    }
}
