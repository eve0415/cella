//! OCI registry fetcher for devcontainer features.
//!
//! Pulls feature artifacts from OCI-compliant registries (e.g. `ghcr.io`),
//! extracts the gzipped tarball layer, and caches the result on disk.

use std::future::Future;
use std::path::PathBuf;

use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::secrets::RegistryAuth;
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::cache::FeatureCache;
use crate::reference::NormalizedRef;
use crate::{FeatureError, Platform};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Trait for fetching feature artifacts from a remote source.
///
/// Uses native `-> impl Future` syntax (edition 2024) instead of the
/// `async-trait` crate.
pub trait FeatureFetcher: Send + Sync {
    /// Fetch the feature identified by `reference` for the given `platform`,
    /// returning the local filesystem path where the extracted contents live.
    fn fetch(
        &self,
        reference: &NormalizedRef,
        platform: &Platform,
        cache: &FeatureCache,
    ) -> impl Future<Output = Result<PathBuf, FeatureError>> + Send;
}

// ---------------------------------------------------------------------------
// OCI fetcher
// ---------------------------------------------------------------------------

/// Fetches devcontainer features from OCI registries.
///
/// Wraps [`oci_distribution::Client`] and implements the [`FeatureFetcher`]
/// trait.  Handles authentication, manifest resolution (both OCI and Docker
/// manifest media types), layer download, gzip extraction, and atomic cache
/// commits.
pub struct OciFetcher {
    client: oci_distribution::Client,
}

impl OciFetcher {
    /// Create a new fetcher with default HTTPS transport.
    pub fn new() -> Self {
        let config = ClientConfig {
            protocol: ClientProtocol::Https,
            ..ClientConfig::default()
        };
        Self {
            client: oci_distribution::Client::new(config),
        }
    }
}

