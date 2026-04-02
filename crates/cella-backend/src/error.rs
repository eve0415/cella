//! Unified error type for container backend operations.

use miette::Diagnostic;
use thiserror::Error;

/// Errors that can occur during container backend operations.
///
/// All backends map their internal errors to these variants so callers can
/// handle failures uniformly regardless of the underlying runtime.
#[derive(Debug, Error, Diagnostic)]
pub enum BackendError {
    /// No container found for the given identifier.
    #[error("container not found: {identifier}")]
    #[diagnostic(
        code(cella::container_not_found),
        help("Run `cella up` to start a container, or `cella list` to see existing ones.")
    )]
    ContainerNotFound { identifier: String },

    /// Container exists but is not running.
    #[error("{hint}")]
    #[diagnostic(code(cella::backend::container_not_running))]
    ContainerNotRunning { hint: String },

    /// Image not found locally.
    #[error("image not found: {image}")]
    #[diagnostic(code(cella::backend::image_not_found))]
    ImageNotFound { image: String },

    /// Image build failed.
    #[error("build failed: {message}")]
    #[diagnostic(
        code(cella::build_failed),
        help("Check your Dockerfile syntax and build context.")
    )]
    ImageBuildFailed { message: String },

    /// Command execution inside a container failed.
    #[error("exec failed (exit code {exit_code}): {command}")]
    #[diagnostic(code(cella::backend::exec_failed))]
    ExecFailed { command: String, exit_code: i64 },

    /// Lifecycle command phase failed.
    #[error("lifecycle command failed: {phase} — {message}")]
    #[diagnostic(
        code(cella::lifecycle_failed),
        help("A lifecycle command failed. Use `cella exec` to debug the container.")
    )]
    LifecycleFailed { phase: String, message: String },

    /// Cannot connect to container runtime.
    #[error("connection failed: {message}")]
    #[diagnostic(
        code(cella::connection_failed),
        help("Is Docker running? Try `cella doctor` to diagnose.")
    )]
    ConnectionFailed { message: String },

    /// The requested operation is not supported by this backend.
    #[error("not supported by {backend}: {operation}")]
    #[diagnostic(code(cella::backend::not_supported))]
    NotSupported { backend: String, operation: String },

    /// The backend CLI binary was not found.
    #[error("backend CLI not found: {message}")]
    #[diagnostic(
        code(cella::cli_not_found),
        help("Install Docker Desktop or ensure `docker` is in your PATH.")
    )]
    CliNotFound { message: String },

    /// Container exited immediately after start.
    #[error("container exited immediately (exit code {exit_code}):\n{logs_tail}")]
    #[diagnostic(
        code(cella::container_exited),
        help("The container exited before it could be used. Check the logs above.")
    )]
    ContainerExitedImmediately { exit_code: i64, logs_tail: String },

    /// Agent volume population error.
    #[error("agent volume error: {message}")]
    #[diagnostic(code(cella::backend::agent_volume))]
    AgentVolume { message: String },

    /// Agent binary checksum verification failed.
    #[error("agent binary checksum mismatch: expected {expected}, got {actual}")]
    #[diagnostic(code(cella::backend::agent_checksum_mismatch))]
    AgentChecksumMismatch { expected: String, actual: String },

    /// A host-side command failed.
    #[error("host command failed: {command}")]
    #[diagnostic(code(cella::backend::host_command_failed))]
    HostCommandFailed {
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    #[diagnostic(code(cella::backend::io))]
    Io(#[from] std::io::Error),

    /// Wrapped runtime-specific error from the underlying SDK.
    #[error(transparent)]
    #[diagnostic(code(cella::backend::runtime))]
    Runtime(Box<dyn std::error::Error + Send + Sync>),
}
