//! Fetch and cache the aggregated devcontainer index from containers.dev.
//!
//! The index (`devcontainer-index.json`) is a pre-crawled aggregate of all
//! registered template and feature collections, rebuilt daily by a GitHub
//! Actions cron job in the devcontainers.github.io repository.  It contains
//! the same data VS Code uses for template discovery.

use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use tracing::debug;

use crate::cache::TemplateCache;
use crate::error::TemplateError;
use crate::types::DevcontainerIndex;

/// URL of the pre-crawled aggregate index.
const INDEX_URL: &str = "https://containers.dev/static/devcontainer-index.json";

/// Cache TTL for the aggregated index (same 24h as collection cache).
const INDEX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// OCI reference prefix for official devcontainer collections.
const OFFICIAL_PREFIX: &str = "ghcr.io/devcontainers/";

/// Fetch the aggregated devcontainer index, using the cache when available.
///
/// 1. If `force_refresh` is false and the cache has a fresh (< 24h) entry,
///    return it.
/// 2. Otherwise, fetch from `containers.dev` via HTTPS.
/// 3. On network failure, fall back to a stale cache entry (with warning).
/// 4. If no cache exists at all, return an error.
///
/// # Errors
///
/// Returns [`TemplateError::IndexFetchFailed`] when the fetch fails and no
/// cached data is available.
pub async fn fetch_devcontainer_index(
    cache: &TemplateCache,
    force_refresh: bool,
) -> Result<DevcontainerIndex, TemplateError> {
    let cache_key = INDEX_URL;

    if !force_refresh && let Some(index) = get_cached_index(cache, cache_key) {
        return Ok(index);
    }

    match fetch_index_json().await {
        Ok(json) => {
            let index: DevcontainerIndex =
                serde_json::from_str(&json).map_err(|e| TemplateError::IndexFetchFailed {
                    message: format!("failed to parse devcontainer index: {e}"),
                })?;
            let _ = put_cached_index(cache, cache_key, &json);
            Ok(index)
        }
        Err(e) => {
            // Fall back to stale cache
            if let Some(index) = get_stale_cached_index(cache, cache_key) {
                tracing::warn!("could not fetch devcontainer index: {e}; using cached version");
                Ok(index)
            } else {
                Err(e)
            }
        }
    }
}

/// Check whether a collection's OCI reference marks it as official.
pub fn is_official_collection(oci_reference: &str) -> bool {
    oci_reference.starts_with(OFFICIAL_PREFIX)
}

// ── HTTP fetch ───────────────────────────────────────────────────────

async fn fetch_index_json() -> Result<String, TemplateError> {
    debug!("fetching devcontainer index from {INDEX_URL}");

    let response = reqwest::get(INDEX_URL)
        .await
        .map_err(|e| TemplateError::IndexFetchFailed {
            message: format!("HTTP request failed: {e}"),
        })?;

    if !response.status().is_success() {
        return Err(TemplateError::IndexFetchFailed {
            message: format!("HTTP {}", response.status()),
        });
    }

    response
        .text()
        .await
        .map_err(|e| TemplateError::IndexFetchFailed {
            message: format!("failed to read response body: {e}"),
        })
}

// ── Cache helpers ────────────────────────────────────────────────────
//
// The index is cached as a raw JSON file alongside collection caches,
// using a SHA-256 hash of the URL as the filename.

fn index_cache_path(cache: &TemplateCache, key: &str) -> std::path::PathBuf {
    let hash = hex::encode(&Sha256::digest(key.as_bytes())[..8]);
    cache.root().join("index").join(format!("{hash}.json"))
}

fn get_cached_index(cache: &TemplateCache, key: &str) -> Option<DevcontainerIndex> {
    let path = index_cache_path(cache, key);
    let metadata = std::fs::metadata(&path).ok()?;
    let modified = metadata.modified().ok()?;

    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    if age > INDEX_TTL {
        debug!("index cache expired (age: {age:?})");
        return None;
    }

    let content = std::fs::read_to_string(&path).ok()?;
    let index: DevcontainerIndex = serde_json::from_str(&content).ok()?;
    debug!("index cache hit");
    Some(index)
}

fn get_stale_cached_index(cache: &TemplateCache, key: &str) -> Option<DevcontainerIndex> {
    let path = index_cache_path(cache, key);
    let content = std::fs::read_to_string(&path).ok()?;
    let index: DevcontainerIndex = serde_json::from_str(&content).ok()?;
    debug!("index stale cache hit");
    Some(index)
}

