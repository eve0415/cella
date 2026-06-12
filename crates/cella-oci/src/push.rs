//! OCI push helpers — upload blobs + manifests to an OCI registry.
//!
//! Designed for devcontainer artifact publishing (features and templates).
//! The push surface is kept generic: callers supply the layer bytes, media
//! types, per-layer annotations, and manifest-level annotations; this module
//! handles auth, the `oci-distribution` plumbing, and per-tag manifest PUT.

use std::collections::HashMap;

use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol, Config, ImageLayer};
use oci_distribution::errors::{OciDistributionError, OciErrorCode};
use oci_distribution::manifest::{OciImageManifest, OciManifest};
use tracing::debug;

use crate::build_registry_auth;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Descriptor for a single OCI layer to push.
pub struct LayerSpec {
    /// Raw bytes of the layer blob.
    pub data: Vec<u8>,
    /// OCI media type (e.g. `application/vnd.devcontainers.layer.v1+tar`).
    pub media_type: String,
    /// Per-layer annotations (`org.opencontainers.image.title`, etc.).
    pub annotations: Option<HashMap<String, String>>,
}

/// Result of a successful push operation.
#[derive(Debug, Clone)]
pub struct PushResult {
    /// The canonical digest of the pushed manifest (`sha256:…`).
    pub digest: String,
    /// The tags that were successfully pushed.
    pub pushed_tags: Vec<String>,
}

/// Error type for push operations.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("OCI push to {reference}:{tag} failed: {source}")]
    Push {
        reference: String,
        tag: String,
        source: OciDistributionError,
    },

    #[error("failed to list tags for {reference}: {source}")]
    ListTags {
        reference: String,
        source: OciDistributionError,
    },
}

// ---------------------------------------------------------------------------
// Tag listing
// ---------------------------------------------------------------------------

/// Fetch the existing tag list for a repository, returning an empty vec when
/// the repository does not yet exist (`NAME_UNKNOWN`) or on any 404-equivalent.
///
/// Used to implement skip-republish and semver tag fan-out.
///
/// # Errors
///
/// Returns [`PushError::ListTags`] if the registry returns an unexpected error
/// (anything other than `NAME_UNKNOWN` / 404).
pub async fn list_published_tags(
    registry: &str,
    repository: &str,
) -> Result<Vec<String>, PushError> {
    let client = new_client();
    let auth = build_registry_auth(registry);
    let oci_ref = Reference::with_tag(
        registry.to_owned(),
        repository.to_owned(),
        "latest".to_owned(),
    );

    match client.list_tags(&oci_ref, &auth, None, None).await {
        Ok(response) => {
            let tags = response.tags;
            debug!("listed {} tags for {registry}/{repository}", tags.len());
            Ok(tags)
        }
        Err(OciDistributionError::RegistryError { envelope, .. })
            if envelope
                .errors
                .iter()
                .any(|e| e.code == OciErrorCode::NameUnknown) =>
        {
            debug!("{registry}/{repository} not yet published (NAME_UNKNOWN)");
            Ok(Vec::new())
        }
        Err(source) => Err(PushError::ListTags {
            reference: format!("{registry}/{repository}"),
            source,
        }),
    }
}

// ---------------------------------------------------------------------------
// Artifact push
// ---------------------------------------------------------------------------

