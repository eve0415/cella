//! Shared OCI reference parsing and client construction for template fetchers.

use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};

use cella_oci::build_registry_auth;

use crate::error::TemplateError;

/// The pinned version of a template reference: a mutable tag or an immutable
/// content digest.
///
/// Mirrors the official CLI's `getRef`, which resolves a ref to either a tag
/// or a `sha256:` digest and pulls the manifest by whichever is most specific.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefVersion {
    /// A mutable tag, e.g. `latest` or `5.0.0`.
    Tag(String),
    /// A content-addressable digest, e.g. `sha256:abc123…`.
    Digest(String),
}

impl RefVersion {
    /// The version string used both as a cache key and as the manifest
    /// reference in the registry URL (tag value or full `sha256:…` digest).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Tag(t) | Self::Digest(t) => t,
        }
    }
}

/// A parsed template reference: registry, repository, and pinned version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRef {
    /// Registry host, e.g. `ghcr.io`.
    pub registry: String,
    /// Repository path, e.g. `devcontainers/templates/rust`.
    pub repository: String,
    /// Pinned tag or digest.
    pub version: RefVersion,
}

/// Parse a template reference into its registry, repository, and version.
///
/// Resolution order mirrors the official CLI's `getRef`:
/// - A `@sha256:<hex>` suffix is parsed as a digest (everything before `@` is
///   the resource).
/// - Otherwise a trailing `:tag` (where the last `:` is after the last `/`, so
///   it isn't a registry port) is parsed as a tag.
/// - With neither, the version defaults to the `latest` tag.
///
/// # Errors
///
/// Returns [`TemplateError::RegistryError`] when the reference has no
/// `registry/repository` separator, or when a digest is malformed (wrong
/// `algorithm:hex` shape or an unsupported algorithm).
pub fn parse_template_ref(template_ref: &str) -> Result<TemplateRef, TemplateError> {
    let invalid = |message: &str| TemplateError::RegistryError {
        registry: template_ref.to_owned(),
        message: message.to_owned(),
    };

    let (base, version) = if let Some((resource, digest)) = template_ref.rsplit_once('@') {
        // Digest-pinned ref, e.g. ghcr.io/devcontainers/templates/rust@sha256:<hex>.
        let (algorithm, hex) = digest
            .split_once(':')
            .ok_or_else(|| invalid("invalid digest: expected format 'sha256:<hex>'"))?;
        if algorithm != "sha256" {
            return Err(invalid("unsupported digest algorithm: expected 'sha256'"));
        }
        if hex.is_empty() {
            return Err(invalid("invalid digest: empty hash"));
        }
        (resource, RefVersion::Digest(digest.to_owned()))
    } else {
        match template_ref.rsplit_once(':') {
            // A real tag: the `:` is after the last `/`, so it isn't a port.
            Some((b, t)) if !t.contains('/') && !t.is_empty() => (b, RefVersion::Tag(t.to_owned())),
            _ => (template_ref, RefVersion::Tag("latest".to_owned())),
        }
    };

    let (registry, repository) = base
        .split_once('/')
        .ok_or_else(|| invalid("invalid template reference: expected registry/repository[:tag]"))?;

    Ok(TemplateRef {
        registry: registry.to_owned(),
        repository: repository.to_owned(),
        version,
    })
}

