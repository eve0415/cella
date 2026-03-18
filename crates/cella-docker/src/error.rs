use thiserror::Error;

/// Errors that can occur during container runtime operations.
#[derive(Debug, Error)]
pub enum CellaDockerError {
    /// The container runtime is not available.
    #[error("container runtime not found")]
    RuntimeNotFound,

    /// A container operation failed.
    #[error("container operation failed: {0}")]
    OperationFailed(String),
}
