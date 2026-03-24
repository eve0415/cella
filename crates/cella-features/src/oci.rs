//! OCI registry fetcher for devcontainer features.
//!
//! Pulls feature artifacts from OCI-compliant registries (e.g. `ghcr.io`),
//! extracts the gzipped tarball layer, and caches the result on disk.

use std::future::Future;
use std::path::PathBuf;

use flate2::read::GzDecoder;
use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::manifest::{
    IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
};
use oci_distribution::secrets::RegistryAuth;
use tracing::{debug, warn};

use crate::auth::resolve_credentials;
use crate::cache::FeatureCache;
use crate::reference::NormalizedRef;
use crate::{FeatureError, Platform};

/// Media type for devcontainer feature layers (plain tar).
const DEVCONTAINERS_LAYER_MEDIA_TYPE: &str = "application/vnd.devcontainers.layer.v1+tar";

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

    /// Download a single layer blob from the registry.
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

/// Detect the target platform from the Docker daemon.
///
/// Queries `docker version` via the bollard client and normalises the
/// architecture string to Go/OCI conventions (`amd64`, `arm64`, etc.).
///
/// # Errors
///
/// Returns [`FeatureError::RegistryError`] if the Docker daemon is
/// unreachable or the version query fails.
pub async fn detect_platform(docker: &bollard::Docker) -> Result<Platform, FeatureError> {
    let version = docker
        .version()
        .await
        .map_err(|e| FeatureError::RegistryError {
            registry: "docker".to_string(),
            message: format!("failed to detect platform: {e}"),
        })?;

    let os = version.os.unwrap_or_else(|| "linux".to_string());
    let arch = version.arch.unwrap_or_else(|| "amd64".to_string());

    let architecture = match arch.as_str() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };

    Ok(Platform {
        os,
        architecture: architecture.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build [`RegistryAuth`] from Docker credential store for the given registry.
fn build_registry_auth(registry: &str) -> RegistryAuth {
    let creds = resolve_credentials(registry);
    if let (Some(u), Some(p)) = (creds.username, creds.password) {
        debug!("using basic auth for {registry}");
        RegistryAuth::Basic(u, p)
    } else {
        debug!("no credentials for {registry}; using anonymous auth");
        RegistryAuth::Anonymous
    }
}

/// Returns `true` when the media type indicates a layer we can extract.
fn is_extractable_layer(media_type: &str) -> bool {
    matches!(
        media_type,
        IMAGE_LAYER_GZIP_MEDIA_TYPE
            | IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE
            | IMAGE_LAYER_MEDIA_TYPE
            | DEVCONTAINERS_LAYER_MEDIA_TYPE
    ) || media_type.contains("tar+gzip")
        || media_type.contains("tar.gzip")
}

/// Extract a layer blob (gzip tarball or plain tar) into `dest`.
fn extract_layer(blob: &[u8], media_type: &str, dest: &std::path::Path) -> std::io::Result<()> {
    let is_gzip = blob.len() >= 2 && blob[0] == 0x1f && blob[1] == 0x8b;

    if media_type.contains("gzip") || media_type == IMAGE_LAYER_GZIP_MEDIA_TYPE {
        if is_gzip {
            let gz = GzDecoder::new(blob);
            let mut archive = tar::Archive::new(gz);
            archive.unpack(dest)?;
        } else {
            warn!("layer declared as gzip but does not have gzip magic; trying raw tar");
            let mut archive = tar::Archive::new(blob);
            archive.unpack(dest)?;
        }
    } else if is_gzip {
        // Plain tar media type but gzip magic — publisher compressed the blob
        // without reflecting it in the media type (common with devcontainer features).
        warn!("layer declared as plain tar but has gzip magic; decompressing");
        let gz = GzDecoder::new(blob);
        let mut archive = tar::Archive::new(gz);
        archive.unpack(dest)?;
    } else {
        let mut archive = tar::Archive::new(blob);
        archive.unpack(dest)?;
    }
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_extractable_layer_recognises_oci_gzip() {
        assert!(is_extractable_layer(IMAGE_LAYER_GZIP_MEDIA_TYPE));
    }

    #[test]
    fn is_extractable_layer_recognises_docker_gzip() {
        assert!(is_extractable_layer(IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE));
    }

    #[test]
    fn is_extractable_layer_recognises_plain_tar() {
        assert!(is_extractable_layer(IMAGE_LAYER_MEDIA_TYPE));
    }

    #[test]
    fn is_extractable_layer_recognises_devcontainers_tar() {
        assert!(is_extractable_layer(
            "application/vnd.devcontainers.layer.v1+tar"
        ));
    }

    #[test]
    fn is_extractable_layer_rejects_manifest_type() {
        assert!(!is_extractable_layer(
            "application/vnd.oci.image.manifest.v1+json"
        ));
    }

    #[test]
    fn is_extractable_layer_rejects_config() {
        assert!(!is_extractable_layer(
            "application/vnd.oci.image.config.v1+json"
        ));
    }

    #[test]
    fn build_auth_anonymous_when_no_creds() {
        // No Docker config on CI -- should fall back to anonymous.
        let auth = build_registry_auth("does-not-exist.example.com");
        assert_eq!(auth, RegistryAuth::Anonymous);
    }

    #[test]
    fn extract_layer_gzip_tarball() {
        // Build a minimal gzipped tarball in memory.
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = tempfile::tempdir().unwrap();

        // Create a file to tar up.
        let src_path = dir.path().join("hello.txt");
        std::fs::write(&src_path, "world").unwrap();

        // Create tar.gz in memory.
        let buf = Vec::new();
        let encoder = flate2::write::GzEncoder::new(buf, flate2::Compression::fast());
        let mut tar_builder = tar::Builder::new(encoder);
        tar_builder
            .append_path_with_name(&src_path, "hello.txt")
            .unwrap();
        let encoder = tar_builder.into_inner().unwrap();
        let compressed = encoder.finish().unwrap();

        extract_layer(&compressed, IMAGE_LAYER_GZIP_MEDIA_TYPE, staging_dir.path()).unwrap();

        let extracted = staging_dir.path().join("hello.txt");
        assert!(extracted.exists(), "extracted file should exist");
        assert_eq!(std::fs::read_to_string(&extracted).unwrap(), "world");
    }

    #[test]
    fn extract_layer_plain_tar() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = tempfile::tempdir().unwrap();

        let src_path = dir.path().join("data.txt");
        std::fs::write(&src_path, "content").unwrap();

        let buf = Vec::new();
        let mut tar_builder = tar::Builder::new(buf);
        tar_builder
            .append_path_with_name(&src_path, "data.txt")
            .unwrap();
        let raw_tar = tar_builder.into_inner().unwrap();

        extract_layer(&raw_tar, IMAGE_LAYER_MEDIA_TYPE, staging_dir.path()).unwrap();

        let extracted = staging_dir.path().join("data.txt");
        assert!(extracted.exists());
        assert_eq!(std::fs::read_to_string(&extracted).unwrap(), "content");
    }

    #[test]
    fn extract_layer_devcontainers_tar() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = tempfile::tempdir().unwrap();

        let src_path = dir.path().join("install.sh");
        std::fs::write(&src_path, "#!/bin/sh\necho hello").unwrap();

        let buf = Vec::new();
        let mut tar_builder = tar::Builder::new(buf);
        tar_builder
            .append_path_with_name(&src_path, "install.sh")
            .unwrap();
        let raw_tar = tar_builder.into_inner().unwrap();

        extract_layer(&raw_tar, DEVCONTAINERS_LAYER_MEDIA_TYPE, staging_dir.path()).unwrap();

        let extracted = staging_dir.path().join("install.sh");
        assert!(extracted.exists());
        assert_eq!(
            std::fs::read_to_string(&extracted).unwrap(),
            "#!/bin/sh\necho hello"
        );
    }

    #[test]
    fn extract_layer_plain_tar_media_type_with_gzip_content() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = tempfile::tempdir().unwrap();

        let src_path = dir.path().join("feature.txt");
        std::fs::write(&src_path, "gzipped-content").unwrap();

        // Create a gzipped tarball.
        let buf = Vec::new();
        let encoder = flate2::write::GzEncoder::new(buf, flate2::Compression::fast());
        let mut tar_builder = tar::Builder::new(encoder);
        tar_builder
            .append_path_with_name(&src_path, "feature.txt")
            .unwrap();
        let encoder = tar_builder.into_inner().unwrap();
        let compressed = encoder.finish().unwrap();

        // Pass with a plain tar media type (no "gzip" in the string).
        extract_layer(
            &compressed,
            DEVCONTAINERS_LAYER_MEDIA_TYPE,
            staging_dir.path(),
        )
        .unwrap();

        let extracted = staging_dir.path().join("feature.txt");
        assert!(extracted.exists());
        assert_eq!(
            std::fs::read_to_string(&extracted).unwrap(),
            "gzipped-content"
        );
    }

    // -----------------------------------------------------------------------
    // Integration test -- requires network access
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires network access to ghcr.io"]
    async fn fetch_node_feature_from_ghcr() {
        let fetcher = OciFetcher::new();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let reference = NormalizedRef::OciTarget {
            registry: "ghcr.io".to_owned(),
            repository: "devcontainers/features/node".to_owned(),
            tag: "1".to_owned(),
        };

        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

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
    #[ignore = "requires network access to ghcr.io"]
    async fn fetch_github_cli_feature_from_ghcr() {
        let fetcher = OciFetcher::new();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let reference = NormalizedRef::OciTarget {
            registry: "ghcr.io".to_owned(),
            repository: "devcontainers/features/github-cli".to_owned(),
            tag: "1".to_owned(),
        };

        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

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
