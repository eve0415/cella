use miette::Diagnostic;
use thiserror::Error;

/// Errors from devcontainer template resolution and application.
#[derive(Debug, Error, Diagnostic)]
pub enum TemplateError {
    /// OCI registry communication failed.
    #[error("registry error for {registry}: {message}")]
    #[diagnostic(code(cella::templates::registry_error))]
    RegistryError { registry: String, message: String },

    /// The collection index could not be fetched or parsed.
    #[error("failed to fetch collection index from {registry}: {message}")]
    #[diagnostic(code(cella::templates::collection_fetch_failed))]
    CollectionFetchFailed { registry: String, message: String },

    /// The requested template does not exist in the collection.
    #[error("template not found: {id}")]
    #[diagnostic(code(cella::templates::template_not_found))]
    TemplateNotFound { id: String },

    /// The downloaded template artifact is malformed.
    #[error("invalid template artifact for {template_id}: {reason}")]
    #[diagnostic(code(cella::templates::invalid_artifact))]
    InvalidArtifact { template_id: String, reason: String },

    /// Template metadata (devcontainer-template.json) is invalid.
    #[error("invalid template metadata for {template_id}: {reason}")]
    #[diagnostic(code(cella::templates::invalid_metadata))]
    InvalidMetadata { template_id: String, reason: String },

    /// A template option value is invalid.
    #[error("invalid option value for {template_id}.{option}: {reason}")]
    #[diagnostic(code(cella::templates::invalid_option_value))]
    InvalidOptionValue {
        template_id: String,
        option: String,
        reason: String,
    },

    /// Downloaded blob digest does not match the OCI manifest.
    #[error("digest mismatch for {template_id}: expected {expected}, got {actual}")]
    #[diagnostic(code(cella::templates::digest_mismatch))]
    DigestMismatch {
        template_id: String,
        expected: String,
        actual: String,
    },

    /// The output directory already contains a devcontainer configuration.
    #[error("devcontainer configuration already exists at {}", path.display())]
    #[diagnostic(code(cella::templates::config_already_exists))]
    ConfigAlreadyExists { path: std::path::PathBuf },

    /// Failed to fetch the aggregated devcontainer index.
    #[error("failed to fetch devcontainer index: {message}")]
    #[diagnostic(code(cella::templates::index_fetch_failed))]
    IndexFetchFailed { message: String },

    /// Failed to fetch image tags from a registry.
    #[error("failed to fetch tags for {image}: {message}")]
    #[diagnostic(code(cella::templates::tag_fetch_failed))]
    TagFetchFailed { image: String, message: String },

    /// Cache I/O error.
    #[error("cache error: {message}")]
    #[diagnostic(code(cella::templates::cache_error))]
    CacheError { message: String },

    /// Generic I/O error.
    #[error(transparent)]
    #[diagnostic(code(cella::templates::io))]
    Io(#[from] std::io::Error),
}
