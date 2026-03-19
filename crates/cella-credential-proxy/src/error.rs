use thiserror::Error;

/// Errors that can occur in the credential proxy.
#[derive(Debug, Error)]
pub enum CellaCredentialProxyError {
    /// Failed to create or bind the Unix socket.
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
    #[error("credential proxy daemon is already running (PID {pid})")]
    AlreadyRunning { pid: u32 },

    /// The daemon is not running.
    #[error("credential proxy daemon is not running")]
    NotRunning,

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
