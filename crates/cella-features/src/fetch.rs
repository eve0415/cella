//! HTTP tarball and local-path fetchers for devcontainer features.
//!
//! [`HttpFetcher`] downloads a gzipped tarball from a URL, extracts it into
//! the cache, and returns the cached path.  [`LocalFetcher`] validates that
//! a local directory contains the expected `devcontainer-feature.json` and
//! returns the path directly (no caching).
//!
//! A [`MockFetcher`] is provided for unit testing code that depends on
//! [`FeatureFetcher`] without requiring network or filesystem setup.

use std::path::PathBuf;

use flate2::read::GzDecoder;
use tracing::debug;

use crate::Platform;
use crate::cache::FeatureCache;
use crate::error::FeatureError;
use crate::oci::FeatureFetcher;
use crate::reference::NormalizedRef;

// ---------------------------------------------------------------------------
// HTTP tarball fetcher
// ---------------------------------------------------------------------------

/// Fetches devcontainer features from HTTP(S) tarball URLs.
///
/// Downloads the tarball, verifies the gzip magic bytes, extracts with
/// `flate2` + `tar`, and commits the result to the feature cache.
pub struct HttpFetcher;

impl FeatureFetcher for HttpFetcher {
    async fn fetch(
        &self,
        reference: &NormalizedRef,
        _platform: &Platform,
        cache: &FeatureCache,
    ) -> Result<PathBuf, FeatureError> {
        let NormalizedRef::HttpTarget { url } = reference else {
            return Err(FeatureError::InvalidReference {
                reference: reference.to_string(),
                reason: "HttpFetcher only handles HTTP targets".to_owned(),
            });
        };

        // Step 1: check cache.
        if let Some(cached) = cache.get_url(url) {
            debug!("cache hit for URL {url}");
            return Ok(cached);
        }

        // Step 2: download the tarball.
        let bytes = reqwest::get(url)
            .await
            .map_err(|e| FeatureError::FetchFailed {
                url: url.clone(),
                message: format!("HTTP request failed: {e}"),
            })?
            .error_for_status()
            .map_err(|e| FeatureError::FetchFailed {
                url: url.clone(),
                message: format!("HTTP error status: {e}"),
            })?
            .bytes()
            .await
            .map_err(|e| FeatureError::FetchFailed {
                url: url.clone(),
                message: format!("failed to read response body: {e}"),
            })?;

        debug!("downloaded {} bytes from {url}", bytes.len());

        // Step 3: verify gzip magic bytes.
        if bytes.len() < 2 || bytes[0] != 0x1f || bytes[1] != 0x8b {
            return Err(FeatureError::FetchFailed {
                url: url.clone(),
                message: "response is not a gzip archive (missing magic bytes 0x1f 0x8b)"
                    .to_owned(),
            });
        }

        // Step 4: extract into staging directory.
        let final_path = cache.url_path(url);
        let staging = FeatureCache::staging_path(&final_path);

        std::fs::create_dir_all(&staging).map_err(|e| FeatureError::FetchFailed {
            url: url.clone(),
            message: format!("failed to create staging directory: {e}"),
        })?;

        let gz = GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(gz);
        archive.unpack(&staging).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging);
            FeatureError::FetchFailed {
                url: url.clone(),
                message: format!("failed to extract tarball: {e}"),
            }
        })?;

        // Step 5: atomic commit.
        FeatureCache::commit(&staging, &final_path).map_err(|e| FeatureError::FetchFailed {
            url: url.clone(),
            message: format!("failed to commit cache entry: {e}"),
        })?;

        debug!("cached HTTP feature at {}", final_path.display());

        Ok(final_path)
    }
}

// ---------------------------------------------------------------------------
// Local path fetcher
// ---------------------------------------------------------------------------

/// Fetches devcontainer features from local filesystem paths.
///
/// Validates that the directory exists and contains a
/// `devcontainer-feature.json` file.  No caching is performed -- the
/// original path is returned directly.
pub struct LocalFetcher;

impl FeatureFetcher for LocalFetcher {
    async fn fetch(
        &self,
        reference: &NormalizedRef,
        _platform: &Platform,
        _cache: &FeatureCache,
    ) -> Result<PathBuf, FeatureError> {
        let NormalizedRef::LocalTarget { absolute_path } = reference else {
            return Err(FeatureError::InvalidReference {
                reference: reference.to_string(),
                reason: "LocalFetcher only handles local targets".to_owned(),
            });
        };

        // Step 1: verify the directory exists.
        if !absolute_path.is_dir() {
            return Err(FeatureError::LocalFeatureNotFound {
                path: absolute_path.clone(),
            });
        }

        // Step 2: verify devcontainer-feature.json exists.
        let metadata_path = absolute_path.join("devcontainer-feature.json");
        if !metadata_path.is_file() {
            return Err(FeatureError::InvalidMetadata {
                feature_id: absolute_path.display().to_string(),
                reason: "directory does not contain devcontainer-feature.json".to_owned(),
            });
        }

        debug!("local feature found at {}", absolute_path.display());

        Ok(absolute_path.clone())
    }
}

// ---------------------------------------------------------------------------
// Mock fetcher (test support)
// ---------------------------------------------------------------------------

/// Mock fetcher that returns pre-populated directories.
///
/// Maps string representations of [`NormalizedRef`] values to filesystem
/// paths.  Useful for testing code that depends on [`FeatureFetcher`]
/// without requiring network access or real tarballs.
#[cfg(test)]
pub struct MockFetcher {
    pub responses: std::collections::HashMap<String, PathBuf>,
}

