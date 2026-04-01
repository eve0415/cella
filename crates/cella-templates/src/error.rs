use miette::Diagnostic;
use thiserror::Error;

/// Errors from devcontainer template resolution and application.
#[derive(Debug, Error, Diagnostic)]
pub enum TemplateError {
    /// OCI registry communication failed.
    #[error("registry error for {registry}: {message}")]
    RegistryError { registry: String, message: String },

    /// The collection index could not be fetched or parsed.
    #[error("failed to fetch collection index from {registry}: {message}")]
    CollectionFetchFailed { registry: String, message: String },

    /// The requested template does not exist in the collection.
    #[error("template not found: {id}")]
    TemplateNotFound { id: String },

    /// The downloaded template artifact is malformed.
    #[error("invalid template artifact for {template_id}: {reason}")]
    InvalidArtifact { template_id: String, reason: String },

    /// Template metadata (devcontainer-template.json) is invalid.
    #[error("invalid template metadata for {template_id}: {reason}")]
    InvalidMetadata { template_id: String, reason: String },

    /// A template option value is invalid.
    #[error("invalid option value for {template_id}.{option}: {reason}")]
    InvalidOptionValue {
        template_id: String,
        option: String,
        reason: String,
    },

    /// Downloaded blob digest does not match the OCI manifest.
    #[error("digest mismatch for {template_id}: expected {expected}, got {actual}")]
    DigestMismatch {
        template_id: String,
        expected: String,
        actual: String,
    },

    /// The output directory already contains a devcontainer configuration.
    #[error("devcontainer configuration already exists at {}", path.display())]
    ConfigAlreadyExists { path: std::path::PathBuf },

    /// Failed to fetch the aggregated devcontainer index.
    #[error("failed to fetch devcontainer index: {message}")]
    IndexFetchFailed { message: String },

    /// Cache I/O error.
    #[error("cache error: {message}")]
    CacheError { message: String },

    /// Generic I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
