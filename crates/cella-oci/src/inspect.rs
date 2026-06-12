//! OCI manifest inspection helpers.
//!
//! Provides a thin wrapper around `oci_distribution` to fetch a manifest
//! and return it as a raw JSON value together with the resolved sha256 digest.

use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};
use tracing::debug;

use crate::build_registry_auth;

/// Fetch the OCI manifest for a feature reference and return the manifest JSON
/// together with its sha256 digest hex string (without the `sha256:` prefix).
///
/// `reference` must be a fully-qualified OCI reference in one of these forms:
/// - `registry/repo/name:tag`
/// - `registry/repo/name@sha256:<hex>`
///
/// # Errors
///
/// Returns an error wrapped via [`miette`] when the reference cannot be
/// parsed, when the registry is unreachable, or when the manifest response
/// cannot be decoded.
pub async fn fetch_manifest_with_digest(
    reference: &str,
) -> miette::Result<(serde_json::Value, String)> {
    let (registry, repository, tag) = parse_reference(reference)?;

    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    };
    let client = oci_distribution::Client::new(config);

    let oci_ref = Reference::with_tag(registry.clone(), repository.clone(), tag.clone());
    let auth = build_registry_auth(&registry);

    debug!("fetching manifest for {registry}/{repository}:{tag}");

    let (manifest, digest) = client
        .pull_image_manifest(&oci_ref, &auth)
        .await
        .map_err(|e| miette::miette!("failed to fetch manifest for {reference}: {e}"))?;

    // Strip the "sha256:" prefix if present to return only the hex portion.
    let hex_digest = digest
        .strip_prefix("sha256:")
        .map(str::to_owned)
        .unwrap_or(digest);

    let json_value = serde_json::to_value(&manifest)
        .map_err(|e| miette::miette!("failed to serialize manifest: {e}"))?;

    debug!("fetched manifest for {registry}/{repository}:{tag} (digest={hex_digest})");

    Ok((json_value, hex_digest))
}

/// Fetch all published tags for an OCI reference.
///
/// The reference must include at least `registry/repository` — the tag
/// component is ignored (a synthetic `"latest"` tag is used internally to
/// target the repository namespace).
///
/// # Errors
///
/// Returns an error when the reference cannot be parsed or the registry
/// request fails.
pub async fn fetch_published_tags(reference: &str) -> miette::Result<Vec<String>> {
    let (registry, repository, _tag) = parse_reference(reference)?;

    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    };
    let client = oci_distribution::Client::new(config);

    let oci_ref = Reference::with_tag(registry.clone(), repository.clone(), "latest".to_owned());
    let auth = build_registry_auth(&registry);

    debug!("listing tags for {registry}/{repository}");

    let response = client
        .list_tags(&oci_ref, &auth, None, None)
        .await
        .map_err(|e| miette::miette!("failed to list tags for {reference}: {e}"))?;

    Ok(response.tags)
}

/// Parse a feature OCI reference into `(registry, repository, tag)`.
///
/// Accepts `registry/[namespace/]name:tag` or `registry/[namespace/]name`
/// (defaulting the tag to `"latest"`).
///
/// # Errors
///
/// Returns an error when the reference has no `/` separator (i.e., it is
/// not a registry-qualified reference).
pub fn parse_reference(reference: &str) -> miette::Result<(String, String, String)> {
    // Split registry from the rest on the first `/`.
    let (registry, rest) = reference.split_once('/').ok_or_else(|| {
        miette::miette!("invalid OCI reference (expected registry/repo): {reference}")
    })?;

    // Split repository and tag on the last `:` in `rest`.
    let (repository, tag) = rest.rsplit_once(':').map_or_else(
        || (rest.to_owned(), "latest".to_owned()),
        |(repo, t)| (repo.to_owned(), t.to_owned()),
    );

    Ok((registry.to_owned(), repository, tag))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reference_with_tag() {
        let (reg, repo, tag) = parse_reference("ghcr.io/devcontainers/features/node:1").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/features/node");
        assert_eq!(tag, "1");
    }

    #[test]
    fn parse_reference_without_tag_defaults_latest() {
        let (reg, repo, tag) = parse_reference("ghcr.io/devcontainers/features/node").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/features/node");
        assert_eq!(tag, "latest");
    }

    #[test]
    fn parse_reference_no_slash_errors() {
        assert!(parse_reference("not-a-valid-ref").is_err());
    }

    #[test]
    fn parse_reference_deep_path() {
        let (reg, repo, tag) =
            parse_reference("mcr.microsoft.com/devcontainers/base:ubuntu").unwrap();
        assert_eq!(reg, "mcr.microsoft.com");
        assert_eq!(repo, "devcontainers/base");
        assert_eq!(tag, "ubuntu");
    }
}
