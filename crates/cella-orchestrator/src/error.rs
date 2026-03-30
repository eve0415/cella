//! Orchestrator error types.

/// Errors from orchestrator operations.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("docker: {message}")]
    Docker { message: String },

    #[error("git: {message}")]
    Git { message: String },

    #[error("config: {message}")]
    Config { message: String },

    #[error("container exited immediately: {message}")]
    ContainerExited { message: String },

    #[error("host requirements not met: {message}")]
    HostRequirements { message: String },

    #[error("{message}")]
    Other { message: String },
}
