//! Error types for Docker Compose operations.

use std::path::PathBuf;

use miette::Diagnostic;

/// Errors that can occur during Docker Compose operations.
#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum CellaComposeError {
    /// Docker Compose CLI not found or not V2.
    #[error("docker compose CLI not found: {message}")]
    #[diagnostic(code(cella::compose::cli_not_found))]
    CliNotFound {
        /// Details about what went wrong.
        message: String,
    },

    /// Docker Compose command failed with a non-zero exit code.
    #[error("docker compose failed (exit {exit_code}): {stderr}")]
    #[diagnostic(code(cella::compose::compose_failed))]
    ComposeFailed {
        /// The exit code from the docker compose process.
        exit_code: i32,
        /// Captured stderr output.
        stderr: String,
    },

    /// A referenced compose file does not exist.
    #[error("compose file not found: {}", path.display())]
    #[diagnostic(code(cella::compose::file_not_found))]
    FileNotFound {
        /// The missing file path.
        path: PathBuf,
    },

    /// The primary service is not defined in any compose file.
    #[error("service '{service}' not found in compose file(s); available: {available}")]
    #[diagnostic(code(cella::compose::service_not_found))]
    ServiceNotFound {
        /// The service name that was not found.
        service: String,
        /// Comma-separated list of available services.
        available: String,
    },

    /// Failed to parse a compose YAML file.
    #[error("compose YAML parse error: {0}")]
    #[diagnostic(code(cella::compose::yaml_parse))]
    YamlParse(String),

    /// Missing required field in devcontainer.json for compose config.
    #[error("missing required compose field: {field}")]
    #[diagnostic(code(cella::compose::missing_field))]
    MissingField {
        /// The name of the missing field.
        field: String,
    },

    /// Failed to parse `docker compose config` output.
    #[error("docker compose config parse failed: {message}")]
    #[diagnostic(code(cella::compose::config_parse_failed))]
    ConfigParseFailed {
        /// Details about what went wrong.
        message: String,
    },

    /// Service has neither `build` nor `image` defined.
    #[error("service '{service}' has neither 'build' nor 'image' in compose config")]
    #[diagnostic(code(cella::compose::service_has_no_build_or_image))]
    ServiceHasNoBuildOrImage {
        /// The service name.
        service: String,
    },

    /// Docker Compose version is too old for the requested operation.
    #[error("Docker Compose >= {required} required for {feature}. Found: {found}")]
    #[diagnostic(code(cella::compose::unsupported_version))]
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
    #[diagnostic(code(cella::compose::dockerfile_parse))]
    DockerfileParse {
        /// Details about what went wrong.
        message: String,
    },

    /// I/O error (reading compose files, writing override files).
    #[error(transparent)]
    #[diagnostic(code(cella::compose::io))]
    Io(#[from] std::io::Error),
}
