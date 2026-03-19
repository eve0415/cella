//! Control socket listener and handler.
//!
//! Accepts text commands over `~/.cella/tunnel.sock` for managing tunnels:
//! - `connect <container_id>` — start tunnel to container
//! - `disconnect <container_id>` — stop tunnel
//! - `status` — list all tunnels
//! - `ping` — health check

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{info, warn};

use crate::CellaTunnelError;
use crate::daemon::current_time_secs;
use crate::tunnel::TunnelManager;

/// Start the control socket server.
///
/// Listens for text commands, dispatches to the tunnel manager.
///
/// # Errors
///
/// Returns error if socket binding fails.
pub async fn run_control_server(
    socket_path: &Path,
    manager: Arc<TunnelManager>,
    last_activity: Arc<AtomicU64>,
) -> Result<(), CellaTunnelError> {
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path).map_err(|e| CellaTunnelError::Socket {
        message: format!("failed to bind {}: {e}", socket_path.display()),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(socket_path, perms).map_err(|e| CellaTunnelError::Socket {
            message: format!("failed to set socket permissions: {e}"),
        })?;
    }

    info!(
        "Tunnel control socket listening on {}",
        socket_path.display()
    );

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                last_activity.store(current_time_secs(), Ordering::Relaxed);
                let mgr = Arc::clone(&manager);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, mgr).await {
                        warn!("Control connection error: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("Accept error: {e}");
            }
        }
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    manager: Arc<TunnelManager>,
) -> Result<(), CellaTunnelError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| CellaTunnelError::Socket {
            message: format!("read error: {e}"),
        })?;

    let line = line.trim();
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();

    let response = match cmd {
        "ping" => "pong\n".to_string(),
        "connect" => {
            if arg.is_empty() {
                "error missing container_id\n".to_string()
            } else {
                match manager.connect(arg).await {
                    Ok(()) => "ok\n".to_string(),
                    Err(e) => format!("error {e}\n"),
                }
            }
        }
        "disconnect" => {
            if arg.is_empty() {
                "error missing container_id\n".to_string()
            } else {
                manager.disconnect(arg);
                "ok\n".to_string()
            }
        }
        "status" => {
            use std::fmt::Write;
            let statuses = manager.status();
            let mut s = String::new();
            for (id, status) in &statuses {
                let _ = writeln!(s, "{id}: {status}");
            }
            s.push('\n');
            s
        }
        _ => format!("error unknown command: {cmd}\n"),
    };

    writer
        .write_all(response.as_bytes())
        .await
        .map_err(|e| CellaTunnelError::Socket {
            message: format!("write error: {e}"),
        })?;

    Ok(())
}
