use thiserror::Error;

/// Errors that can occur in the cella daemon.
#[derive(Debug, Error)]
pub enum CellaDaemonError {
    /// Failed to create or bind a socket.
    #[error("socket error: {message}")]
    Socket { message: String },

    /// Failed to write or read the PID file.
    #[error("PID file error: {message}")]
    PidFile { message: String },

    /// Failed to invoke the host git credential helper.
    #[error("git credential error: {message}")]
    GitCredential { message: String },

    /// Protocol parse error.
    #[error("protocol error: {message}")]
    Protocol { message: String },

    /// The daemon is already running.
    #[error("cella daemon is already running (PID {pid})")]
    AlreadyRunning { pid: u32 },

    /// The daemon is not running.
    #[error("cella daemon is not running")]
    NotRunning,

    /// Port forwarding error.
    #[error("port forwarding error: {message}")]
    PortForwarding { message: String },

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