/// Push a single artifact (one or more layers) to `registry/repository` under
/// each of the given `tags`.
///
/// `config_media_type` is the OCI config media type (e.g.
/// `application/vnd.devcontainers`).  The config blob is always the two-byte
/// literal `{}` per the devcontainer spec.
///
/// `manifest_annotations` are written into the OCI manifest's `annotations`
/// map (e.g. `dev.containers.metadata`, `com.github.package.type`).
///
/// Returns the manifest digest and the list of tags that were pushed.
/// Returns an empty `PushResult` immediately when `tags` is empty.
///
/// # Errors
///
/// Returns [`PushError::Push`] if the registry rejects the blob or manifest
/// upload for any tag.
pub async fn push_artifact<S: std::hash::BuildHasher>(
    registry: &str,
    repository: &str,
    tags: &[String],
    layers: Vec<LayerSpec>,
    config_media_type: &str,
    manifest_annotations: Option<HashMap<String, String, S>>,
) -> Result<PushResult, PushError> {
    if tags.is_empty() {
        return Ok(PushResult {
            digest: String::new(),
            pushed_tags: Vec::new(),
        });
    }

    let client = new_client();
    let auth = build_registry_auth(registry);

    let image_layers: Vec<ImageLayer> = layers
        .into_iter()
        .map(|l| ImageLayer::new(l.data, l.media_type, l.annotations))
        .collect();

    // The config blob for devcontainer artifacts is always `{}`.
    let config = Config::new(b"{}".to_vec(), config_media_type.to_owned(), None);

    // OciImageManifest::build requires the default hasher; collect into a plain HashMap.
    let annotations_default: Option<HashMap<String, String>> =
        manifest_annotations.map(|m| m.into_iter().collect());
    let manifest = OciImageManifest::build(&image_layers, &config, annotations_default);

    // Push under the first tag — this uploads all blobs and the manifest.
    let first_tag = &tags[0];
    let first_ref = Reference::with_tag(
        registry.to_owned(),
        repository.to_owned(),
        first_tag.clone(),
    );

    debug!("pushing {registry}/{repository}:{first_tag}");

    let push_response = client
        .push(
            &first_ref,
            &image_layers,
            config,
            &auth,
            Some(manifest.clone()),
        )
        .await
        .map_err(|source| PushError::Push {
            reference: format!("{registry}/{repository}"),
            tag: first_tag.clone(),
            source,
        })?;

    let digest = extract_digest_from_url(&push_response.manifest_url);
    debug!("pushed {registry}/{repository}:{first_tag} (digest={digest})");

    let mut pushed_tags = vec![first_tag.clone()];

    // Re-tag by pushing only the manifest for subsequent tags.
    // Blobs are already present in the registry from the first push.
    let oci_manifest = OciManifest::Image(manifest);
    for tag in tags.iter().skip(1) {
        let tag_ref = Reference::with_tag(registry.to_owned(), repository.to_owned(), tag.clone());
        debug!("tagging {registry}/{repository}:{tag}");
        client
            .push_manifest(&tag_ref, &oci_manifest)
            .await
            .map_err(|source| PushError::Push {
                reference: format!("{registry}/{repository}"),
                tag: tag.clone(),
                source,
            })?;
        pushed_tags.push(tag.clone());
    }

    Ok(PushResult {
        digest,
        pushed_tags,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn new_client() -> oci_distribution::Client {
    oci_distribution::Client::new(ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    })
}

/// Extract `sha256:<hex>` from a pullable URL like `registry/repo@sha256:abc`.
fn extract_digest_from_url(url: &str) -> String {
    url.find("sha256:")
        .map_or_else(|| url.to_owned(), |pos| url[pos..].to_owned())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_digest_from_well_formed_url() {
        let url = "ghcr.io/owner/repo@sha256:abc123def456";
        assert_eq!(extract_digest_from_url(url), "sha256:abc123def456");
    }

    #[test]
    fn extract_digest_from_url_without_digest_is_identity() {
        let url = "ghcr.io/owner/repo:latest";
        assert_eq!(extract_digest_from_url(url), url);
    }

    #[test]
    fn push_artifact_returns_empty_result_for_empty_tags() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt
            .block_on(push_artifact(
                "ghcr.io",
                "owner/repo",
                &[],
                Vec::new(),
                "application/vnd.devcontainers",
                None::<HashMap<String, String>>,
            ))
            .unwrap();
        assert!(result.pushed_tags.is_empty());
        assert!(result.digest.is_empty());
    }
}
