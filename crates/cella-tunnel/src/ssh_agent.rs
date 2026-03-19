//! SSH agent channel handler.
//!
//! Connects to the host `SSH_AUTH_SOCK` and relays bytes bidirectionally
//! between the mux channel and the host SSH agent socket.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, mpsc};
use tracing::warn;

use crate::mux::{close_frame, data_frame, write_frame_async};

/// Handle an SSH agent channel.
///
/// Connects to the host SSH agent socket and relays bytes bidirectionally
/// between the mux channel and the host socket.
pub async fn handle_channel(
    channel_id: u32,
    mut rx: mpsc::Receiver<Vec<u8>>,
    exec_writer: Arc<Mutex<ChildStdin>>,
) {
    let ssh_sock = match std::env::var("SSH_AUTH_SOCK") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            warn!("SSH_AUTH_SOCK not set, cannot handle SSH agent channel");
            let mut w = exec_writer.lock().await;
            let _ = write_frame_async(&mut *w, &close_frame(channel_id)).await;
            return;
        }
    };

    let stream = match UnixStream::connect(&ssh_sock).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to SSH agent at {ssh_sock}: {e}");
            let mut w = exec_writer.lock().await;
            let _ = write_frame_async(&mut *w, &close_frame(channel_id)).await;
            return;
        }
    };

    let (mut reader, mut writer) = stream.into_split();

    // Task: receive data from container via mux → write to SSH agent socket
    let write_handle = tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
            if writer.write_all(&payload).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    // Task: read from SSH agent socket → send DATA frames back to container
    let exec_writer_clone = Arc::clone(&exec_writer);
    let read_handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 32768];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut w = exec_writer_clone.lock().await;
                    if write_frame_async(&mut *w, &data_frame(channel_id, buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        // SSH agent closed — send CLOSE to container
        let mut w = exec_writer_clone.lock().await;
        let _ = write_frame_async(&mut *w, &close_frame(channel_id)).await;
    });

    let _ = tokio::join!(write_handle, read_handle);
}
