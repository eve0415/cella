//! Per-container tunnel management.
//!
//! Manages the docker exec lifecycle, multiplexed frame dispatch,
//! heartbeat monitoring, and automatic reconnection with exponential backoff.

use std::collections::HashMap;
use std::fmt;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::BufReader;
use tokio::process::ChildStdin;
use tokio::sync::{Mutex as TokioMutex, mpsc};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{debug, error, info, warn};

use crate::CellaTunnelError;
use crate::mux::{
    ChannelKind, Frame, FrameType, heartbeat_ack_frame, heartbeat_frame, read_frame_async,
    write_frame_async,
};

/// Type alias for the exec stdin writer shared across tasks.
type ExecWriter = Arc<TokioMutex<ChildStdin>>;

/// Type alias for the channel map shared across tasks.
type ChannelMap = Arc<TokioMutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;

/// Maximum consecutive reconnection attempts before giving up.
const MAX_RETRIES: u32 = 5;

/// Heartbeat interval.
const HEARTBEAT_INTERVAL_SECS: u64 = 10;

/// Status of a tunnel to a container.
#[derive(Debug, Clone)]
pub enum TunnelStatus {
    Connected,
    Reconnecting,
    Failed { reason: String },
}

impl fmt::Display for TunnelStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => write!(f, "connected"),
            Self::Reconnecting => write!(f, "reconnecting"),
            Self::Failed { reason } => write!(f, "failed: {reason}"),
        }
    }
}

struct TunnelEntry {
    status: TunnelStatus,
    abort_handle: AbortHandle,
}

/// Manages tunnels to multiple containers.
pub struct TunnelManager {
    tunnels: StdMutex<HashMap<String, TunnelEntry>>,
    last_activity: Arc<AtomicU64>,
}

impl TunnelManager {
    pub fn new(last_activity: Arc<AtomicU64>) -> Self {
        Self {
            tunnels: StdMutex::new(HashMap::new()),
            last_activity,
        }
    }

    /// Start or reconnect a tunnel to a container.
    ///
    /// # Errors
    ///
    /// Returns error if the container ID is empty.
    pub async fn connect(self: &Arc<Self>, container_id: &str) -> Result<(), CellaTunnelError> {
        if container_id.is_empty() {
            return Err(CellaTunnelError::Tunnel {
                message: "container ID is empty".to_string(),
            });
        }

        // If there's an existing tunnel, abort it first
        {
            let mut tunnels = self
                .tunnels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = tunnels.remove(container_id) {
                entry.abort_handle.abort();
            }
        }

        let container_id_owned = container_id.to_string();
        let manager = Arc::clone(self);
        let handle: JoinHandle<()> = tokio::spawn(async move {
            run_tunnel_loop(&container_id_owned, &manager).await;
        });

        let abort_handle = handle.abort_handle();
        self.tunnels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                container_id.to_string(),
                TunnelEntry {
                    status: TunnelStatus::Connected,
                    abort_handle,
                },
            );

        info!("Tunnel registered for container {container_id}");
        Ok(())
    }

    /// Disconnect a tunnel.
    pub fn disconnect(&self, container_id: &str) {
        let mut tunnels = self
            .tunnels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = tunnels.remove(container_id) {
            entry.abort_handle.abort();
            info!("Tunnel disconnected for container {container_id}");
        }
    }

    /// Get status of all tunnels.
    pub fn status(&self) -> HashMap<String, String> {
        let tunnels = self
            .tunnels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tunnels
            .iter()
            .map(|(id, entry)| (id.clone(), entry.status.to_string()))
            .collect()
    }

    fn update_status(&self, container_id: &str, status: TunnelStatus) {
        let mut tunnels = self
            .tunnels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = tunnels.get_mut(container_id) {
            entry.status = status;
        }
    }

    fn touch_activity(&self) {
        self.last_activity
            .store(crate::daemon::current_time_secs(), Ordering::Relaxed);
    }
}

/// Tunnel loop with reconnection and backoff.
async fn run_tunnel_loop(container_id: &str, manager: &TunnelManager) {
    let mut retry_count = 0u32;

    loop {
        manager.update_status(container_id, TunnelStatus::Connected);
        info!("Starting tunnel for container {container_id}");

        match run_single_tunnel(container_id, manager).await {
            Ok(()) => {
                // Clean exit (container stopped or CLOSE)
                info!("Tunnel for {container_id} exited cleanly");
                return;
            }
            Err(e) => {
                warn!("Tunnel for {container_id} failed: {e}");
                retry_count += 1;

                if retry_count > MAX_RETRIES {
                    error!("Max retries ({MAX_RETRIES}) reached for {container_id}");
                    manager.update_status(
                        container_id,
                        TunnelStatus::Failed {
                            reason: format!("max retries exceeded: {e}"),
                        },
                    );
                    return;
                }
            }
        }

        // Exponential backoff: 1s, 2s, 4s, 8s, 16s, capped at 30s
        let delay_secs = (1u64 << (retry_count - 1)).min(30);
        info!("Reconnecting {container_id} in {delay_secs}s (attempt {retry_count}/{MAX_RETRIES})");
        manager.update_status(container_id, TunnelStatus::Reconnecting);
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
    }
}

