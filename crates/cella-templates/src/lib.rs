//! Devcontainer template resolution, fetching, and application.
//!
//! This crate handles the full template lifecycle:
//!
//! - **Discovery**: Fetch collection indexes from OCI registries
//! - **Caching**: 24h file cache for collection indexes and template artifacts
//! - **Fetching**: Download individual template tarballs from OCI registries
//! - **Options**: Validate and resolve template option values
//! - **Application**: Extract template files, substitute placeholders, generate config

pub mod apply;
pub mod cache;
pub mod collection;
pub mod error;
pub mod fetcher;
pub mod options;
pub mod types;

pub use cache::TemplateCache;
pub use collection::{DEFAULT_FEATURE_COLLECTION, DEFAULT_TEMPLATE_COLLECTION};
pub use error::TemplateError;
pub use types::{
    FeatureCollectionIndex, FeatureSummary, InitSelection, OutputFormat, SelectedFeature,
    TemplateCollectionIndex, TemplateMetadata, TemplateOption, TemplateSummary,
};
