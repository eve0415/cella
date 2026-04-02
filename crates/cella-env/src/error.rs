use miette::Diagnostic;
use thiserror::Error;

/// Errors that can occur during environment forwarding setup.
#[derive(Debug, Error, Diagnostic)]
pub enum CellaEnvError {
    /// Failed to read host git configuration.
    #[error("failed to read host git config: {message}")]
    #[diagnostic(code(cella::env::git_config_read))]
    GitConfigRead { message: String },

    /// Failed to read SSH configuration files.
    #[error("failed to read SSH config: {message}")]
    #[diagnostic(code(cella::env::ssh_config_read))]
    SshConfigRead { message: String },

    /// Failed to probe user environment inside container.
    #[error("failed to probe user environment: {message}")]
    #[diagnostic(code(cella::env::user_env_probe))]
    UserEnvProbe { message: String },

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    #[diagnostic(code(cella::env::io))]
    Io(#[from] std::io::Error),
}
