use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

/// Errors from devcontainer feature resolution and installation.
#[derive(Debug, Error, Diagnostic)]
pub enum FeatureError {
    /// OCI registry communication failed.
    #[error("registry error for {registry}: {message}")]
    #[diagnostic(code(cella::features::registry_error))]
    RegistryError { registry: String, message: String },

    /// Registry authentication failed.
    #[error("authentication failed for registry {registry}")]
    #[diagnostic(code(cella::features::authentication_failed))]
    AuthenticationFailed { registry: String },

    /// The requested feature does not exist.
    #[error("feature not found: {reference}")]
    #[diagnostic(code(cella::features::feature_not_found))]
    FeatureNotFound { reference: String },

    /// The downloaded artifact is malformed.
    #[error("invalid feature artifact for {feature_id}: {reason}")]
    #[diagnostic(code(cella::features::invalid_artifact))]
    InvalidArtifact { feature_id: String, reason: String },

    /// Feature metadata (devcontainer-feature.json) is invalid.
    #[error("invalid feature metadata for {feature_id}: {reason}")]
    #[diagnostic(code(cella::features::invalid_metadata))]
    InvalidMetadata { feature_id: String, reason: String },

    /// Cyclic dependency detected among features.
    #[error("cyclic installsAfter dependency among features: {}", features.join(", "))]
    #[diagnostic(code(cella::features::cyclic_dependency))]
    CyclicDependency { features: Vec<String> },

    /// Feature install script failed during container build.
    #[error("feature build failed for {feature_id}: {message}")]
    #[diagnostic(code(cella::features::build_failed))]
    BuildFailed { feature_id: String, message: String },

    /// Downloaded blob digest does not match the OCI manifest.
    #[error("digest mismatch for {feature_id}: expected {expected}, got {actual}")]
    #[diagnostic(code(cella::features::digest_mismatch))]
    DigestMismatch {
        feature_id: String,
        expected: String,
        actual: String,
    },

    /// HTTP fetch failed.
    #[error("failed to fetch {url}: {message}")]
    #[diagnostic(code(cella::features::fetch_failed))]
    FetchFailed { url: String, message: String },

    /// A local feature path does not exist.
    #[error("local feature not found: {}", path.display())]
    #[diagnostic(code(cella::features::local_feature_not_found))]
    LocalFeatureNotFound { path: PathBuf },

    /// The feature reference string cannot be parsed.
    #[error("invalid feature reference: {reference}: {reason}")]
    #[diagnostic(code(cella::features::invalid_reference))]
    InvalidReference { reference: String, reason: String },

    /// Generic I/O error.
    #[error(transparent)]
    #[diagnostic(code(cella::features::io))]
    Io(#[from] std::io::Error),
}

/// Non-fatal warnings emitted during feature resolution.
#[derive(Debug, Clone)]
pub enum FeatureWarning {
    /// Cyclic dependency detected but resolved via best-effort ordering.
    CyclicDependency { features: Vec<String> },
    /// An option key in user config does not exist in the feature metadata.
    UnknownOption { feature_id: String, option: String },
    /// An option value type does not match the declared type.
    TypeMismatch {
        feature_id: String,
        option: String,
        expected: String,
        got: String,
    },
    /// An enum option value is not in the allowed set.
    InvalidEnumValue {
        feature_id: String,
        option: String,
        value: String,
        allowed: Vec<String>,
    },
    /// A GitHub shorthand was used; an OCI equivalent exists.
    DeprecatedFeature { key: String, oci_equivalent: String },
}
