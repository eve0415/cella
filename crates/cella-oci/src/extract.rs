//! OCI layer extraction utilities.

use flate2::read::GzDecoder;
use oci_distribution::manifest::{
    IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
};
use tracing::warn;

/// Media type for devcontainer feature layers (plain tar).
pub const DEVCONTAINERS_LAYER_MEDIA_TYPE: &str = "application/vnd.devcontainers.layer.v1+tar";

/// Returns `true` when the media type indicates a layer we can extract.
pub fn is_extractable_layer(media_type: &str) -> bool {
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
///
/// # Errors
///
/// Returns an I/O error if extraction fails.
pub fn extract_layer(blob: &[u8], media_type: &str, dest: &std::path::Path) -> std::io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(is_extractable_layer(DEVCONTAINERS_LAYER_MEDIA_TYPE));
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
    fn extract_layer_gzip_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = tempfile::tempdir().unwrap();

        let src_path = dir.path().join("hello.txt");
        std::fs::write(&src_path, "world").unwrap();

        let buf = Vec::new();
        let encoder = flate2::write::GzEncoder::new(buf, flate2::Compression::fast());
        let mut tar_builder = tar::Builder::new(encoder);
        tar_builder
            .append_path_with_name(&src_path, "hello.txt")
            .unwrap();
        let gz_bytes = tar_builder.into_inner().unwrap().finish().unwrap();

        extract_layer(&gz_bytes, IMAGE_LAYER_GZIP_MEDIA_TYPE, staging_dir.path()).unwrap();
        let extracted = staging_dir.path().join("hello.txt");
        assert!(extracted.exists());
        assert_eq!(std::fs::read_to_string(extracted).unwrap(), "world");
    }
}
