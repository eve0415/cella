//! Unix socket listener and connection handler.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

use crate::CellaCredentialProxyError;
use crate::host::invoke_git_credential;
use crate::protocol::{CredentialResponse, format_response, parse_request};

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

    // Set socket permissions to 0o600 (owner only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(socket_path, perms).map_err(|e| {
            CellaCredentialProxyError::Socket {
                message: format!("failed to set socket permissions: {e}"),
            }
        })?;
    }

    info!("Credential proxy listening on {}", socket_path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                last_activity.store(current_time_secs(), Ordering::Relaxed);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream).await {
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

/// Handle a single client connection.
async fn handle_connection(mut stream: UnixStream) -> Result<(), CellaCredentialProxyError> {
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
            let output = format_response(&response);
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

/// Get the current time in seconds since the Unix epoch.
pub fn current_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
