//! OCI manifest inspection helpers.
//!
//! Provides a thin wrapper around `oci_client` to fetch a manifest
//! and return it as a raw JSON value together with the resolved sha256 digest.

use oci_client::Reference;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::errors::OciDistributionError;
use tracing::debug;

use crate::build_registry_auth;

/// Number of tags to request per registry page when listing tags.
const TAG_PAGE_SIZE: usize = 100;

/// Registry requests are bounded so a hung endpoint cannot hang the CLI.
/// `oci-client` defaults both of these to `None`.
const REGISTRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Build a registry client with both timeouts set.
fn registry_client() -> oci_client::Client {
    oci_client::Client::new(ClientConfig {
        protocol: ClientProtocol::Https,
        read_timeout: Some(REGISTRY_TIMEOUT),
        connect_timeout: Some(REGISTRY_TIMEOUT),
        ..ClientConfig::default()
    })
}

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

    let client = registry_client();

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
/// `oci_client`'s `list_tags(ref, auth, n, last)` maps to the OCI
/// Distribution Spec's `GET /v2/<name>/tags/list?n=<n>&last=<last>` endpoint.
/// `TagResponse` has no cursor field — pagination is driven by passing the
/// last tag name from the previous page as the `last` parameter on the next
/// call.
///
/// Some registries (e.g. GHCR) return `{"tags": null}` on the final page
/// instead of an empty array, which causes `oci_client`'s
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

    let client = registry_client();

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
            Err(e) => return Err(crate::TagListError::new(reference, &e).into()),
        };

        let page_len = response.tags.len();
        let next_last = response.tags.last().cloned();
        let done = is_final_page(page_len, last.as_deref(), next_last.as_deref());
        last = next_last;
        all_tags.extend(response.tags);

        if done {
            break;
        }
    }

    Ok(all_tags)
}

/// Expand a Docker Hub shorthand into a fully qualified reference.
///
/// A first segment containing `.` or `:`, or the literal `localhost`, is
/// treated as a registry host and left alone — this is the same rule the
/// Docker CLI uses.
///
/// This is applied on the image path only. [`parse_reference`] is also
/// reached with user-supplied *feature* references, which the devcontainer
/// spec requires to be registry-qualified; there its "expected registry/repo"
/// error is the useful diagnostic, and normalizing would turn a typo into a
/// confusing Docker Hub 404.
pub fn normalize_reference(reference: &str) -> String {
    let first = reference.split('/').next().unwrap_or(reference);
    let is_registry = first == "localhost" || first.contains('.') || first.contains(':');

    // The `contains('/')` guard matters: `ubuntu:24.04`'s first segment
    // contains `:` but is a repository, not a host.
    if reference.contains('/') {
        if is_registry {
            return reference.to_owned();
        }
        return format!("docker.io/{reference}");
    }
    format!("docker.io/library/{reference}")
}

/// Whether a tag page is the last one worth asking for.
///
/// Three ways a listing ends:
///
/// - **Short page** — the normal end. Stopping here also avoids a follow-up
///   request that would trip the `{"tags": null}` deserialization bug some
///   registries (including GHCR) hit on the final page.
/// - **Over-full page** — the registry ignored `?n=` and answered with the
///   whole list. MCR does this: `devcontainers/rust` returns all 408 tags
///   however small an `n` you ask for.
/// - **Stalled cursor** — the registry ignored `?last=`, so the next request
///   would return the same page forever. MCR does this too.
///
/// Without the last two, listing an MCR repository never terminates.
fn is_final_page(page_len: usize, previous_last: Option<&str>, new_last: Option<&str>) -> bool {
    page_len != TAG_PAGE_SIZE || new_last.is_none() || previous_last == new_last
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
    fn a_registry_that_ignores_pagination_still_terminates() {
        // Regression: MCR ignores both `?n=` and `?last=` and answers every
        // request with the full list (408 tags for devcontainers/rust). The
        // old `page_len < TAG_PAGE_SIZE` exit could never fire, so listing
        // MCR tags looped forever.
        assert!(
            is_final_page(408, None, Some("trixie")),
            "an over-full page means the registry ignored `n`"
        );
        // ...and even at exactly the page size, a cursor that does not move
        // is the end of the road.
        assert!(
            is_final_page(TAG_PAGE_SIZE, Some("trixie"), Some("trixie")),
            "a stalled cursor must end the listing"
        );
    }

    #[test]
    fn a_paginating_registry_keeps_going_until_a_short_page() {
        assert!(
            !is_final_page(TAG_PAGE_SIZE, Some("a"), Some("b")),
            "a full page with an advancing cursor has more to come"
        );
        assert!(
            is_final_page(7, Some("a"), Some("b")),
            "a short page ends it"
        );
        assert!(is_final_page(0, None, None), "an empty page ends it");
    }

    #[test]
    fn normalizes_bare_docker_hub_references() {
        assert_eq!(
            normalize_reference("ubuntu:24.04"),
            "docker.io/library/ubuntu:24.04"
        );
        assert_eq!(
            normalize_reference("node:22-bookworm"),
            "docker.io/library/node:22-bookworm"
        );
        assert_eq!(normalize_reference("myorg/img:1"), "docker.io/myorg/img:1");
        assert_eq!(
            normalize_reference("mcr.microsoft.com/devcontainers/rust:2.0.14-trixie"),
            "mcr.microsoft.com/devcontainers/rust:2.0.14-trixie"
        );
        assert_eq!(
            normalize_reference("localhost:5000/img:1"),
            "localhost:5000/img:1"
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
