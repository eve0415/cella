//! OCI manifest inspection helpers.
//!
//! Provides a thin wrapper around `oci_distribution` to fetch a manifest
//! and return it as a raw JSON value together with the resolved sha256 digest.

use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::errors::OciDistributionError;
use tracing::debug;

use crate::build_registry_auth;

/// Number of tags to request per registry page when listing tags.
const TAG_PAGE_SIZE: usize = 100;

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
    let (registry, repository, version) = parse_reference(reference)?;

    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    };
    let client = oci_distribution::Client::new(config);

    let oci_ref = match &version {
        ReferenceVersion::Tag(tag) => {
            Reference::with_tag(registry.clone(), repository.clone(), tag.clone())
        }
        ReferenceVersion::Digest(digest) => {
            Reference::with_digest(registry.clone(), repository.clone(), digest.clone())
        }
    };
    let auth = build_registry_auth(&registry);

    debug!("fetching manifest for {registry}/{repository} ({version:?})");

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

    debug!("fetched manifest for {registry}/{repository} ({version:?}, digest={hex_digest})");

    Ok((json_value, hex_digest))
}

/// Fetch **all** published tags for an OCI reference, paginating as needed.
///
/// The reference must include at least `registry/repository` — the tag
/// component is ignored (a synthetic `"latest"` tag is used internally to
/// target the repository namespace).
///
/// ## Pagination contract
///
/// `oci_distribution`'s `list_tags(ref, auth, n, last)` maps to the OCI
/// Distribution Spec's `GET /v2/<name>/tags/list?n=<n>&last=<last>` endpoint.
/// `TagResponse` has no cursor field — pagination is driven by passing the
/// last tag name from the previous page as the `last` parameter on the next
/// call.
///
/// Some registries (e.g. GHCR) return `{"tags": null}` on the final page
/// instead of an empty array, which causes `oci_distribution`'s
/// `TagResponse { tags: Vec<String> }` to produce a deserialization error.
/// We avoid triggering that path by stopping as soon as a page returns fewer
/// tags than [`TAG_PAGE_SIZE`] — a partial page always means end-of-list.
///
/// # Errors
///
/// Returns an error when the reference cannot be parsed or any registry
/// request fails.
pub async fn fetch_published_tags(reference: &str) -> miette::Result<Vec<String>> {
    let (registry, repository, _version) = parse_reference(reference)?;

    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    };
    let client = oci_distribution::Client::new(config);

    let oci_ref = Reference::with_tag(registry.clone(), repository.clone(), "latest".to_owned());
    let auth = build_registry_auth(&registry);

    debug!("listing tags for {registry}/{repository}");

    let mut all_tags: Vec<String> = Vec::new();
    let mut last: Option<String> = None;

    loop {
        let response = match client
            .list_tags(&oci_ref, &auth, Some(TAG_PAGE_SIZE), last.as_deref())
            .await
        {
            Ok(response) => response,
            // A follow-up page can fail on registries that answer the final
            // page with `{"tags": null}` (e.g. GHCR when the previous page was
            // exactly full): the null body fails JSON deserialization. Treat
            // *only* that case as end-of-list. Network/auth/registry errors
            // must propagate — otherwise a transient failure on page 2+ would
            // silently truncate the listing and look like a complete result.
            Err(OciDistributionError::JsonError(_)) if !all_tags.is_empty() => {
                debug!("treating null-tags deserialization on follow-up page as end-of-list");
                break;
            }
            Err(e) => {
                return Err(miette::miette!("failed to list tags for {reference}: {e}"));
            }
        };

        let page_len = response.tags.len();
        last = response.tags.last().cloned();
        all_tags.extend(response.tags);

        // A partial page means we've reached the end. Avoid making a
        // follow-up request that would trigger the null-tags deserialization
        // bug in some registries (including GHCR).
        if page_len < TAG_PAGE_SIZE {
            break;
        }
    }

    Ok(all_tags)
}

/// The version component of an OCI reference: a tag or a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceVersion {
    /// A named tag, e.g. `1` or `latest`.
    Tag(String),
    /// A content digest, e.g. `sha256:<hex>`.
    Digest(String),
}

/// Parse a feature OCI reference into `(registry, repository, version)`.
///
/// Accepts `registry/[namespace/]name:tag`, `registry/[namespace/]name@digest`,
/// or `registry/[namespace/]name` (defaulting the tag to `"latest"`).
///
/// # Errors
///
/// Returns an error when the reference has no `/` separator (i.e., it is
/// not a registry-qualified reference).
pub fn parse_reference(reference: &str) -> miette::Result<(String, String, ReferenceVersion)> {
    // Split registry from the rest on the first `/`.
    let (registry, rest) = reference.split_once('/').ok_or_else(|| {
        miette::miette!("invalid OCI reference (expected registry/repo): {reference}")
    })?;

    // A digest reference uses `@` (e.g. `name@sha256:<hex>`); check it first
    // because the digest itself contains a `:`.
    let (repository, version) = rest.rsplit_once('@').map_or_else(
        || {
            // Split repository and tag on the last `:` in `rest`.
            rest.rsplit_once(':').map_or_else(
                || (rest.to_owned(), ReferenceVersion::Tag("latest".to_owned())),
                |(repo, t)| (repo.to_owned(), ReferenceVersion::Tag(t.to_owned())),
            )
        },
        |(repo, digest)| (repo.to_owned(), ReferenceVersion::Digest(digest.to_owned())),
    );

    Ok((registry.to_owned(), repository, version))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reference_with_tag() {
        let (reg, repo, version) =
            parse_reference("ghcr.io/devcontainers/features/node:1").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/features/node");
        assert_eq!(version, ReferenceVersion::Tag("1".to_owned()));
    }

    #[test]
    fn parse_reference_without_tag_defaults_latest() {
        let (reg, repo, version) = parse_reference("ghcr.io/devcontainers/features/node").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/features/node");
        assert_eq!(version, ReferenceVersion::Tag("latest".to_owned()));
    }

    #[test]
    fn parse_reference_with_digest() {
        let (reg, repo, version) =
            parse_reference("ghcr.io/devcontainers/features/node@sha256:abc123").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/features/node");
        assert_eq!(
            version,
            ReferenceVersion::Digest("sha256:abc123".to_owned())
        );
    }

    #[test]
    fn parse_reference_no_slash_errors() {
        assert!(parse_reference("not-a-valid-ref").is_err());
    }

    #[test]
    fn parse_reference_deep_path() {
        let (reg, repo, version) =
            parse_reference("mcr.microsoft.com/devcontainers/base:ubuntu").unwrap();
        assert_eq!(reg, "mcr.microsoft.com");
        assert_eq!(repo, "devcontainers/base");
        assert_eq!(version, ReferenceVersion::Tag("ubuntu".to_owned()));
    }
}
