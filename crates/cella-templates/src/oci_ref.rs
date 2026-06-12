//! Shared OCI reference parsing and client construction for template fetchers.

use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};

use cella_oci::build_registry_auth;

use crate::error::TemplateError;

/// Parse a template reference into `(registry, repository, tag)`.
///
/// Defaults to `latest` when no tag is present.
pub fn parse_template_ref(template_ref: &str) -> Result<(String, String, String), TemplateError> {
    let (base, tag) = match template_ref.rsplit_once(':') {
        Some((b, t)) if !t.contains('/') && !t.is_empty() => (b, t.to_owned()),
        _ => (template_ref, "latest".to_owned()),
    };

    let (registry, repository) =
        base.split_once('/')
            .ok_or_else(|| TemplateError::RegistryError {
                registry: template_ref.to_owned(),
                message: "invalid template reference: expected registry/repository[:tag]"
                    .to_owned(),
            })?;

    Ok((registry.to_owned(), repository.to_owned(), tag))
}

/// Build an OCI [`oci_distribution::Client`] and [`oci_distribution::Reference`]
/// for the given `(registry, repository, tag)` triple.
///
/// Returns `(client, reference, auth)` ready for manifest or blob pulls.
pub fn build_oci_client(
    registry: &str,
    repository: &str,
    tag: &str,
) -> (
    oci_distribution::Client,
    Reference,
    oci_distribution::secrets::RegistryAuth,
) {
    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    };
    let client = oci_distribution::Client::new(config);
    let oci_ref = Reference::with_tag(registry.to_owned(), repository.to_owned(), tag.to_owned());
    let auth = build_registry_auth(registry);
    (client, oci_ref, auth)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_ref() {
        let (reg, repo, tag) =
            parse_template_ref("ghcr.io/devcontainers/templates/rust:5.0.0").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/templates/rust");
        assert_eq!(tag, "5.0.0");
    }

    #[test]
    fn parse_ref_no_tag_defaults_to_latest() {
        let (reg, repo, tag) = parse_template_ref("ghcr.io/devcontainers/templates/rust").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/templates/rust");
        assert_eq!(tag, "latest");
    }

    #[test]
    fn parse_ref_invalid_returns_error() {
        assert!(parse_template_ref("noslash").is_err());
    }

    #[test]
    fn parse_ref_tag_with_slash_treated_as_no_tag() {
        // A "tag" containing a slash is not a tag — treat as tagless (latest).
        let (reg, repo, tag) =
            parse_template_ref("ghcr.io/devcontainers/templates/rust:repo/path").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/templates/rust:repo/path");
        assert_eq!(tag, "latest");
    }
}