/// Run a single tunnel session (one docker exec invocation).
async fn run_single_tunnel(
    container_id: &str,
    manager: &TunnelManager,
) -> Result<(), CellaTunnelError> {
    let mut child = tokio::process::Command::new("docker")
        .args([
            "exec",
            "-i",
            container_id,
            "/usr/local/bin/cella-tunnel-server",
            "--ssh-agent",
            "/tmp/cella-ssh-agent.sock",
            "--credential",
            "/tmp/cella-credential-proxy.sock",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CellaTunnelError::Tunnel {
            message: format!("failed to spawn docker exec: {e}"),
        })?;

    let exec_stdin = child.stdin.take().ok_or_else(|| CellaTunnelError::Tunnel {
        message: "docker exec stdin not available".to_string(),
    })?;
    let exec_stdout = child
        .stdout
        .take()
        .ok_or_else(|| CellaTunnelError::Tunnel {
            message: "docker exec stdout not available".to_string(),
        })?;

    let exec_writer: ExecWriter = Arc::new(TokioMutex::new(exec_stdin));

    // Channel map: channel_id → sender for forwarding data to channel handlers
    let channels: ChannelMap = Arc::new(TokioMutex::new(HashMap::new()));

    // Spawn heartbeat sender
    let heartbeat_writer = Arc::clone(&exec_writer);
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        loop {
            interval.tick().await;
            let mut w = heartbeat_writer.lock().await;
            if write_frame_async(&mut *w, &heartbeat_frame())
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Main read loop: read frames from docker exec stdout
    let mut reader = BufReader::new(exec_stdout);
    let result = loop {
        match read_frame_async(&mut reader).await {
            Ok(None) => {
                debug!("EOF on docker exec stdout for {container_id}");
                break Ok(());
            }
            Err(e) => {
                break Err(CellaTunnelError::Tunnel {
                    message: format!("frame read error: {e}"),
                });
            }
            Ok(Some(frame)) => {
                manager.touch_activity();
                dispatch_host_frame(frame, &exec_writer, &channels).await;
            }
        }
    };

    heartbeat_handle.abort();

    // Close all remaining channels
    channels.lock().await.clear();

    // Wait for child process to exit
    let _ = child.wait().await;

    result
}

/// Dispatch a frame received from the container.
async fn dispatch_host_frame(frame: Frame, exec_writer: &ExecWriter, channels: &ChannelMap) {
    match frame.frame_type {
        FrameType::Open => {
            let kind = frame
                .payload
                .first()
                .copied()
                .and_then(ChannelKind::from_byte);

            let Some(kind) = kind else {
                warn!("OPEN frame with invalid channel kind");
                return;
            };

            let (tx, rx) = mpsc::channel(256);
            channels.lock().await.insert(frame.channel, tx);

            let writer = Arc::clone(exec_writer);
            let channel_id = frame.channel;

            match kind {
                ChannelKind::SshAgent => {
                    debug!("Opening SSH agent channel {channel_id}");
                    tokio::spawn(async move {
                        crate::ssh_agent::handle_channel(channel_id, rx, writer).await;
                    });
                }
                ChannelKind::Credential => {
                    debug!("Opening credential channel {channel_id}");
                    tokio::spawn(async move {
                        crate::git_credential::handle_channel(channel_id, rx, writer).await;
                    });
                }
            }
        }
        FrameType::Data => {
            let map = channels.lock().await;
            if let Some(tx) = map.get(&frame.channel) {
                let _ = tx.send(frame.payload).await;
            }
        }
        FrameType::Close => {
            debug!("Channel {} closed by container", frame.channel);
            channels.lock().await.remove(&frame.channel);
        }
        FrameType::HeartbeatAck => {
            debug!("Heartbeat ACK received");
        }
        FrameType::Heartbeat => {
            // Container shouldn't send heartbeats, but respond gracefully
            let mut w = exec_writer.lock().await;
            let _ = write_frame_async(&mut *w, &heartbeat_ack_frame()).await;
        }
    }
}
