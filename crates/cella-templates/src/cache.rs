//! Persistent on-disk cache for template collection indexes and artifacts.
//!
//! Collection indexes are cached with a 24-hour TTL.  Individual template
//! artifacts (OCI tarballs) are cached by registry/repository/digest, using
//! the same atomic staging pattern as `cella-features`.

use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use tracing::debug;

use crate::error::TemplateError;
use crate::{FeatureCollectionIndex, TemplateCollectionIndex};

/// How long a cached collection index is considered fresh.
const COLLECTION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// On-disk cache for template collections and artifacts.
#[derive(Debug, Clone)]
pub struct TemplateCache {
    root: PathBuf,
}

impl TemplateCache {
    /// Create a cache rooted at the platform default location.
    ///
    /// Uses `dirs::cache_dir()/cella/templates/` when available, falling
    /// back to `/tmp/cella-templates-cache`.
    pub fn new() -> Self {
        let root = dirs::cache_dir().map_or_else(
            || PathBuf::from("/tmp/cella-templates-cache"),
            |d| d.join("cella").join("templates"),
        );
        Self { root }
    }

    /// Create a cache at an explicit path (useful for testing).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Return the cache root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    // -----------------------------------------------------------------------
    // Collection index cache
    // -----------------------------------------------------------------------

    /// Get a cached collection index if it exists and is fresh (< 24h old).
    ///
    /// Returns `Some((index, modified_time))` on hit, `None` on miss or
    /// expiry.
    pub fn get_collection(&self, registry: &str) -> Option<(TemplateCollectionIndex, SystemTime)> {
        let path = self.collection_path(registry);
        let metadata = std::fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?;

        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if age > COLLECTION_TTL {
            debug!("collection cache expired for {registry} (age: {age:?})");
            return None;
        }

        let content = std::fs::read_to_string(&path).ok()?;
        let index: TemplateCollectionIndex = serde_json::from_str(&content).ok()?;
        debug!("collection cache hit for {registry}");
        Some((index, modified))
    }

    /// Get a cached collection index even if expired (for offline fallback).
    ///
    /// Returns `Some((index, modified_time))` if the file exists at all.
    pub fn get_collection_stale(
        &self,
        registry: &str,
    ) -> Option<(TemplateCollectionIndex, SystemTime)> {
        let path = self.collection_path(registry);
        let metadata = std::fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        let index: TemplateCollectionIndex = serde_json::from_str(&content).ok()?;
        debug!("collection stale cache hit for {registry}");
        Some((index, modified))
    }

