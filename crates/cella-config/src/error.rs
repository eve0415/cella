use thiserror::Error;

/// Errors that can occur during configuration parsing and management.
#[derive(Debug, Error)]
pub enum CellaConfigError {
    /// Failed to read a configuration file.
    #[error("failed to read config file: {path}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// Failed to parse JSON content.
    #[error("failed to parse config")]
    Parse(#[from] serde_json::Error),
}
