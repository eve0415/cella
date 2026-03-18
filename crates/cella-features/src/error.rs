use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

/// Errors from devcontainer feature resolution and installation.
#[derive(Debug, Error, Diagnostic)]
pub enum FeatureError {
    /// OCI registry communication failed.
    #[error("registry error for {registry}: {message}")]
    RegistryError { registry: String, message: String },

    /// Registry authentication failed.
    #[error("authentication failed for registry {registry}")]
    AuthenticationFailed { registry: String },

    /// The requested feature does not exist.
    #[error("feature not found: {reference}")]
    FeatureNotFound { reference: String },

    /// The downloaded artifact is malformed.
    #[error("invalid feature artifact for {feature_id}: {reason}")]
    InvalidArtifact { feature_id: String, reason: String },

    /// Feature metadata (devcontainer-feature.json) is invalid.
    #[error("invalid feature metadata for {feature_id}: {reason}")]
    InvalidMetadata { feature_id: String, reason: String },

    /// Cyclic dependency detected among features.
    #[error("cyclic installsAfter dependency among features: {}", features.join(", "))]
    CyclicDependency { features: Vec<String> },

    /// Feature install script failed during container build.
    #[error("feature build failed for {feature_id}: {message}")]
    BuildFailed { feature_id: String, message: String },

    /// HTTP fetch failed.
    #[error("failed to fetch {url}: {message}")]
    FetchFailed { url: String, message: String },

    /// A local feature path does not exist.
    #[error("local feature not found: {}", path.display())]
    LocalFeatureNotFound { path: PathBuf },

    /// The feature reference string cannot be parsed.
    #[error("invalid feature reference: {reference}: {reason}")]
    InvalidReference { reference: String, reason: String },

    /// Generic I/O error.
    #[error(transparent)]
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
