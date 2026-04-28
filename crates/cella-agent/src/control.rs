//! TCP client for communicating with the host daemon.

use cella_port::CellaPortError;
use cella_protocol::{AgentHello, AgentMessage, DaemonHello, DaemonMessage, PROTOCOL_VERSION};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Client for sending messages to the host daemon via TCP.
pub struct ControlClient {
    writer: tokio::io::WriteHalf<TcpStream>,
    /// After `start_reader()`, messages arrive via this channel instead of the
    /// raw TCP reader. `None` before `start_reader()` is called.
    response_rx: Option<tokio::sync::mpsc::Receiver<DaemonMessage>>,
    /// Direct reader, used only during handshake and before `start_reader()`.
    reader: Option<BufReader<tokio::io::ReadHalf<TcpStream>>>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ControlClient {
    /// Connect to the host daemon via TCP.
    ///
    /// Returns the client and the `DaemonHello` received during handshake.
    ///
    /// # Errors
    ///
    /// Returns error if connection or handshake fails.
    pub async fn connect(
        addr: &str,
        container_name: &str,
        auth_token: &str,
    ) -> Result<(Self, DaemonHello), CellaPortError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| CellaPortError::ControlSocket {
                message: format!("failed to connect to {addr}: {e}"),
            })?;

        let (reader, writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        let mut client = Self {
            writer,
            response_rx: None,
            reader: None,
            reader_handle: None,
        };

        // Perform Hello handshake
        let hello = AgentHello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            container_name: container_name.to_string(),
            auth_token: auth_token.to_string(),
        };
        let mut json = serde_json::to_string(&hello)?;
        json.push('\n');
        client
            .writer
            .write_all(json.as_bytes())
            .await
            .map_err(|e| CellaPortError::ControlSocket {
                message: format!("hello write error: {e}"),
            })?;
        client
            .writer
            .flush()
            .await
            .map_err(|e| CellaPortError::ControlSocket {
                message: format!("hello flush error: {e}"),
            })?;

        // Read DaemonHello response
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| CellaPortError::ControlSocket {
                message: format!("hello read error: {e}"),
            })?;
        if line.is_empty() {
            return Err(CellaPortError::ControlSocket {
                message: "daemon closed connection during hello".to_string(),
            });
        }
        let daemon_hello: DaemonHello = serde_json::from_str(line.trim())?;
        if let Some(err) = daemon_hello.error {
            return Err(CellaPortError::ControlSocket {
                message: format!("daemon rejected connection: {err}"),
            });
        }

        client.reader = Some(reader);
        Ok((client, daemon_hello))
    }

    /// Spawn a background reader task that dispatches `TunnelRequest` messages
    /// to the tunnel handler and forwards everything else to the response channel.
    pub fn start_reader(&mut self, tunnel_config: Option<crate::tunnel::TunnelConfig>) {
        let Some(reader) = self.reader.take() else {
            return;
        };

        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        self.response_rx = Some(rx);

        self.reader_handle = Some(tokio::spawn(async move {
            run_reader_loop(reader, tx, tunnel_config).await;
        }));
    }

    /// Send a message to the daemon (newline-delimited JSON).
    ///
    /// # Errors
    ///
    /// Returns error on serialization or I/O failure.
    pub async fn send(&mut self, msg: &AgentMessage) -> Result<(), CellaPortError> {
        let mut json = serde_json::to_string(msg)?;
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.map_err(|e| {
            CellaPortError::ControlSocket {
                message: format!("write error: {e}"),
            }
        })?;
        self.writer
            .flush()
            .await
            .map_err(|e| CellaPortError::ControlSocket {
                message: format!("flush error: {e}"),
            })?;
        Ok(())
    }

    /// Read a response message from the daemon.
    ///
    /// Uses the background reader channel if `start_reader()` was called,
    /// otherwise reads directly from the TCP stream (for one-shot clients).
    ///
    /// # Errors
    ///
    /// Returns error on I/O or deserialization failure.
    pub async fn recv(&mut self) -> Result<DaemonMessage, CellaPortError> {
        if let Some(ref mut rx) = self.response_rx {
            return rx
                .recv()
                .await
                .ok_or_else(|| CellaPortError::ControlSocket {
                    message: "connection closed".to_string(),
                });
        }

        if let Some(ref mut reader) = self.reader {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .map_err(|e| CellaPortError::ControlSocket {
                    message: format!("read error: {e}"),
                })?;
            if line.is_empty() {
                return Err(CellaPortError::ControlSocket {
                    message: "connection closed".to_string(),
                });
            }
            let msg: DaemonMessage = serde_json::from_str(&line)?;
            return Ok(msg);
        }

        Err(CellaPortError::ControlSocket {
            message: "not connected".to_string(),
        })
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}

