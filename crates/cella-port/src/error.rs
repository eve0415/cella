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
}
