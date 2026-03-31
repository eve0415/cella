//! Fetch and parse devcontainer collection indexes from OCI registries.
//!
//! A collection index (`devcontainer-collection.json`) lists all templates
//! or features available in a registry namespace.  It is published as an
//! OCI artifact with a specific media type.

use oci_distribution::Reference;
use oci_distribution::client::{ClientConfig, ClientProtocol};
use oci_distribution::secrets::RegistryAuth;
use tracing::debug;

use crate::cache::TemplateCache;
use crate::error::TemplateError;
use crate::types::{FeatureCollectionIndex, TemplateCollectionIndex};

/// Media type for the collection metadata layer.
const COLLECTION_MEDIA_TYPE: &str = "application/vnd.devcontainers.collection.layer.v1+json";

/// Default registry namespace for official templates.
pub const DEFAULT_TEMPLATE_COLLECTION: &str = "ghcr.io/devcontainers/templates";

/// Default registry namespace for official features.
pub const DEFAULT_FEATURE_COLLECTION: &str = "ghcr.io/devcontainers/features";

/// Fetch the template collection index, using the cache when available.
///
/// 1. If `force_refresh` is false and the cache has a fresh (< 24h) entry,
///    return it.
/// 2. Otherwise, fetch from the OCI registry.
/// 3. On network failure, fall back to a stale cache entry (with warning).
/// 4. If no cache exists at all, return an error.
///
/// # Errors
///
/// Returns [`TemplateError::CollectionFetchFailed`] when the fetch fails
/// and no cached data is available.
pub async fn fetch_template_collection(
    collection_ref: &str,
    cache: &TemplateCache,
    force_refresh: bool,
) -> Result<TemplateCollectionIndex, TemplateError> {
    if !force_refresh && let Some((index, _)) = cache.get_collection(collection_ref) {
        return Ok(index);
    }

    match fetch_collection_json(collection_ref).await {
        Ok(json) => {
            let index: TemplateCollectionIndex =
                serde_json::from_str(&json).map_err(|e| TemplateError::CollectionFetchFailed {
                    registry: collection_ref.to_owned(),
                    message: format!("failed to parse collection index: {e}"),
                })?;
            let _ = cache.put_collection(collection_ref, &index);
            Ok(index)
        }
        Err(e) => {
            // Fall back to stale cache
            if let Some((index, modified)) = cache.get_collection_stale(collection_ref) {
                let age = std::time::SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default();
                tracing::warn!(
                    "could not fetch collection from {collection_ref}: {e}; \
                     using cached index ({age:.0?} old)"
                );
                Ok(index)
            } else {
                Err(e)
            }
        }
    }
}

/// Fetch the feature collection index (same pattern as templates).
///
/// # Errors
///
/// Returns [`TemplateError::CollectionFetchFailed`] when the fetch fails
/// and no cached data is available.
pub async fn fetch_feature_collection(
    collection_ref: &str,
    cache: &TemplateCache,
    force_refresh: bool,
) -> Result<FeatureCollectionIndex, TemplateError> {
    if !force_refresh {
        // Feature collections share the same cache infra; we store them
        // under a different key by using the full collection ref.
        if let Some((index, _)) = cache.get_collection(collection_ref) {
            let feature_index: FeatureCollectionIndex =
                serde_json::from_value(serde_json::to_value(index).unwrap_or_default())
                    .unwrap_or_else(|_| FeatureCollectionIndex {
                        features: vec![],
                        source_information: None,
                    });
            return Ok(feature_index);
        }
    }

    match fetch_collection_json(collection_ref).await {
        Ok(json) => {
            let index: FeatureCollectionIndex =
                serde_json::from_str(&json).map_err(|e| TemplateError::CollectionFetchFailed {
                    registry: collection_ref.to_owned(),
                    message: format!("failed to parse feature collection index: {e}"),
                })?;
            // Cache the raw JSON by wrapping in a TemplateCollectionIndex shape
            // (the cache is generic JSON under the hood)
            let raw: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
            if let Ok(template_shaped) = serde_json::from_value::<TemplateCollectionIndex>(raw) {
                let _ = cache.put_collection(collection_ref, &template_shaped);
            }
            Ok(index)
        }
        Err(e) => Err(e),
    }
}

