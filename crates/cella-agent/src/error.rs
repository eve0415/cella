use thiserror::Error;

/// Errors that can occur during agent sandbox operations.
#[derive(Debug, Error)]
pub enum CellaAgentError {
    /// Failed to create the agent sandbox.
    #[error("failed to create agent sandbox: {0}")]
    SandboxCreation(String),

    /// The agent sandbox is not running.
    #[error("agent sandbox is not running")]
    NotRunning,
}
