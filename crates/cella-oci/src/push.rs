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
use sha2::{Digest, Sha256};
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

    #[error("failed to serialize manifest for digest computation: {0}")]
    ManifestSerialize(serde_json::Error),
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
/// Returns `Some(PushResult)` with the manifest digest and pushed tags, or
/// `None` when `tags` is empty (nothing to push).
///
/// # Errors
///
/// Returns [`PushError`] if manifest serialization fails or if the registry
/// rejects the blob or manifest upload for any tag.
pub async fn push_artifact<S: std::hash::BuildHasher>(
    registry: &str,
    repository: &str,
    tags: &[String],
    layers: Vec<LayerSpec>,
    config_media_type: &str,
    manifest_annotations: Option<HashMap<String, String, S>>,
) -> Result<Option<PushResult>, PushError> {
    if tags.is_empty() {
        return Ok(None);
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

    // Compute the manifest digest from the canonical JSON bytes that
    // oci-distribution will PUT to the registry.  The crate uses
    // `olpc_cjson::CanonicalFormatter` (OCI canonical JSON — sorted keys, no
    // extra whitespace), so we must use the same serializer here; a plain
    // `serde_json::to_vec` would produce a different byte sequence and a wrong
    // digest.
    let digest = canonical_manifest_digest(&manifest)?;

    // Push under the first tag — this uploads all blobs and the manifest.
    let first_tag = &tags[0];
    let first_ref = Reference::with_tag(
        registry.to_owned(),
        repository.to_owned(),
        first_tag.clone(),
    );

    debug!("pushing {registry}/{repository}:{first_tag}");

    client
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

    Ok(Some(PushResult {
        digest,
        pushed_tags,
    }))
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

/// Serialize `manifest` using OCI canonical JSON (the same formatter that
/// `oci-distribution` uses before the registry PUT) and return the digest as
/// `sha256:<hex>`.
///
/// # Errors
///
/// Returns [`PushError::ManifestSerialize`] if `serde_json` cannot serialise
/// the manifest struct (should never happen in practice).
fn canonical_manifest_digest(manifest: &OciImageManifest) -> Result<String, PushError> {
    let mut body = Vec::new();
    let mut ser =
        serde_json::Serializer::with_formatter(&mut body, olpc_cjson::CanonicalFormatter::new());
    serde::Serialize::serialize(manifest, &mut ser).map_err(PushError::ManifestSerialize)?;
    let hash = Sha256::digest(&body);
    Ok(format!("sha256:{}", hex::encode(hash)))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_manifest_digest_is_stable() {
        // Build a minimal manifest and verify the digest is deterministic and
        // has the right format.
        let layers = vec![ImageLayer::new(
            b"hello".to_vec(),
            "application/vnd.devcontainers.layer.v1+tar".to_owned(),
            None,
        )];
        let config = Config::new(
            b"{}".to_vec(),
            "application/vnd.devcontainers".to_owned(),
            None,
        );
        let manifest = OciImageManifest::build(&layers, &config, None);

        let d1 = canonical_manifest_digest(&manifest).unwrap();
        let d2 = canonical_manifest_digest(&manifest).unwrap();
        assert_eq!(d1, d2, "digest must be deterministic");
        assert!(d1.starts_with("sha256:"), "digest must start with sha256:");
        assert_eq!(d1.len(), "sha256:".len() + 64, "sha256 hex is 64 chars");
    }

    #[test]
    fn push_artifact_returns_none_for_empty_tags() {
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
        assert!(result.is_none(), "empty tags must yield None");
    }
}
