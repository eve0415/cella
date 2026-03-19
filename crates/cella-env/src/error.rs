use thiserror::Error;

/// Errors that can occur during environment forwarding setup.
#[derive(Debug, Error)]
pub enum CellaEnvError {
    /// Failed to read host git configuration.
    #[error("failed to read host git config: {message}")]
    GitConfigRead { message: String },

    /// Failed to read SSH configuration files.
    #[error("failed to read SSH config: {message}")]
    SshConfigRead { message: String },

    /// Failed to probe user environment inside container.
    #[error("failed to probe user environment: {message}")]
    UserEnvProbe { message: String },

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
