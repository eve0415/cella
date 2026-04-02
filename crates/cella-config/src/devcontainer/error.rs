use super::discover;
use miette::Diagnostic;
use thiserror::Error;

/// Errors that can occur during configuration parsing and management.
#[derive(Debug, Error, Diagnostic)]
pub enum CellaConfigError {
    /// Failed to read a configuration file.
    #[error("failed to read config file: {path}")]
    #[diagnostic(code(cella::config::read_file))]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// Failed to parse JSON content.
    #[error("failed to parse config")]
    #[diagnostic(code(cella::config::parse))]
    Parse(#[from] serde_json::Error),

    /// JSONC preprocessing failed.
    #[error("JSONC error: {0}")]
    #[diagnostic(code(cella::config::jsonc))]
    Jsonc(String),

    /// Config validation failed with diagnostics.
    #[error("config validation failed with {error_count} error(s)")]
    #[diagnostic(code(cella::config::validation))]
    Validation { error_count: usize },

    /// Config discovery failed.
    #[error("config discovery failed: {0}")]
    #[diagnostic(code(cella::config::discovery))]
    Discovery(#[from] discover::Error),
}
