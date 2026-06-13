//! OCI registry fetcher for devcontainer template artifacts.
//!
//! Pulls individual template tarballs from OCI registries, extracts them,
//! and caches the result on disk.

use std::path::PathBuf;

use sha2::{Digest, Sha256};
use tracing::debug;

use crate::TemplateMetadata;
use crate::cache::TemplateCache;
use crate::error::TemplateError;
use crate::oci_ref::build_oci_client;
use crate::oci_ref::parse_template_ref;

/// Fetch a template artifact from an OCI registry and return the local
/// path where it was extracted.
///
/// The template reference should be in the form `registry/repository:tag`
/// (e.g. `ghcr.io/devcontainers/templates/rust:latest`).
///
/// # Errors
///
/// Returns [`TemplateError`] variants for registry errors, missing
/// artifacts, or extraction failures.
pub async fn fetch_template(
    template_ref: &str,
    cache: &TemplateCache,
) -> Result<PathBuf, TemplateError> {
    let parsed = parse_template_ref(template_ref)?;
    let registry = parsed.registry.clone();
    let repository = parsed.repository.clone();
    let version = parsed.version.as_str().to_owned();

    // Check cache first
    if let Some(cached) = cache.get_template(&registry, &repository, &version) {
        debug!("template cache hit for {template_ref}");
        return Ok(cached);
    }

    let (client, oci_ref, auth) = build_oci_client(&parsed);

    debug!("fetching template {registry}/{repository}@{version}");

    let (manifest, digest) = client
        .pull_image_manifest(&oci_ref, &auth)
        .await
        .map_err(|e| TemplateError::RegistryError {
            registry: registry.clone(),
            message: format!("failed to pull manifest for {repository}:{version}: {e}"),
        })?;

    // Check cache by digest
    if let Some(cached) = cache.get_template(&registry, &repository, &digest) {
        debug!("template cache hit by digest {digest}");
        return Ok(cached);
    }

    let layer = find_extractable_layer(&manifest, template_ref)?;

    let capacity = usize::try_from(layer.size.max(0)).unwrap_or(0);
    let mut blob = Vec::with_capacity(capacity);
    client
        .pull_blob(&oci_ref, layer, &mut blob)
        .await
        .map_err(|e| TemplateError::RegistryError {
            registry: registry.clone(),
            message: format!("failed to pull layer blob: {e}"),
        })?;

    // Verify digest
    let actual_digest = format!("sha256:{}", hex::encode(Sha256::digest(&blob)));
    if actual_digest != layer.digest {
        return Err(TemplateError::DigestMismatch {
            template_id: template_ref.to_owned(),
            expected: layer.digest.clone(),
            actual: actual_digest,
        });
    }

    // Extract to cache
    let final_path = cache.template_path(&registry, &repository, &digest);
    let staging = TemplateCache::staging_path(&final_path);

    std::fs::create_dir_all(&staging).map_err(|e| TemplateError::CacheError {
        message: format!("failed to create staging directory: {e}"),
    })?;

    cella_oci::extract_layer(&blob, &layer.media_type, &staging).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        TemplateError::InvalidArtifact {
            template_id: template_ref.to_owned(),
            reason: format!("failed to extract layer: {e}"),
        }
    })?;

    TemplateCache::commit(&staging, &final_path).map_err(|e| TemplateError::CacheError {
        message: format!("failed to commit cache entry: {e}"),
    })?;

    debug!("cached template {template_ref} at {}", final_path.display());
    Ok(final_path)
}

/// Read `devcontainer-template.json` from an extracted template directory.
///
/// # Errors
///
/// Returns [`TemplateError::InvalidMetadata`] if the file is missing or
/// malformed.
pub fn read_template_metadata(
    template_dir: &std::path::Path,
) -> Result<TemplateMetadata, TemplateError> {
    let meta_path = template_dir.join("devcontainer-template.json");
    let content =
        std::fs::read_to_string(&meta_path).map_err(|e| TemplateError::InvalidMetadata {
            template_id: template_dir.display().to_string(),
            reason: format!("cannot read devcontainer-template.json: {e}"),
        })?;
    serde_json::from_str(&content).map_err(|e| TemplateError::InvalidMetadata {
        template_id: template_dir.display().to_string(),
        reason: format!("invalid devcontainer-template.json: {e}"),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_extractable_layer<'a>(
    manifest: &'a oci_distribution::manifest::OciImageManifest,
    template_ref: &str,
) -> Result<&'a oci_distribution::manifest::OciDescriptor, TemplateError> {
    manifest
        .layers
        .iter()
        .find(|l| cella_oci::is_extractable_layer(&l.media_type))
        .ok_or_else(|| TemplateError::InvalidArtifact {
            template_id: template_ref.to_owned(),
            reason: format!(
                "no extractable layer found; layer media types: [{}]",
                manifest
                    .layers
                    .iter()
                    .map(|l| l.media_type.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extractable_layer_detection() {
        use oci_distribution::manifest::{
            IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
        };
        assert!(cella_oci::is_extractable_layer(IMAGE_LAYER_GZIP_MEDIA_TYPE));
        assert!(cella_oci::is_extractable_layer(
            IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE
        ));
        assert!(cella_oci::is_extractable_layer(IMAGE_LAYER_MEDIA_TYPE));
        assert!(cella_oci::is_extractable_layer(
            cella_oci::extract::DEVCONTAINERS_LAYER_MEDIA_TYPE
        ));
        assert!(!cella_oci::is_extractable_layer(
            "application/vnd.oci.image.config.v1+json"
        ));
    }

    #[test]
    fn extract_gzip_tarball() {
        use oci_distribution::manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE;

        let dir = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();

        let src_path = dir.path().join("hello.txt");
        std::fs::write(&src_path, "world").unwrap();

        let buf = Vec::new();
        let encoder = flate2::write::GzEncoder::new(buf, flate2::Compression::fast());
        let mut tar_builder = tar::Builder::new(encoder);
        tar_builder
            .append_path_with_name(&src_path, "hello.txt")
            .unwrap();
        let encoder = tar_builder.into_inner().unwrap();
        let compressed = encoder.finish().unwrap();

        cella_oci::extract_layer(&compressed, IMAGE_LAYER_GZIP_MEDIA_TYPE, staging.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(staging.path().join("hello.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn read_metadata_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let meta_json = r#"{
            "id": "test",
            "version": "1.0.0",
            "name": "Test Template",
            "options": {}
        }"#;
        std::fs::write(dir.path().join("devcontainer-template.json"), meta_json).unwrap();

        let meta = read_template_metadata(dir.path()).unwrap();
        assert_eq!(meta.id, "test");
        assert_eq!(meta.name.as_deref(), Some("Test Template"));
    }

    #[test]
    fn read_metadata_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_template_metadata(dir.path()).is_err());
    }

    #[cella_testing::runtime_test(network)]
    async fn fetch_rust_template() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(cache_dir.path());

        let path = fetch_template("ghcr.io/devcontainers/templates/rust:latest", &cache)
            .await
            .unwrap();

        assert!(
            path.join("devcontainer-template.json").exists(),
            "should contain devcontainer-template.json"
        );

        let meta = read_template_metadata(&path).unwrap();
        assert_eq!(meta.id, "rust");
    }
}