impl Default for OciFetcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the first extractable layer in a manifest, or return an error listing available types.
fn find_feature_layer<'a>(
    manifest: &'a oci_distribution::manifest::OciImageManifest,
    registry: &str,
    repository: &str,
    tag: &str,
) -> Result<&'a oci_distribution::manifest::OciDescriptor, FeatureError> {
    manifest
        .layers
        .iter()
        .find(|l| is_extractable_layer(&l.media_type))
        .ok_or_else(|| FeatureError::InvalidArtifact {
            feature_id: format!("{registry}/{repository}:{tag}"),
            reason: format!(
                "no extractable layer found in manifest; layer media types: [{}]",
                manifest
                    .layers
                    .iter()
                    .map(|l| l.media_type.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
}

/// Verify that a blob matches its expected OCI digest (e.g. `sha256:<hex>`).
fn verify_blob_digest(
    blob: &[u8],
    expected_digest: &str,
    repository: &str,
) -> Result<(), FeatureError> {
    let actual_digest = format!("sha256:{}", hex::encode(Sha256::digest(blob)));
    if actual_digest != expected_digest {
        return Err(FeatureError::DigestMismatch {
            feature_id: repository.to_owned(),
            expected: expected_digest.to_owned(),
            actual: actual_digest,
        });
    }
    debug!("blob digest verified: {expected_digest}");
    Ok(())
}

/// Extract a layer blob to a staging directory and atomically commit to the cache.
fn extract_and_commit(
    blob: &[u8],
    media_type: &str,
    cache: &FeatureCache,
    registry: &str,
    repository: &str,
    tag: &str,
    digest: &str,
) -> Result<PathBuf, FeatureError> {
    let final_path = cache.oci_path(registry, repository, digest);
    let staging = FeatureCache::staging_path(&final_path);

    std::fs::create_dir_all(&staging).map_err(|e| FeatureError::RegistryError {
        registry: registry.to_owned(),
        message: format!("failed to create staging directory: {e}"),
    })?;

    extract_layer(blob, media_type, &staging).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        FeatureError::InvalidArtifact {
            feature_id: format!("{registry}/{repository}:{tag}"),
            reason: format!("failed to extract layer: {e}"),
        }
    })?;

    FeatureCache::commit(&staging, &final_path).map_err(|e| FeatureError::RegistryError {
        registry: registry.to_owned(),
        message: format!("failed to commit cache entry: {e}"),
    })?;

    debug!(
        "cached {registry}/{repository}:{tag} at {}",
        final_path.display()
    );
    Ok(final_path)
}

impl OciFetcher {
    /// Pull the manifest and resolve the digest for the given reference.
    async fn pull_manifest(
        &self,
        oci_ref: &Reference,
        auth: &RegistryAuth,
        registry: &str,
        repository: &str,
        tag: &str,
    ) -> Result<(oci_distribution::manifest::OciImageManifest, String), FeatureError> {
        let (manifest, digest) = self
            .client
            .pull_image_manifest(oci_ref, auth)
            .await
            .map_err(|e| FeatureError::RegistryError {
                registry: registry.to_owned(),
                message: format!("failed to pull manifest for {repository}:{tag}: {e}"),
            })?;

        debug!(
            "pulled manifest for {registry}/{repository}:{tag} (digest={digest}, layers={})",
            manifest.layers.len()
        );
        Ok((manifest, digest))
    }

    /// Download a single layer blob from the registry and verify its digest.
    async fn pull_layer_blob(
        &self,
        oci_ref: &Reference,
        layer: &oci_distribution::manifest::OciDescriptor,
        registry: &str,
    ) -> Result<Vec<u8>, FeatureError> {
        let capacity = usize::try_from(layer.size.max(0)).unwrap_or(0);
        let mut blob = Vec::with_capacity(capacity);
        self.client
            .pull_blob(oci_ref, layer, &mut blob)
            .await
            .map_err(|e| FeatureError::RegistryError {
                registry: registry.to_owned(),
                message: format!("failed to pull layer blob: {e}"),
            })?;
        debug!(
            "downloaded layer blob ({} bytes, media_type={})",
            blob.len(),
            layer.media_type
        );

        // Verify blob integrity against the manifest digest.
        verify_blob_digest(&blob, &layer.digest, oci_ref.repository())?;

        Ok(blob)
    }
}

impl FeatureFetcher for OciFetcher {
    async fn fetch(
        &self,
        reference: &NormalizedRef,
        _platform: &Platform,
        cache: &FeatureCache,
    ) -> Result<PathBuf, FeatureError> {
        let NormalizedRef::OciTarget {
            registry,
            repository,
            tag,
        } = reference
        else {
            return Err(FeatureError::InvalidReference {
                reference: reference.to_string(),
                reason: "OciFetcher only handles OCI targets".to_owned(),
            });
        };

        if let Some(cached) = cache.get_oci(registry, repository, tag) {
            debug!("cache hit for {registry}/{repository}:{tag}");
            return Ok(cached);
        }

        let oci_ref = Reference::with_tag(registry.clone(), repository.clone(), tag.clone());
        let auth = build_registry_auth(registry);

        let (manifest, digest) = self
            .pull_manifest(&oci_ref, &auth, registry, repository, tag)
            .await?;

        if let Some(cached) = cache.get_oci(registry, repository, &digest) {
            debug!("cache hit by digest {digest}");
            return Ok(cached);
        }

        let layer = find_feature_layer(&manifest, registry, repository, tag)?;
        let blob = self.pull_layer_blob(&oci_ref, layer, registry).await?;

        extract_and_commit(
            &blob,
            &layer.media_type,
            cache,
            registry,
            repository,
            tag,
            &digest,
        )
    }
}

// ---------------------------------------------------------------------------
// Platform detection
// ---------------------------------------------------------------------------

/// Build a [`Platform`] from raw OS and architecture strings.
///
/// Normalises the architecture to Go/OCI conventions (`amd64`, `arm64`, etc.).
/// Callers obtain the raw values from [`ContainerBackend::detect_platform()`].
pub fn detect_platform(os: &str, arch: &str) -> Platform {
    let architecture = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };

    Platform {
        os: os.to_string(),
        architecture: architecture.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use cella_oci::{build_registry_auth, extract_layer, is_extractable_layer};

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "integration-tests")]
    use crate::test_utils::test_platform;

    #[test]
    fn verify_blob_digest_match_passes() {
        let data = b"test blob data";
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(data)));
        assert!(verify_blob_digest(data, &digest, "test/repo").is_ok());
    }

    #[test]
    fn verify_blob_digest_mismatch_fails() {
        let data = b"test blob data";
        let wrong_digest =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let result = verify_blob_digest(data, wrong_digest, "test/repo");
        assert!(
            matches!(result, Err(FeatureError::DigestMismatch { .. })),
            "expected DigestMismatch, got {result:?}",
        );
    }

    // -----------------------------------------------------------------------
    // Integration test -- requires network access
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[cfg(feature = "integration-tests")]
    async fn fetch_node_feature_from_ghcr() {
        let fetcher = OciFetcher::new();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let reference = NormalizedRef::OciTarget {
            registry: "ghcr.io".to_owned(),
            repository: "devcontainers/features/node".to_owned(),
            tag: "1".to_owned(),
        };

        let platform = test_platform();

        let path = fetcher.fetch(&reference, &platform, &cache).await.unwrap();

        // The extracted feature must contain these files.
        assert!(
            path.join("devcontainer-feature.json").exists(),
            "devcontainer-feature.json should exist at {}",
            path.display()
        );
        assert!(
            path.join("install.sh").exists(),
            "install.sh should exist at {}",
            path.display()
        );

        // Fetching again should hit the cache.
        let path2 = fetcher.fetch(&reference, &platform, &cache).await.unwrap();
        assert_eq!(path, path2, "second fetch should return cached path");
    }

    #[tokio::test]
    #[cfg(feature = "integration-tests")]
    async fn fetch_github_cli_feature_from_ghcr() {
        let fetcher = OciFetcher::new();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let reference = NormalizedRef::OciTarget {
            registry: "ghcr.io".to_owned(),
            repository: "devcontainers/features/github-cli".to_owned(),
            tag: "1".to_owned(),
        };

        let platform = test_platform();

        let path = fetcher.fetch(&reference, &platform, &cache).await.unwrap();

        assert!(
            path.join("devcontainer-feature.json").exists(),
            "devcontainer-feature.json should exist at {}",
            path.display()
        );
        assert!(
            path.join("install.sh").exists(),
            "install.sh should exist at {}",
            path.display()
        );

        // Second fetch should hit cache.
        let path2 = fetcher.fetch(&reference, &platform, &cache).await.unwrap();
        assert_eq!(path, path2, "second fetch should return cached path");
    }
}