async fn run_reader_loop(
    mut reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    response_tx: tokio::sync::mpsc::Sender<DaemonMessage>,
    tunnel_config: Option<crate::tunnel::TunnelConfig>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_str::<DaemonMessage>(line.trim()) else {
            tracing::warn!("Invalid daemon message, skipping");
            continue;
        };
        if let DaemonMessage::TunnelRequest {
            connection_id,
            target_port,
        } = msg
        {
            if let Some(ref config) = tunnel_config {
                let config = config.clone();
                tokio::spawn(async move {
                    crate::tunnel::handle_tunnel_request(connection_id, target_port, &config).await;
                });
            }
        } else if response_tx.send(msg).await.is_err() {
            break;
        }
    }
}

/// Daemon connection info read from the shared volume file.
pub struct DaemonAddrInfo {
    pub addr: String,
    pub token: String,
}

/// Read daemon connection info from `/cella/.daemon_addr`.
///
/// The file contains two lines: the daemon address (`host:port`) and the
/// auth token. Written by the host CLI during `cella up`.
///
/// Returns `None` if the file doesn't exist or can't be parsed.
pub fn read_daemon_addr_file() -> Option<DaemonAddrInfo> {
    let content = std::fs::read_to_string("/cella/.daemon_addr").ok()?;
    let mut lines = content.lines();
    let addr = lines.next()?.trim().to_string();
    let token = lines.next()?.trim().to_string();
    if addr.is_empty() || token.is_empty() {
        return None;
    }
    Some(DaemonAddrInfo { addr, token })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[test]
    fn read_daemon_addr_file_returns_a_result() {
        // Returns Some if the file exists, None if not — either is valid.
        let _result = read_daemon_addr_file();
    }

    #[test]
    fn daemon_addr_info_fields() {
        let info = DaemonAddrInfo {
            addr: "127.0.0.1:5000".to_string(),
            token: "secret-token".to_string(),
        };
        assert_eq!(info.addr, "127.0.0.1:5000");
        assert_eq!(info.token, "secret-token");
    }

    #[tokio::test]
    async fn connect_fails_on_unreachable_address() {
        let result = ControlClient::connect("127.0.0.1:1", "test-container", "token").await;
        let Err(err) = result else {
            panic!("expected error on unreachable address");
        };
        let msg = err.to_string();
        assert!(msg.contains("failed to connect"));
    }

    #[tokio::test]
    async fn connect_fails_when_server_closes_immediately() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        let result = ControlClient::connect(&addr.to_string(), "test-container", "token").await;
        assert!(
            result.is_err(),
            "expected error when server closes connection immediately"
        );
    }

    #[tokio::test]
    async fn connect_fails_on_invalid_json_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            stream.write_all(b"not-json\n").await.unwrap();
        });

        let result = ControlClient::connect(&addr.to_string(), "test-container", "token").await;
        let Err(err) = result else {
            panic!("expected error on invalid JSON");
        };
        let msg = err.to_string();
        assert!(msg.contains("protocol error"));
    }

    #[tokio::test]
    async fn connect_fails_when_daemon_rejects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            let hello = DaemonHello {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".to_string(),
                error: Some("auth failed".to_string()),
                workspace_path: None,
                parent_repo: None,
                is_worktree: false,
            };
            let mut json = serde_json::to_string(&hello).unwrap();
            json.push('\n');
            stream.write_all(json.as_bytes()).await.unwrap();
        });

        let result = ControlClient::connect(&addr.to_string(), "test-container", "bad-token").await;
        let Err(err) = result else {
            panic!("expected error when daemon rejects");
        };
        let msg = err.to_string();
        assert!(msg.contains("daemon rejected connection"));
        assert!(msg.contains("auth failed"));
    }

    #[tokio::test]
    async fn connect_succeeds_with_valid_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read the AgentHello.
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            // Send a valid DaemonHello.
            let hello = DaemonHello {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".to_string(),
                error: None,
                workspace_path: None,
                parent_repo: None,
                is_worktree: false,
            };
            let mut json = serde_json::to_string(&hello).unwrap();
            json.push('\n');
            stream.write_all(json.as_bytes()).await.unwrap();
        });

        let result = ControlClient::connect(&addr.to_string(), "test-container", "token").await;
        assert!(result.is_ok());
        let (_client, hello) = result.unwrap();
        assert_eq!(hello.daemon_version, "0.1.0");
        assert!(hello.error.is_none());
    }

    #[tokio::test]
    async fn send_and_recv_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read AgentHello.
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            // Send DaemonHello.
            let hello = DaemonHello {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".to_string(),
                error: None,
                workspace_path: None,
                parent_repo: None,
                is_worktree: false,
            };
            let mut json = serde_json::to_string(&hello).unwrap();
            json.push('\n');
            stream.write_all(json.as_bytes()).await.unwrap();

            // Read the agent message.
            let mut msg_buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut msg_buf).await;

            // Send a DaemonMessage response (Ack).
            let response = DaemonMessage::Ack {
                id: Some("test-1".to_string()),
            };
            let mut resp_json = serde_json::to_string(&response).unwrap();
            resp_json.push('\n');
            stream.write_all(resp_json.as_bytes()).await.unwrap();
        });

        let (mut client, _hello) =
            ControlClient::connect(&addr.to_string(), "test-container", "token")
                .await
                .unwrap();
        client.start_reader(None);

        // Send a message.
        let msg = AgentMessage::Health {
            uptime_secs: 42,
            ports_detected: 3,
        };
        client.send(&msg).await.unwrap();

        // Receive the response.
        let resp = client.recv().await.unwrap();
        assert!(matches!(resp, DaemonMessage::Ack { id } if id.as_deref() == Some("test-1")));
    }

    #[tokio::test]
    async fn recv_returns_error_on_closed_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read AgentHello.
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            // Send DaemonHello.
            let hello = DaemonHello {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".to_string(),
                error: None,
                workspace_path: None,
                parent_repo: None,
                is_worktree: false,
            };
            let mut json = serde_json::to_string(&hello).unwrap();
            json.push('\n');
            stream.write_all(json.as_bytes()).await.unwrap();
            // Close the connection.
            drop(stream);
        });

        let (mut client, _hello) =
            ControlClient::connect(&addr.to_string(), "test-container", "token")
                .await
                .unwrap();
        client.start_reader(None);

        let result = client.recv().await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("connection closed"));
    }

    #[tokio::test]
    async fn recv_works_without_start_reader_for_oneshot_clients() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            let hello = DaemonHello {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".to_string(),
                error: None,
                workspace_path: None,
                parent_repo: None,
                is_worktree: false,
            };
            let mut json = serde_json::to_string(&hello).unwrap();
            json.push('\n');
            stream.write_all(json.as_bytes()).await.unwrap();

            let mut msg_buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut msg_buf).await;

            let response = DaemonMessage::Ack {
                id: Some("oneshot-1".to_string()),
            };
            let mut resp_json = serde_json::to_string(&response).unwrap();
            resp_json.push('\n');
            stream.write_all(resp_json.as_bytes()).await.unwrap();
        });

        let (mut client, _hello) =
            ControlClient::connect(&addr.to_string(), "test-container", "token")
                .await
                .unwrap();

        let msg = AgentMessage::Health {
            uptime_secs: 1,
            ports_detected: 0,
        };
        client.send(&msg).await.unwrap();

        let resp = client.recv().await.unwrap();
        assert!(matches!(resp, DaemonMessage::Ack { id } if id.as_deref() == Some("oneshot-1")));
    }
}
