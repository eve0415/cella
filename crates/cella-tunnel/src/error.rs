use thiserror::Error;

/// Errors that can occur in the tunnel system.
#[derive(Debug, Error)]
pub enum CellaTunnelError {
    /// Failed to create or bind a Unix socket.
    #[error("socket error: {message}")]
    Socket { message: String },

    /// Failed to write or read the PID file.
    #[error("PID file error: {message}")]
    PidFile { message: String },

    /// Tunnel operation failed.
    #[error("tunnel error: {message}")]
    Tunnel { message: String },

    /// Wire protocol error.
    #[error("protocol error: {message}")]
    Protocol { message: String },

    /// Failed to invoke the host git credential helper.
    #[error("git credential error: {message}")]
    GitCredential { message: String },

    /// The daemon is already running.
    #[error("tunnel daemon is already running (PID {pid})")]
    AlreadyRunning { pid: u32 },

    /// The daemon is not running.
    #[error("tunnel daemon is not running")]
    NotRunning,

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
