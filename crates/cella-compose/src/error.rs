//! Error types for Docker Compose operations.

use std::path::PathBuf;

/// Errors that can occur during Docker Compose operations.
#[derive(Debug, thiserror::Error)]
pub enum CellaComposeError {
    /// Docker Compose CLI not found or not V2.
    #[error("docker compose CLI not found: {message}")]
    CliNotFound {
        /// Details about what went wrong.
        message: String,
    },

    /// Docker Compose command failed with a non-zero exit code.
    #[error("docker compose failed (exit {exit_code}): {stderr}")]
    ComposeFailed {
        /// The exit code from the docker compose process.
        exit_code: i32,
        /// Captured stderr output.
        stderr: String,
    },

    /// A referenced compose file does not exist.
    #[error("compose file not found: {}", path.display())]
    FileNotFound {
        /// The missing file path.
        path: PathBuf,
    },

    /// The primary service is not defined in any compose file.
    #[error("service '{service}' not found in compose file(s); available: {available}")]
    ServiceNotFound {
        /// The service name that was not found.
        service: String,
        /// Comma-separated list of available services.
        available: String,
    },

    /// Failed to parse a compose YAML file.
    #[error("compose YAML parse error: {0}")]
    YamlParse(String),

    /// Missing required field in devcontainer.json for compose config.
    #[error("missing required compose field: {field}")]
    MissingField {
        /// The name of the missing field.
        field: String,
    },

    /// Failed to parse `docker compose config` output.
    #[error("docker compose config parse failed: {message}")]
    ConfigParseFailed {
        /// Details about what went wrong.
        message: String,
    },

    /// Service has neither `build` nor `image` defined.
    #[error("service '{service}' has neither 'build' nor 'image' in compose config")]
    ServiceHasNoBuildOrImage {
        /// The service name.
        service: String,
    },

    /// Docker Compose version is too old for the requested operation.
    #[error("Docker Compose >= {required} required for {feature}. Found: {found}")]
    UnsupportedVersion {
        /// The minimum required version (e.g., "2.17.0").
        required: String,
        /// The detected version (e.g., "2.16.0").
        found: String,
        /// The feature that requires the newer version.
        feature: String,
    },

    /// Dockerfile parsing error.
    #[error("dockerfile error: {message}")]
    DockerfileParse {
        /// Details about what went wrong.
        message: String,
    },

    /// I/O error (reading compose files, writing override files).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