/// Fetch the raw JSON of a `devcontainer-collection.json` from an OCI registry.
///
/// The collection is published as an OCI artifact.  We pull its manifest,
/// find the JSON layer, and download it.
async fn fetch_collection_json(collection_ref: &str) -> Result<String, TemplateError> {
    let (registry, repository) = parse_collection_ref(collection_ref)?;

    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..ClientConfig::default()
    };
    let client = oci_distribution::Client::new(config);

    let oci_ref = Reference::with_tag(registry.clone(), repository.clone(), "latest".to_owned());
    let auth = build_registry_auth(&registry);

    debug!("fetching collection index from {registry}/{repository}:latest");

    let (manifest, _digest) = client
        .pull_image_manifest(&oci_ref, &auth)
        .await
        .map_err(|e| TemplateError::CollectionFetchFailed {
            registry: collection_ref.to_owned(),
            message: format!("failed to pull manifest: {e}"),
        })?;

    // Find the collection JSON layer
    let layer = manifest
        .layers
        .iter()
        .find(|l| l.media_type == COLLECTION_MEDIA_TYPE || l.media_type.contains("json"))
        .or_else(|| manifest.layers.first())
        .ok_or_else(|| TemplateError::CollectionFetchFailed {
            registry: collection_ref.to_owned(),
            message: "no layers found in collection manifest".to_owned(),
        })?;

    let mut blob = Vec::with_capacity(usize::try_from(layer.size.max(0)).unwrap_or(0));
    client
        .pull_blob(&oci_ref, layer, &mut blob)
        .await
        .map_err(|e| TemplateError::CollectionFetchFailed {
            registry: collection_ref.to_owned(),
            message: format!("failed to pull collection blob: {e}"),
        })?;

    String::from_utf8(blob).map_err(|e| TemplateError::CollectionFetchFailed {
        registry: collection_ref.to_owned(),
        message: format!("collection blob is not valid UTF-8: {e}"),
    })
}

/// Parse a collection reference like `ghcr.io/devcontainers/templates` into
/// `(registry, repository)`.
fn parse_collection_ref(collection_ref: &str) -> Result<(String, String), TemplateError> {
    let (registry, repository) =
        collection_ref
            .split_once('/')
            .ok_or_else(|| TemplateError::CollectionFetchFailed {
                registry: collection_ref.to_owned(),
                message: "invalid collection reference: expected registry/repository".to_owned(),
            })?;
    Ok((registry.to_owned(), repository.to_owned()))
}

/// Build [`RegistryAuth`] from Docker credential store.
fn build_registry_auth(registry: &str) -> RegistryAuth {
    let creds = cella_features::auth::resolve_credentials(registry);
    if let (Some(u), Some(p)) = (creds.username, creds.password) {
        debug!("using basic auth for {registry}");
        RegistryAuth::Basic(u, p)
    } else {
        debug!("no credentials for {registry}; using anonymous auth");
        RegistryAuth::Anonymous
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ghcr_collection_ref() {
        let (reg, repo) = parse_collection_ref("ghcr.io/devcontainers/templates").unwrap();
        assert_eq!(reg, "ghcr.io");
        assert_eq!(repo, "devcontainers/templates");
    }

    #[test]
    fn parse_custom_collection_ref() {
        let (reg, repo) = parse_collection_ref("myregistry.azurecr.io/team/templates").unwrap();
        assert_eq!(reg, "myregistry.azurecr.io");
        assert_eq!(repo, "team/templates");
    }

    #[test]
    fn parse_invalid_collection_ref() {
        let err = parse_collection_ref("no-slash").unwrap_err();
        assert!(matches!(err, TemplateError::CollectionFetchFailed { .. }));
    }

    #[tokio::test]
    #[cfg(feature = "integration-tests")]
    async fn fetch_official_template_collection() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(cache_dir.path());

        let index = fetch_template_collection(DEFAULT_TEMPLATE_COLLECTION, &cache, true)
            .await
            .unwrap();

        assert!(
            !index.templates.is_empty(),
            "official collection should have templates"
        );

        // Should contain well-known templates
        let ids: Vec<&str> = index.templates.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"rust"), "should contain rust template");
        assert!(ids.contains(&"debian"), "should contain debian template");
    }

    #[tokio::test]
    #[cfg(feature = "integration-tests")]
    async fn fetch_official_feature_collection() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(cache_dir.path());

        let index = fetch_feature_collection(DEFAULT_FEATURE_COLLECTION, &cache, true)
            .await
            .unwrap();

        assert!(
            !index.features.is_empty(),
            "official collection should have features"
        );

        let ids: Vec<&str> = index.features.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"node"), "should contain node feature");
    }
}
