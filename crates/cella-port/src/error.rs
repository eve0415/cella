use thiserror::Error;

/// Errors that can occur during port management.
#[derive(Debug, Error)]
pub enum CellaPortError {
    /// The requested port is already in use.
    #[error("port {0} is already in use")]
    PortInUse(u16),

    /// No available ports in the configured range.
    #[error("no available ports in range")]
    NoAvailablePorts,

    /// Failed to read /proc/net/tcp.
    #[error("port detection error: {0}")]
    Detection(#[from] std::io::Error),

    /// Control socket communication error.
    #[error("control socket error: {message}")]
    ControlSocket { message: String },

    /// Failed to serialize/deserialize protocol messages.
    #[error("protocol error: {0}")]
    Protocol(#[from] serde_json::Error),
}
