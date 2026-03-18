use crate::discover::DiscoverError;
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

    /// JSONC preprocessing failed.
    #[error("JSONC error: {0}")]
    Jsonc(String),

    /// Config validation failed with diagnostics.
    #[error("config validation failed with {error_count} error(s)")]
    Validation { error_count: usize },

    /// Config discovery failed.
    #[error("config discovery failed: {0}")]
    Discovery(#[from] DiscoverError),
}
