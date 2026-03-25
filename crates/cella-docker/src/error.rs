use thiserror::Error;

/// Errors that can occur during container runtime operations.
#[derive(Debug, Error)]
pub enum CellaDockerError {
    /// The container runtime is not available.
    #[error("container runtime not found: {message}")]
    RuntimeNotFound { message: String },

    /// Docker API error.
    #[error("Docker API error: {0}")]
    DockerApi(#[from] bollard::errors::Error),

    /// Image not found.
    #[error("image not found: {image}")]
    ImageNotFound { image: String },

    /// Docker CLI not found.
    #[error("docker CLI not found: {message}")]
    DockerCliNotFound { message: String },

    /// Image build failed.
    #[error("build failed: {message}")]
    BuildFailed { message: String },

    /// No container for the given workspace.
    #[error("container not found for workspace: {workspace}")]
    ContainerNotFound { workspace: String },

    /// Container exists but is not running.
    #[error("{hint}")]
    ContainerNotRunning { hint: String },

    /// A command executed inside the container failed.
    #[error("exec failed (exit code {exit_code}): {command}")]
    ExecFailed { command: String, exit_code: i64 },

    /// A lifecycle command phase failed.
    #[error("lifecycle command failed: {phase} — {message}")]
    LifecycleFailed { phase: String, message: String },

    /// A host-side command failed.
    #[error("host command failed: {command}")]
    HostCommandFailed {
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// The container exited immediately after start.
    #[error("container exited immediately (exit code {exit_code}):\n{logs_tail}")]
    ContainerExitedImmediately { exit_code: i64, logs_tail: String },

    /// Agent volume population error.
    #[error("agent volume error: {message}")]
    AgentVolume { message: String },

    /// Agent binary checksum verification failed.
    #[error("agent binary checksum mismatch: expected {expected}, got {actual}")]
    AgentChecksumMismatch { expected: String, actual: String },

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
