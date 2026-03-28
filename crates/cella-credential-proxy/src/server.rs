//! Socket listener and connection handler (Unix + TCP).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tracing::{debug, info, warn};

use cella_daemon::shared::{current_time_secs, set_socket_permissions};

use crate::CellaCredentialProxyError;
use crate::host::invoke_git_credential;
use crate::protocol::{CredentialResponse, format_credential_fields, parse_request};

/// Start the credential proxy server on a Unix socket.
///
/// Listens for connections, handles each in a separate task,
/// and tracks the last activity time for idle timeout.
///
/// # Errors
///
/// Returns error if socket binding fails.
pub async fn run_server(
    socket_path: &Path,
    last_activity: Arc<AtomicU64>,
) -> Result<(), CellaCredentialProxyError> {
    // Clean up stale socket
    let _ = std::fs::remove_file(socket_path);

    let listener =
        UnixListener::bind(socket_path).map_err(|e| CellaCredentialProxyError::Socket {
            message: format!("failed to bind {}: {e}", socket_path.display()),
        })?;

    set_socket_permissions(socket_path);

    info!("Credential proxy listening on {}", socket_path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                last_activity.store(current_time_secs(), Ordering::Relaxed);
                tokio::spawn(async move {
                    if let Err(e) = handle_stream(stream).await {
                        warn!("Connection handler error: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("Accept error: {e}");
            }
        }
    }
}

/// Bind a TCP listener, attempting to reclaim a previously used port.
async fn bind_tcp_listener(port_path: &Path) -> Result<TcpListener, CellaCredentialProxyError> {
    let preferred_port = std::fs::read_to_string(port_path)
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(0);

    cella_daemon::shared::bind_tcp_reclaim(preferred_port)
        .await
        .map_err(|e| CellaCredentialProxyError::Socket {
            message: format!("failed to bind TCP: {e}"),
        })
}

/// Write the TCP port file so clients can discover the port.
fn write_port_file(port_path: &Path, port: u16) -> Result<(), CellaCredentialProxyError> {
    if let Some(parent) = port_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(port_path, port.to_string()).map_err(|e| CellaCredentialProxyError::PidFile {
        message: format!("failed to write port file: {e}"),
    })
}

/// Start the credential proxy server on a TCP socket bound to localhost.
///
/// Binds to `127.0.0.1:0` (OS-assigned port) and writes the allocated port
/// to `port_path` for clients to discover.
///
/// # Errors
///
/// Returns error if binding or port file writing fails.
pub async fn run_tcp_server(
    port_path: &Path,
    last_activity: Arc<AtomicU64>,
) -> Result<(), CellaCredentialProxyError> {
    let listener = bind_tcp_listener(port_path).await?;

    let port = listener
        .local_addr()
        .map_err(|e| CellaCredentialProxyError::Socket {
            message: format!("failed to get local addr: {e}"),
        })?
        .port();

    write_port_file(port_path, port)?;

    info!("Credential proxy TCP listening on 127.0.0.1:{port}");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                last_activity.store(current_time_secs(), Ordering::Relaxed);
                debug!("TCP connection from {peer}");
                tokio::spawn(async move {
                    if let Err(e) = handle_stream(stream).await {
                        warn!("TCP connection handler error: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("TCP accept error: {e}");
            }
        }
    }
}

/// Handle a single client connection on any async stream.
async fn handle_stream(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
) -> Result<(), CellaCredentialProxyError> {
    let mut buf = vec![0u8; 8192];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| CellaCredentialProxyError::Socket {
            message: format!("read error: {e}"),
        })?;

    if n == 0 {
        return Ok(());
    }

    let data = String::from_utf8_lossy(&buf[..n]);
    let request = parse_request(&data)?;

    debug!("Credential request: op={}", request.operation);

    // Handle ping for health checks
    if request.operation == "ping" {
        stream
            .write_all(b"pong\n")
            .await
            .map_err(|e| CellaCredentialProxyError::Socket {
                message: format!("write error: {e}"),
            })?;
        return Ok(());
    }

    // Invoke host git credential helper (blocking operation in spawn_blocking)
    let operation = request.operation.clone();
    let fields = request.fields.clone();

    let result = tokio::task::spawn_blocking(move || invoke_git_credential(&operation, &fields))
        .await
        .map_err(|e| CellaCredentialProxyError::GitCredential {
            message: format!("task join error: {e}"),
        })?;

    match result {
        Ok(response_fields) => {
            let response = CredentialResponse {
                fields: response_fields,
            };
            let output = format_credential_fields(&response.fields);
            stream.write_all(output.as_bytes()).await.map_err(|e| {
                CellaCredentialProxyError::Socket {
                    message: format!("write error: {e}"),
                }
            })?;
        }
        Err(e) => {
            warn!("git credential error: {e}");
            // Send empty response on error (git will prompt for credentials)
            stream
                .write_all(b"\n")
                .await
                .map_err(|e| CellaCredentialProxyError::Socket {
                    message: format!("write error: {e}"),
                })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    #[tokio::test]
    async fn tcp_server_reuses_port() {
        let dir = tempfile::tempdir().unwrap();
        let port_path = dir.path().join("test.port");
        let activity = Arc::new(AtomicU64::new(current_time_secs()));

        // Write a known port to the port file
        let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
        let listener = TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // Free the port
        std::fs::write(&port_path, port.to_string()).unwrap();

        // Start TCP server — should reuse the same port
        let activity_clone = activity.clone();
        let port_path_clone = port_path.clone();
        let handle =
            tokio::spawn(async move { run_tcp_server(&port_path_clone, activity_clone).await });

        // Give server time to bind
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify port file still has the same port
        let written_port: u16 = std::fs::read_to_string(&port_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(written_port, port);

        handle.abort();
    }
}