/// Build an OCI [`oci_distribution::Client`] and [`oci_distribution::Reference`]
/// for the given parsed [`TemplateRef`].
///
/// The reference is constructed by digest when the version is pinned, otherwise
/// by tag — so a digest-pinned ref pulls the exact manifest rather than a tag.
///
/// Returns `(client, reference, auth)` ready for manifest or blob pulls.
pub fn build_oci_client(
    parsed: &TemplateRef,
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
    let registry = parsed.registry.clone();
    let repository = parsed.repository.clone();
    let oci_ref = match &parsed.version {
        RefVersion::Tag(tag) => Reference::with_tag(registry, repository, tag.clone()),
        RefVersion::Digest(digest) => Reference::with_digest(registry, repository, digest.clone()),
    };
    let auth = build_registry_auth(&parsed.registry);
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
        let parsed = parse_template_ref("ghcr.io/devcontainers/templates/rust:5.0.0").unwrap();
        assert_eq!(parsed.registry, "ghcr.io");
        assert_eq!(parsed.repository, "devcontainers/templates/rust");
        assert_eq!(parsed.version, RefVersion::Tag("5.0.0".to_owned()));
    }

    #[test]
    fn parse_ref_no_tag_defaults_to_latest() {
        let parsed = parse_template_ref("ghcr.io/devcontainers/templates/rust").unwrap();
        assert_eq!(parsed.registry, "ghcr.io");
        assert_eq!(parsed.repository, "devcontainers/templates/rust");
        assert_eq!(parsed.version, RefVersion::Tag("latest".to_owned()));
    }

    #[test]
    fn parse_ref_invalid_returns_error() {
        assert!(parse_template_ref("noslash").is_err());
    }

    #[test]
    fn parse_ref_tag_with_slash_treated_as_no_tag() {
        // A "tag" containing a slash is not a tag — treat as tagless (latest).
        let parsed = parse_template_ref("ghcr.io/devcontainers/templates/rust:repo/path").unwrap();
        assert_eq!(parsed.registry, "ghcr.io");
        assert_eq!(parsed.repository, "devcontainers/templates/rust:repo/path");
        assert_eq!(parsed.version, RefVersion::Tag("latest".to_owned()));
    }

    #[test]
    fn parse_digest_ref() {
        // Regression: a digest-pinned ref must not mis-parse `@sha256` into the
        // repository with the hex treated as a tag. The whole `sha256:<hex>`
        // becomes the version, and the manifest is later fetched by digest.
        let hex = "a".repeat(64);
        let input = format!("ghcr.io/devcontainers/templates/rust@sha256:{hex}");
        let parsed = parse_template_ref(&input).unwrap();
        assert_eq!(parsed.registry, "ghcr.io");
        assert_eq!(parsed.repository, "devcontainers/templates/rust");
        assert_eq!(parsed.version, RefVersion::Digest(format!("sha256:{hex}")));
    }

    #[test]
    fn parse_digest_ref_builds_reference_by_digest() {
        let hex = "b".repeat(64);
        let input = format!("ghcr.io/devcontainers/templates/rust@sha256:{hex}");
        let parsed = parse_template_ref(&input).unwrap();
        let (_, oci_ref, _) = build_oci_client(&parsed);
        assert_eq!(oci_ref.digest(), Some(format!("sha256:{hex}").as_str()));
        assert_eq!(oci_ref.tag(), None);
    }

    #[test]
    fn parse_tag_ref_builds_reference_by_tag() {
        let parsed = parse_template_ref("ghcr.io/devcontainers/templates/rust:5.0.0").unwrap();
        let (_, oci_ref, _) = build_oci_client(&parsed);
        assert_eq!(oci_ref.tag(), Some("5.0.0"));
        assert_eq!(oci_ref.digest(), None);
    }

    #[test]
    fn parse_digest_ref_rejects_non_sha256() {
        let input = "ghcr.io/devcontainers/templates/rust@sha512:abcdef";
        assert!(parse_template_ref(input).is_err());
    }

    #[test]
    fn parse_digest_ref_rejects_malformed_digest() {
        let input = "ghcr.io/devcontainers/templates/rust@notadigest";
        assert!(parse_template_ref(input).is_err());
    }

    #[test]
    fn version_as_str_returns_tag_or_digest() {
        assert_eq!(RefVersion::Tag("latest".to_owned()).as_str(), "latest");
        assert_eq!(
            RefVersion::Digest("sha256:abc".to_owned()).as_str(),
            "sha256:abc"
        );
    }
}
