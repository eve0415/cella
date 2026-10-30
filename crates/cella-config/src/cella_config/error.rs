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

    #[error(transparent)]
    #[diagnostic(transparent)]
    Network(#[from] cella_network::NetworkConfigError),

    #[error(
        "network rule `{rule}` in customizations.cella uses `{field}`, which is only accepted in \
         .devcontainer/cella.toml (or ~/.cella/config.toml); move the rule there"
    )]
    #[diagnostic(help(
        "`action = \"respond\"`, `responses` and `response_headers` are cella config file settings"
    ))]
    RespondFieldInCustomizations { rule: String, field: String },

    #[error("network rule `{rule}`: `{field}` sets both `body` and `body_file`; use one")]
    ResponseBodyConflict { rule: String, field: String },

    #[error(
        "network rule `{rule}`: `{field}.body_file` ({path}) is not allowed because status {status} has no body",
        path = path.display()
    )]
    ResponseBodyFileNotAllowed {
        rule: String,
        field: String,
        status: u16,
        path: PathBuf,
    },

    #[error(
        "network rule `{rule}`: `{field}.body_file` ({path}): {problem}",
        path = path.display()
    )]
    ResponseBodyFile {
        rule: String,
        field: String,
        path: PathBuf,
        problem: crate::settings::network::BodyFileProblem,
    },
}