    /// Write a collection index to the cache.
    ///
    /// # Errors
    ///
    /// Returns [`TemplateError::CacheError`] if the write fails.
    pub fn put_collection(
        &self,
        registry: &str,
        index: &TemplateCollectionIndex,
    ) -> Result<(), TemplateError> {
        let path = self.collection_path(registry);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TemplateError::CacheError {
                message: format!("failed to create cache directory: {e}"),
            })?;
        }
        let json = serde_json::to_string(index).map_err(|e| TemplateError::CacheError {
            message: format!("failed to serialize collection index: {e}"),
        })?;
        std::fs::write(&path, json).map_err(|e| TemplateError::CacheError {
            message: format!("failed to write collection cache: {e}"),
        })?;
        debug!(
            "cached collection index for {registry} at {}",
            path.display()
        );
        Ok(())
    }

    /// Compute the cache file path for a collection index.
    fn collection_path(&self, registry: &str) -> PathBuf {
        let hash = hex::encode(&Sha256::digest(registry.as_bytes())[..8]);
        self.root.join("collections").join(format!("{hash}.json"))
    }

    // -----------------------------------------------------------------------
    // Feature collection index cache
    // -----------------------------------------------------------------------

    /// Get a cached feature collection index if it exists and is fresh (< 24h old).
    pub fn get_feature_collection(
        &self,
        registry: &str,
    ) -> Option<(FeatureCollectionIndex, SystemTime)> {
        let path = self.collection_path(registry);
        let metadata = std::fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?;

        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if age > COLLECTION_TTL {
            debug!("feature collection cache expired for {registry} (age: {age:?})");
            return None;
        }

        let content = std::fs::read_to_string(&path).ok()?;
        let index: FeatureCollectionIndex = serde_json::from_str(&content).ok()?;
        debug!("feature collection cache hit for {registry}");
        Some((index, modified))
    }

    /// Get a cached feature collection index even if expired (for offline fallback).
    pub fn get_feature_collection_stale(
        &self,
        registry: &str,
    ) -> Option<(FeatureCollectionIndex, SystemTime)> {
        let path = self.collection_path(registry);
        let metadata = std::fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        let index: FeatureCollectionIndex = serde_json::from_str(&content).ok()?;
        debug!("feature collection stale cache hit for {registry}");
        Some((index, modified))
    }

    /// Write a feature collection index to the cache.
    pub fn put_feature_collection(
        &self,
        registry: &str,
        index: &FeatureCollectionIndex,
    ) -> Result<(), TemplateError> {
        let path = self.collection_path(registry);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TemplateError::CacheError {
                message: format!("failed to create cache directory: {e}"),
            })?;
        }
        let json = serde_json::to_string(index).map_err(|e| TemplateError::CacheError {
            message: format!("failed to serialize feature collection index: {e}"),
        })?;
        std::fs::write(&path, json).map_err(|e| TemplateError::CacheError {
            message: format!("failed to write feature collection cache: {e}"),
        })?;
        debug!(
            "cached feature collection index for {registry} at {}",
            path.display()
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Template artifact cache
    // -----------------------------------------------------------------------

    /// Check whether a template artifact is cached.
    pub fn get_template(&self, registry: &str, repository: &str, digest: &str) -> Option<PathBuf> {
        let path = self.template_path(registry, repository, digest);
        path.is_dir().then_some(path)
    }

    /// Compute the cache path for a template artifact.
    pub fn template_path(&self, registry: &str, repository: &str, digest: &str) -> PathBuf {
        self.root
            .join("oci")
            .join(registry)
            .join(repository)
            .join(digest)
    }

    /// Return a staging path for atomic writes.
    pub fn staging_path(final_path: &Path) -> PathBuf {
        let name = final_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staging_name = format!("{name}.partial-{}", process::id());
        final_path.with_file_name(staging_name)
    }

    /// Atomically commit a staging directory to its final location.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if both the rename and the cleanup of the
    /// staging directory fail.
    pub fn commit(staging: &Path, final_path: &Path) -> io::Result<()> {
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::rename(staging, final_path) {
            Ok(()) => Ok(()),
            Err(_) if final_path.exists() => {
                let _ = std::fs::remove_dir_all(staging);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Default for TemplateCache {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_collection() -> TemplateCollectionIndex {
        serde_json::from_str(
            r#"{
            "templates": [
                { "id": "rust", "version": "5.0.0", "name": "Rust" }
            ]
        }"#,
        )
        .unwrap()
    }

    // -----------------------------------------------------------------------
    // Collection cache: miss, hit, expiry
    // -----------------------------------------------------------------------

    #[test]
    fn collection_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        assert!(cache.get_collection("ghcr.io").is_none());
    }

    #[test]
    fn collection_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let index = sample_collection();

        cache.put_collection("ghcr.io", &index).unwrap();
        let (cached, _) = cache.get_collection("ghcr.io").unwrap();
        assert_eq!(cached.templates.len(), 1);
        assert_eq!(cached.templates[0].id, "rust");
    }

    #[test]
    fn collection_stale_cache_returns_expired() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let index = sample_collection();

        cache.put_collection("ghcr.io", &index).unwrap();

        // Manually backdate the file to simulate expiry
        let path = cache.collection_path("ghcr.io");
        let old_time = filetime::FileTime::from_unix_time(0, 0);
        filetime::set_file_mtime(&path, old_time).unwrap();

        // Fresh cache should miss
        assert!(cache.get_collection("ghcr.io").is_none());

        // Stale cache should still hit
        let (stale, _) = cache.get_collection_stale("ghcr.io").unwrap();
        assert_eq!(stale.templates[0].id, "rust");
    }

    // -----------------------------------------------------------------------
    // Template artifact cache
    // -----------------------------------------------------------------------

    #[test]
    fn template_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        assert!(
            cache
                .get_template("ghcr.io", "devcontainers/templates/rust", "sha256:abc")
                .is_none()
        );
    }

    #[test]
    fn template_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let path = cache.template_path("ghcr.io", "devcontainers/templates/rust", "sha256:abc");
        std::fs::create_dir_all(&path).unwrap();
        assert_eq!(
            cache.get_template("ghcr.io", "devcontainers/templates/rust", "sha256:abc"),
            Some(path),
        );
    }

    // -----------------------------------------------------------------------
    // Staging + commit
    // -----------------------------------------------------------------------

    #[test]
    fn staging_path_includes_pid() {
        let path = PathBuf::from("/cache/templates/sha256-abc");
        let staging = TemplateCache::staging_path(&path);
        let name = staging.file_name().unwrap().to_string_lossy();
        assert!(name.contains(&format!(".partial-{}", process::id())));
    }

    #[test]
    fn atomic_commit() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("target");
        let staging = TemplateCache::staging_path(&final_path);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("marker"), b"ok").unwrap();

        TemplateCache::commit(&staging, &final_path).unwrap();
        assert!(final_path.is_dir());
        assert_eq!(
            std::fs::read_to_string(final_path.join("marker")).unwrap(),
            "ok"
        );
        assert!(!staging.exists());
    }

    #[test]
    fn atomic_commit_race_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("target");
        std::fs::create_dir_all(&final_path).unwrap();
        std::fs::write(final_path.join("winner"), b"first").unwrap();

        let staging = TemplateCache::staging_path(&final_path);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("loser"), b"second").unwrap();

        TemplateCache::commit(&staging, &final_path).unwrap();
        assert_eq!(
            std::fs::read_to_string(final_path.join("winner")).unwrap(),
            "first"
        );
        assert!(!staging.exists());
    }
}
