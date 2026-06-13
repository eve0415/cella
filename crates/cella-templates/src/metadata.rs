//! OCI manifest metadata fetcher for devcontainer templates.
//!
//! Fetches the OCI image manifest for a template reference and extracts the
//! `dev.containers.metadata` annotation, which contains the template's JSON
//! metadata as a string-escaped JSON value.

use oci_distribution::manifest::OciManifest;
use tracing::debug;

use crate::error::TemplateError;
use crate::oci_ref::{build_oci_client, parse_template_ref};

/// Annotation key used by the devcontainer CLI to embed template metadata.
const METADATA_ANNOTATION: &str = "dev.containers.metadata";

/// Fetch the `dev.containers.metadata` annotation from the OCI manifest for
/// the given template reference.
///
/// The template reference may be in the form `registry/repository[:tag]`
/// (e.g. `ghcr.io/devcontainers/templates/alpine`) or pinned by digest
/// (e.g. `ghcr.io/devcontainers/templates/alpine@sha256:<hex>`). A digest-pinned
/// ref fetches the exact manifest by digest rather than by tag.
///
/// Returns:
/// - `Ok(Some(value))` — annotation found; the raw JSON string value.
/// - `Ok(None)` — manifest was fetched successfully but the annotation is absent.
/// - `Err(_)` — network or registry error.
///
/// # Errors
///
/// Returns [`TemplateError`] for registry communication failures or invalid refs.
pub async fn fetch_manifest_metadata(template_ref: &str) -> Result<Option<String>, TemplateError> {
    let parsed = parse_template_ref(template_ref)?;
    let registry = parsed.registry.clone();
    let repository = parsed.repository.clone();
    let version = parsed.version.as_str().to_owned();

    let (client, oci_ref, auth) = build_oci_client(&parsed);

    debug!("fetching manifest for template {registry}/{repository}@{version}");

    let (manifest, _digest) =
        client
            .pull_manifest(&oci_ref, &auth)
            .await
            .map_err(|e| TemplateError::RegistryError {
                registry: registry.clone(),
                message: format!("failed to pull manifest for {repository}:{version}: {e}"),
            })?;

    Ok(extract_metadata_annotation(&manifest))
}

/// Extract the `dev.containers.metadata` annotation from an OCI manifest.
///
/// For an [`OciManifest::Image`], reads `manifest.annotations`.
/// For an [`OciManifest::ImageIndex`], reads `index.annotations` first; if
/// absent, falls back to the first index entry's `annotations`.
fn extract_metadata_annotation(manifest: &OciManifest) -> Option<String> {
    match manifest {
        OciManifest::Image(img) => img
            .annotations
            .as_ref()
            .and_then(|a| a.get(METADATA_ANNOTATION))
            .cloned(),
        OciManifest::ImageIndex(idx) => {
            // Check the index-level annotations first.
            let from_index = idx
                .annotations
                .as_ref()
                .and_then(|a| a.get(METADATA_ANNOTATION))
                .cloned();
            if from_index.is_some() {
                return from_index;
            }
            // Fall back to the first manifest entry's annotations.
            idx.manifests
                .first()
                .and_then(|entry| entry.annotations.as_ref())
                .and_then(|a| a.get(METADATA_ANNOTATION))
                .cloned()
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use oci_distribution::manifest::{OciImageManifest, OciManifest};

    use super::*;

    /// Build an image manifest JSON string with optional annotations and
    /// deserialize it into [`OciManifest`].
    fn image_manifest(annotation_value: Option<&str>) -> OciManifest {
        let annotations_json = annotation_value.map_or_else(
            || "{}".to_owned(),
            |v| {
                let escaped = v.replace('"', "\\\"");
                format!(r#"{{"dev.containers.metadata": "{escaped}"}}"#)
            },
        );
        let json = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {{"mediaType": "application/vnd.devcontainers", "digest": "sha256:abc", "size": 0}},
                "layers": [],
                "annotations": {annotations_json}
            }}"#
        );
        let m: OciImageManifest = serde_json::from_str(&json).unwrap();
        OciManifest::Image(m)
    }

    /// Build an image index JSON string and deserialize it into [`OciManifest`].
    fn image_index(index_annotation: Option<&str>, entry_annotation: Option<&str>) -> OciManifest {
        let index_ann_json = index_annotation.map_or_else(
            || "null".to_owned(),
            |v| {
                let escaped = v.replace('"', "\\\"");
                format!(r#"{{"dev.containers.metadata": "{escaped}"}}"#)
            },
        );
        let entry_ann_json = entry_annotation.map_or_else(
            || "null".to_owned(),
            |v| {
                let escaped = v.replace('"', "\\\"");
                format!(r#"{{"dev.containers.metadata": "{escaped}"}}"#)
            },
        );
        let json = format!(
            r#"{{
                "schemaVersion": 2,
                "manifests": [
                    {{
                        "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "digest": "sha256:abc",
                        "size": 0,
                        "annotations": {entry_ann_json}
                    }}
                ],
                "annotations": {index_ann_json}
            }}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    // -----------------------------------------------------------------------
    // extract_metadata_annotation
    // -----------------------------------------------------------------------

    #[test]
    fn image_manifest_returns_annotation() {
        let manifest = image_manifest(Some(r#"{"id":"alpine","version":"1.0.0"}"#));
        let result = extract_metadata_annotation(&manifest);
        assert_eq!(
            result.as_deref(),
            Some(r#"{"id":"alpine","version":"1.0.0"}"#)
        );
    }

    #[test]
    fn image_manifest_without_annotation_returns_none() {
        let manifest = image_manifest(None);
        assert!(extract_metadata_annotation(&manifest).is_none());
    }

    #[test]
    fn image_index_reads_index_level_annotation() {
        let manifest = image_index(
            Some(r#"{"id":"from-index"}"#),
            Some(r#"{"id":"from-entry"}"#),
        );
        // Index-level annotation takes precedence.
        assert_eq!(
            extract_metadata_annotation(&manifest).as_deref(),
            Some(r#"{"id":"from-index"}"#)
        );
    }

    #[test]
    fn image_index_falls_back_to_first_entry_annotation() {
        let manifest = image_index(None, Some(r#"{"id":"from-entry"}"#));
        assert_eq!(
            extract_metadata_annotation(&manifest).as_deref(),
            Some(r#"{"id":"from-entry"}"#)
        );
    }

    #[test]
    fn image_index_no_annotation_returns_none() {
        let manifest = image_index(None, None);
        assert!(extract_metadata_annotation(&manifest).is_none());
    }
}
