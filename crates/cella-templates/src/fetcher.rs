//! OCI registry fetcher for devcontainer template artifacts.
//!
//! Pulls individual template tarballs from OCI registries, extracts them,
//! and caches the result on disk.

use std::path::PathBuf;

use flate2::read::GzDecoder;
use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::manifest::{
    IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
};
use oci_distribution::secrets::RegistryAuth;
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::TemplateMetadata;
use crate::cache::TemplateCache;
use crate::error::TemplateError;

/// Media type for devcontainer template/feature layers (plain tar).
const DEVCONTAINERS_LAYER_MEDIA_TYPE: &str = "application/vnd.devcontainers.layer.v1+tar";

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
    let (registry, repository, tag) = parse_template_ref(template_ref)?;

    // Check cache first
    if let Some(cached) = cache.get_template(&registry, &repository, &tag) {
        debug!("template cache hit for {template_ref}");
        return Ok(cached);
    }

    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    };
    let client = oci_distribution::Client::new(config);
    let oci_ref = Reference::with_tag(registry.clone(), repository.clone(), tag.clone());
    let auth = build_registry_auth(&registry);

    debug!("fetching template {registry}/{repository}:{tag}");

    let (manifest, digest) = client
        .pull_image_manifest(&oci_ref, &auth)
        .await
        .map_err(|e| TemplateError::RegistryError {
            registry: registry.clone(),
            message: format!("failed to pull manifest for {repository}:{tag}: {e}"),
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

    extract_layer(&blob, &layer.media_type, &staging).map_err(|e| {
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

/// Parse a template reference into `(registry, repository, tag)`.
fn parse_template_ref(template_ref: &str) -> Result<(String, String, String), TemplateError> {
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

fn build_registry_auth(registry: &str) -> RegistryAuth {
    let creds = cella_features::auth::resolve_credentials(registry);
    if let (Some(u), Some(p)) = (creds.username, creds.password) {
        RegistryAuth::Basic(u, p)
    } else {
        RegistryAuth::Anonymous
    }
}

fn find_extractable_layer<'a>(
    manifest: &'a oci_distribution::manifest::OciImageManifest,
    template_ref: &str,
) -> Result<&'a oci_distribution::manifest::OciDescriptor, TemplateError> {
    manifest
        .layers
        .iter()
        .find(|l| is_extractable_layer(&l.media_type))
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

fn extract_layer(blob: &[u8], media_type: &str, dest: &std::path::Path) -> std::io::Result<()> {
    let is_gzip = blob.len() >= 2 && blob[0] == 0x1f && blob[1] == 0x8b;

    if media_type.contains("gzip") || media_type == IMAGE_LAYER_GZIP_MEDIA_TYPE {
        if is_gzip {
            let gz = GzDecoder::new(blob);
            let mut archive = tar::Archive::new(gz);
            archive.unpack(dest)?;
        } else {
            tracing::warn!("layer declared as gzip but no gzip magic; trying raw tar");
            let mut archive = tar::Archive::new(blob);
            archive.unpack(dest)?;
        }
    } else if is_gzip {
        tracing::warn!("layer declared as plain tar but has gzip magic; decompressing");
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

    #[test]
    fn parse_full_ref() {
        let (reg, repo, tag) =
            parse_template_ref("ghcr.io/devcontainers/templates/rust:5.0.0").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/templates/rust");
        assert_eq!(tag, "5.0.0");
    }

    #[test]
    fn parse_ref_no_tag() {
        let (reg, repo, tag) = parse_template_ref("ghcr.io/devcontainers/templates/rust").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/templates/rust");
        assert_eq!(tag, "latest");
    }

    #[test]
    fn parse_ref_invalid() {
        assert!(parse_template_ref("noslash").is_err());
    }

    #[test]
    fn extractable_layer_detection() {
        assert!(is_extractable_layer(IMAGE_LAYER_GZIP_MEDIA_TYPE));
        assert!(is_extractable_layer(IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE));
        assert!(is_extractable_layer(IMAGE_LAYER_MEDIA_TYPE));
        assert!(is_extractable_layer(DEVCONTAINERS_LAYER_MEDIA_TYPE));
        assert!(!is_extractable_layer(
            "application/vnd.oci.image.config.v1+json"
        ));
    }

    #[test]
    fn extract_gzip_tarball() {
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

        extract_layer(&compressed, IMAGE_LAYER_GZIP_MEDIA_TYPE, staging.path()).unwrap();
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

    #[tokio::test]
    #[cfg(feature = "integration-tests")]
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
