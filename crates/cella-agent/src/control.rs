//! TCP client for communicating with the host daemon.

use cella_port::CellaPortError;
use cella_port::protocol::{
    AgentHello, AgentMessage, DaemonHello, DaemonMessage, PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Client for sending messages to the host daemon via TCP.
pub struct ControlClient {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: tokio::io::WriteHalf<TcpStream>,
}

impl ControlClient {
    /// Connect to the host daemon via TCP.
    ///
    /// # Errors
    ///
    /// Returns error if connection or handshake fails.
    pub async fn connect(
        addr: &str,
        container_name: &str,
        auth_token: &str,
    ) -> Result<Self, CellaPortError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| CellaPortError::ControlSocket {
                message: format!("failed to connect to {addr}: {e}"),
            })?;

        let (reader, writer) = tokio::io::split(stream);
        let mut client = Self {
            reader: BufReader::new(reader),
            writer,
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
        client
            .reader
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

        Ok(client)
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
    /// # Errors
    ///
    /// Returns error on I/O or deserialization failure.
    pub async fn recv(&mut self) -> Result<DaemonMessage, CellaPortError> {
        let mut line = String::new();
        self.reader
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
        Ok(msg)
    }
}
