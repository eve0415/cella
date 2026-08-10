//! Typed, actionable errors for registry communication.

use miette::Diagnostic;
use thiserror::Error;

/// A registry request failed.
///
/// Carries help text because the two causes a user can actually do something
/// about — no network, or missing credentials — are indistinguishable from
/// the transport error alone.
#[derive(Debug, Error, Diagnostic)]
#[error("failed to list tags for {reference}: {message}")]
#[diagnostic(
    code(cella::oci::tag_list_failed),
    help("check your network connection, or registry credentials in ~/.docker/config.json")
)]
pub struct TagListError {
    /// The reference that could not be listed.
    pub reference: String,
    /// The underlying transport or registry error.
    pub message: String,
}

impl TagListError {
    /// Build a listing failure for `reference` from any displayable cause.
    pub fn new(reference: &str, cause: &impl std::fmt::Display) -> Self {
        Self {
            reference: reference.to_owned(),
            message: cause.to_string(),
        }
    }
}
