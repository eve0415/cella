use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum CellaConfigError {
    #[error("failed to read config file {path}")]
    #[diagnostic(help("check that the file exists and is readable"))]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid TOML in {path}")]
    #[diagnostic(help("check TOML syntax at the indicated location"))]
    ParseToml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("invalid JSON in {path}")]
    #[diagnostic(help("check JSON syntax — JSONC comments (// and /* */) are supported"))]
    ParseJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("JSONC preprocessing failed for {path}")]
    #[diagnostic(help("check for unterminated block comments"))]
    JsoncStrip { path: PathBuf, message: String },

    #[error("config validation failed")]
    #[diagnostic(help("check field names and value types against the cella config schema"))]
    Deserialization {
        #[source]
        source: serde_json::Error,
    },
}