#[cfg(test)]
impl FeatureFetcher for MockFetcher {
    async fn fetch(
        &self,
        reference: &NormalizedRef,
        _platform: &Platform,
        _cache: &FeatureCache,
    ) -> Result<PathBuf, FeatureError> {
        let key = reference.to_string();
        self.responses
            .get(&key)
            .cloned()
            .ok_or_else(|| FeatureError::FetchFailed {
                url: key,
                message: "no mock response configured for this reference".to_owned(),
            })
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    // -----------------------------------------------------------------------
    // HttpFetcher: cache hit skips download
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn http_cache_hit_skips_download() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());

        let url = "https://example.com/feature.tgz";
        let cached_path = cache.url_path(url);
        std::fs::create_dir_all(&cached_path).unwrap();
        std::fs::write(
            cached_path.join("devcontainer-feature.json"),
            r#"{"id":"test","version":"1.0.0"}"#,
        )
        .unwrap();

        let fetcher = HttpFetcher;
        let reference = NormalizedRef::HttpTarget {
            url: url.to_owned(),
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let result = fetcher.fetch(&reference, &platform, &cache).await.unwrap();
        assert_eq!(result, cached_path);
    }

    // -----------------------------------------------------------------------
    // HttpFetcher: invalid URL produces FetchFailed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn http_invalid_url_returns_fetch_failed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());

        let fetcher = HttpFetcher;
        let reference = NormalizedRef::HttpTarget {
            url: "http://[::1:bad-url".to_owned(),
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let err = fetcher
            .fetch(&reference, &platform, &cache)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FeatureError::FetchFailed { .. }),
            "expected FetchFailed, got {err:?}",
        );
    }

    // -----------------------------------------------------------------------
    // HttpFetcher: wrong reference type is rejected
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn http_fetcher_rejects_non_http_reference() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());

        let fetcher = HttpFetcher;
        let reference = NormalizedRef::LocalTarget {
            absolute_path: PathBuf::from("/tmp/nope"),
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let err = fetcher
            .fetch(&reference, &platform, &cache)
            .await
            .unwrap_err();
        assert!(matches!(err, FeatureError::InvalidReference { .. }));
    }

    // -----------------------------------------------------------------------
    // LocalFetcher: valid directory with metadata
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn local_existing_directory_with_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let feature_dir = dir.path().join("my-feature");
        std::fs::create_dir_all(&feature_dir).unwrap();
        std::fs::write(
            feature_dir.join("devcontainer-feature.json"),
            r#"{"id":"my-feature","version":"1.0.0"}"#,
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let fetcher = LocalFetcher;
        let reference = NormalizedRef::LocalTarget {
            absolute_path: feature_dir.clone(),
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let result = fetcher.fetch(&reference, &platform, &cache).await.unwrap();
        assert_eq!(result, feature_dir);
    }

    // -----------------------------------------------------------------------
    // LocalFetcher: missing directory
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn local_missing_directory_returns_not_found() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let fetcher = LocalFetcher;
        let reference = NormalizedRef::LocalTarget {
            absolute_path: PathBuf::from("/nonexistent/feature/path"),
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let err = fetcher
            .fetch(&reference, &platform, &cache)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FeatureError::LocalFeatureNotFound { .. }),
            "expected LocalFeatureNotFound, got {err:?}",
        );
    }

    // -----------------------------------------------------------------------
    // LocalFetcher: directory without devcontainer-feature.json
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn local_directory_without_metadata_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let feature_dir = dir.path().join("empty-feature");
        std::fs::create_dir_all(&feature_dir).unwrap();
        // No devcontainer-feature.json created.

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let fetcher = LocalFetcher;
        let reference = NormalizedRef::LocalTarget {
            absolute_path: feature_dir,
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let err = fetcher
            .fetch(&reference, &platform, &cache)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FeatureError::InvalidMetadata { .. }),
            "expected InvalidMetadata, got {err:?}",
        );
    }

    // -----------------------------------------------------------------------
    // LocalFetcher: wrong reference type is rejected
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn local_fetcher_rejects_non_local_reference() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let fetcher = LocalFetcher;
        let reference = NormalizedRef::HttpTarget {
            url: "https://example.com/feat.tgz".to_owned(),
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let err = fetcher
            .fetch(&reference, &platform, &cache)
            .await
            .unwrap_err();
        assert!(matches!(err, FeatureError::InvalidReference { .. }));
    }

    // -----------------------------------------------------------------------
    // MockFetcher: returns configured path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mock_returns_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let expected_path = dir.path().join("mock-feature");
        std::fs::create_dir_all(&expected_path).unwrap();

        let mut responses = HashMap::new();
        responses.insert(
            "https://example.com/feat.tgz".to_owned(),
            expected_path.clone(),
        );

        let fetcher = MockFetcher { responses };

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let reference = NormalizedRef::HttpTarget {
            url: "https://example.com/feat.tgz".to_owned(),
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let result = fetcher.fetch(&reference, &platform, &cache).await.unwrap();
        assert_eq!(result, expected_path);
    }

    // -----------------------------------------------------------------------
    // MockFetcher: missing response returns error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mock_missing_response_returns_error() {
        let fetcher = MockFetcher {
            responses: HashMap::new(),
        };

        let cache_dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(cache_dir.path());

        let reference = NormalizedRef::HttpTarget {
            url: "https://example.com/unknown.tgz".to_owned(),
        };
        let platform = Platform {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        };

        let err = fetcher
            .fetch(&reference, &platform, &cache)
            .await
            .unwrap_err();
        assert!(matches!(err, FeatureError::FetchFailed { .. }));
    }
}
