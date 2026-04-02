use miette::Diagnostic;
use thiserror::Error;

/// Errors that can occur during port management.
#[derive(Debug, Error, Diagnostic)]
pub enum CellaPortError {
    /// The requested port is already in use.
    #[error("port {0} is already in use")]
    #[diagnostic(code(cella::port::port_in_use))]
    PortInUse(u16),

    /// No available ports in the configured range.
    #[error("no available ports in range")]
    #[diagnostic(code(cella::port::no_available_ports))]
    NoAvailablePorts,

    /// Failed to read /proc/net/tcp.
    #[error("port detection error: {0}")]
    #[diagnostic(code(cella::port::detection))]
    Detection(#[from] std::io::Error),

    /// Control socket communication error.
    #[error("control socket error: {message}")]
    #[diagnostic(code(cella::port::control_socket))]
    ControlSocket { message: String },

    /// Failed to serialize/deserialize protocol messages.
    #[error("protocol error: {0}")]
    #[diagnostic(code(cella::port::protocol))]
    Protocol(#[from] serde_json::Error),
}
