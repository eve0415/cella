//! Unified error type for container backend operations.

use thiserror::Error;

/// Errors that can occur during container backend operations.
///
/// All backends map their internal errors to these variants so callers can
/// handle failures uniformly regardless of the underlying runtime.
#[derive(Debug, Error)]
pub enum BackendError {
    /// No container found for the given identifier.
    #[error("container not found: {identifier}")]
    ContainerNotFound { identifier: String },

    /// Container exists but is not running.
    #[error("{hint}")]
    ContainerNotRunning { hint: String },

    /// Image not found locally.
    #[error("image not found: {image}")]
    ImageNotFound { image: String },

    /// Image build failed.
    #[error("build failed: {message}")]
    ImageBuildFailed { message: String },

    /// Command execution inside a container failed.
    #[error("exec failed (exit code {exit_code}): {command}")]
    ExecFailed { command: String, exit_code: i64 },

    /// Lifecycle command phase failed.
    #[error("lifecycle command failed: {phase} — {message}")]
    LifecycleFailed { phase: String, message: String },

    /// Cannot connect to container runtime.
    #[error("connection failed: {message}")]
    ConnectionFailed { message: String },

    /// The requested operation is not supported by this backend.
    #[error("not supported by {backend}: {operation}")]
    NotSupported { backend: String, operation: String },

    /// The backend CLI binary was not found.
    #[error("backend CLI not found: {message}")]
    CliNotFound { message: String },

    /// Container exited immediately after start.
    #[error("container exited immediately (exit code {exit_code}):\n{logs_tail}")]
    ContainerExitedImmediately { exit_code: i64, logs_tail: String },

    /// Agent volume population error.
    #[error("agent volume error: {message}")]
    AgentVolume { message: String },

    /// Agent binary checksum verification failed.
    #[error("agent binary checksum mismatch: expected {expected}, got {actual}")]
    AgentChecksumMismatch { expected: String, actual: String },

    /// A host-side command failed.
    #[error("host command failed: {command}")]
    HostCommandFailed {
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Wrapped runtime-specific error from the underlying SDK.
    #[error(transparent)]
    Runtime(Box<dyn std::error::Error + Send + Sync>),
}