fn put_cached_index(cache: &TemplateCache, key: &str, json: &str) -> Result<(), TemplateError> {
    let path = index_cache_path(cache, key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TemplateError::CacheError {
            message: format!("failed to create index cache directory: {e}"),
        })?;
    }
    std::fs::write(&path, json).map_err(|e| TemplateError::CacheError {
        message: format!("failed to write index cache: {e}"),
    })?;
    debug!("cached devcontainer index at {}", path.display());
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_collection_detection() {
        assert!(is_official_collection("ghcr.io/devcontainers/templates"));
        assert!(is_official_collection("ghcr.io/devcontainers/features"));
        assert!(!is_official_collection("ghcr.io/microsoft/templates"));
        assert!(!is_official_collection("ghcr.io/localstack/templates"));
        assert!(!is_official_collection("myregistry.azurecr.io/templates"));
    }

    #[test]
    fn parse_minimal_index() {
        let json = r#"{ "collections": [] }"#;
        let index: DevcontainerIndex = serde_json::from_str(json).unwrap();
        assert!(index.collections.is_empty());
    }

    #[test]
    fn parse_index_with_collections() {
        let json = r#"{
            "collections": [
                {
                    "sourceInformation": {
                        "name": "Reference Implementation Templates",
                        "maintainer": "Dev Container Spec Maintainers",
                        "ociReference": "ghcr.io/devcontainers/templates"
                    },
                    "templates": [
                        {
                            "id": "ghcr.io/devcontainers/templates/rust",
                            "version": "5.0.0",
                            "name": "Rust",
                            "description": "Develop Rust applications."
                        }
                    ],
                    "features": []
                },
                {
                    "sourceInformation": {
                        "name": "Microsoft Templates",
                        "maintainer": "Microsoft",
                        "ociReference": "ghcr.io/microsoft/templates"
                    },
                    "templates": [
                        {
                            "id": "ghcr.io/microsoft/templates/dotnet",
                            "version": "2.0.0",
                            "name": ".NET",
                            "description": ".NET development"
                        }
                    ],
                    "features": []
                }
            ]
        }"#;

        let index: DevcontainerIndex = serde_json::from_str(json).unwrap();
        assert_eq!(index.collections.len(), 2);

        let first = &index.collections[0];
        assert_eq!(
            first.source_information.name.as_deref(),
            Some("Reference Implementation Templates")
        );
        assert_eq!(
            first.source_information.oci_reference.as_deref(),
            Some("ghcr.io/devcontainers/templates")
        );
        assert!(is_official_collection(
            first.source_information.oci_reference.as_deref().unwrap()
        ));
        assert_eq!(first.templates.len(), 1);
        assert_eq!(first.templates[0].name.as_deref(), Some("Rust"));

        let second = &index.collections[1];
        assert!(!is_official_collection(
            second.source_information.oci_reference.as_deref().unwrap()
        ));
    }

    #[test]
    fn parse_index_with_features() {
        let json = r#"{
            "collections": [
                {
                    "sourceInformation": {
                        "name": "Official Features",
                        "ociReference": "ghcr.io/devcontainers/features"
                    },
                    "templates": [],
                    "features": [
                        {
                            "id": "ghcr.io/devcontainers/features/node",
                            "version": "1.5.0",
                            "name": "Node.js",
                            "description": "Installs Node.js and npm"
                        }
                    ]
                }
            ]
        }"#;

        let index: DevcontainerIndex = serde_json::from_str(json).unwrap();
        let features = &index.collections[0].features;
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].name.as_deref(), Some("Node.js"));
    }

    #[test]
    fn index_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let key = "https://example.com/index.json";
        let json = r#"{"collections":[]}"#;

        put_cached_index(&cache, key, json).unwrap();
        let index = get_cached_index(&cache, key).unwrap();
        assert!(index.collections.is_empty());
    }

    #[test]
    fn index_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        assert!(get_cached_index(&cache, "nonexistent").is_none());
    }

    #[test]
    fn index_stale_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let key = "https://example.com/index.json";
        let json = r#"{"collections":[]}"#;

        put_cached_index(&cache, key, json).unwrap();

        // Backdate to simulate expiry
        let path = index_cache_path(&cache, key);
        let old_time = filetime::FileTime::from_unix_time(0, 0);
        filetime::set_file_mtime(&path, old_time).unwrap();

        // Fresh cache should miss
        assert!(get_cached_index(&cache, key).is_none());

        // Stale cache should hit
        assert!(get_stale_cached_index(&cache, key).is_some());
    }
}
