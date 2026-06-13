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
const COLLECTION_TTL: Duration = Duration::from_hours(24);

/// How long cached image tags are considered fresh.
const IMAGE_TAG_TTL: Duration = Duration::from_hours(1);

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

    /// Compute the cache file path for a feature collection index.
    ///
    /// Uses a `feature::` prefix before hashing to avoid collisions with
    /// template collection entries that share the same registry string.
    fn feature_collection_path(&self, registry: &str) -> PathBuf {
        let prefixed = format!("feature::{registry}");
        let hash = hex::encode(&Sha256::digest(prefixed.as_bytes())[..8]);
        self.root.join("collections").join(format!("{hash}.json"))
    }

    /// Get a cached feature collection index if it exists and is fresh (< 24h old).
    pub fn get_feature_collection(
        &self,
        registry: &str,
    ) -> Option<(FeatureCollectionIndex, SystemTime)> {
        let path = self.feature_collection_path(registry);
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
        let path = self.feature_collection_path(registry);
        let metadata = std::fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?;
        let content = std::fs::read_to_string(&path).ok()?;
        let index: FeatureCollectionIndex = serde_json::from_str(&content).ok()?;
        debug!("feature collection stale cache hit for {registry}");
        Some((index, modified))
    }

    /// Write a feature collection index to the cache.
    ///
    /// # Errors
    ///
    /// Returns [`TemplateError::CacheError`] if the write fails.
    pub fn put_feature_collection(
        &self,
        registry: &str,
        index: &FeatureCollectionIndex,
    ) -> Result<(), TemplateError> {
        let path = self.feature_collection_path(registry);
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

    // -----------------------------------------------------------------------
    // Image tag cache (1-hour TTL)
    // -----------------------------------------------------------------------

    /// Get cached image tags if they exist and are fresh (< 1h old).
    pub fn get_image_tags(&self, image_ref: &str) -> Option<Vec<String>> {
        let path = self.image_tags_path(image_ref);
        let metadata = std::fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?;

        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if age > IMAGE_TAG_TTL {
            debug!("image tag cache expired for {image_ref} (age: {age:?})");
            return None;
        }

        let content = std::fs::read_to_string(&path).ok()?;
        let tags: Vec<String> = serde_json::from_str(&content).ok()?;
        debug!("image tag cache hit for {image_ref}");
        Some(tags)
    }

    /// Write image tags to the cache.
    ///
    /// # Errors
    ///
    /// Returns [`TemplateError::CacheError`] if the write fails.
    pub fn put_image_tags(&self, image_ref: &str, tags: &[String]) -> Result<(), TemplateError> {
        let path = self.image_tags_path(image_ref);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TemplateError::CacheError {
                message: format!("failed to create cache directory: {e}"),
            })?;
        }
        let json = serde_json::to_string(tags).map_err(|e| TemplateError::CacheError {
            message: format!("failed to serialize image tags: {e}"),
        })?;
        std::fs::write(&path, json).map_err(|e| TemplateError::CacheError {
            message: format!("failed to write image tag cache: {e}"),
        })?;
        debug!("cached image tags for {image_ref} at {}", path.display());
        Ok(())
    }

    /// Compute the cache file path for image tags.
    fn image_tags_path(&self, image_ref: &str) -> PathBuf {
        let prefixed = format!("tags::{image_ref}");
        let hash = hex::encode(&Sha256::digest(prefixed.as_bytes())[..8]);
        self.root.join("tags").join(format!("{hash}.json"))
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
    ///
    /// Each ref component is hashed before being used as a path segment so
    /// that crafted values containing `..`, absolute paths, or embedded
    /// separators cannot escape the cache root.  This changes the on-disk
    /// layout; any existing unhashed entries are treated as misses.
    pub fn template_path(&self, registry: &str, repository: &str, digest: &str) -> PathBuf {
        self.root
            .join("oci")
            .join(hash_ref_component(registry))
            .join(hash_ref_component(repository))
            .join(hash_ref_component(digest))
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

/// Hash an untrusted OCI ref component (registry, repository, or digest) to a
/// fixed-length hex string safe for use as a single filesystem path segment.
///
/// SHA-256 over the raw bytes produces a 64-character hex string that contains
/// no path separators, no `..` sequences, and no leading dots, regardless of
/// the input.  This is the same approach used by the collection-index and
/// image-tag path helpers.
fn hash_ref_component(component: &str) -> String {
    hex::encode(Sha256::digest(component.as_bytes()))
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

    /// Assert that `path` is strictly contained under `root` with no traversal
    /// components and the expected `oci/<reg>/<repo>/<dig>` shape.
    ///
    /// `Path::starts_with` is a lexical prefix check and returns `true` for
    /// `root/oci/../../escape`, so it cannot prove containment on its own.
    /// This helper additionally:
    ///   - rejects any `..` or `.` components in the suffix past root
    ///   - rejects any `RootDir` component in the suffix (absolute injection)
    ///   - asserts the suffix is exactly 4 segments: `oci`, reg-hash,
    ///     repo-hash, digest-hash
    fn assert_oci_path_shape(root: &Path, path: &Path) {
        use std::path::Component;

        // Strip the root prefix — this also verifies the path shares the root.
        let suffix = path
            .strip_prefix(root)
            .unwrap_or_else(|_| panic!("path {path:?} does not start under root {root:?}"));

        let components: Vec<_> = suffix.components().collect();

        // Exactly 4 segments past root: "oci", reg-hash, repo-hash, dig-hash.
        assert_eq!(
            components.len(),
            4,
            "expected exactly 4 path components past root (oci/<reg>/<repo>/<dig>), \
             got {}: {path:?}",
            components.len(),
        );

        for c in &components {
            match c {
                Component::Normal(_) => {}
                Component::ParentDir => {
                    panic!("path contains `..` traversal component: {path:?}");
                }
                Component::CurDir => {
                    panic!("path contains `.` component: {path:?}");
                }
                Component::RootDir | Component::Prefix(_) => {
                    panic!("path contains absolute injection component: {path:?}");
                }
            }
        }

        assert_eq!(
            components[0],
            Component::Normal(std::ffi::OsStr::new("oci")),
            "first segment must be \"oci\": {path:?}",
        );

        // Each of the three hash segments must be a 64-character hex string.
        for (i, c) in components[1..].iter().enumerate() {
            let Component::Normal(seg) = c else {
                unreachable!();
            };
            let s = seg.to_string_lossy();
            assert_eq!(
                s.len(),
                64,
                "hash segment {i} must be 64 hex chars, got {}: {path:?}",
                s.len(),
            );
            assert!(
                s.chars().all(|ch| ch.is_ascii_hexdigit()),
                "hash segment {i} contains non-hex characters: {path:?}",
            );
        }
    }

    #[test]
    fn template_path_dotdot_registry_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let path = cache.template_path("../../escape", "repo", "sha256:abc");
        assert_oci_path_shape(dir.path(), &path);
    }

    #[test]
    fn template_path_dotdot_repository_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let path = cache.template_path("ghcr.io", "../../escape", "sha256:abc");
        assert_oci_path_shape(dir.path(), &path);
    }

    #[test]
    fn template_path_dotdot_digest_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let path = cache.template_path("ghcr.io", "repo", "../../escape");
        assert_oci_path_shape(dir.path(), &path);
    }

    #[test]
    fn template_path_absolute_registry_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let path = cache.template_path("/etc/passwd", "repo", "sha256:abc");
        assert_oci_path_shape(dir.path(), &path);
    }

    #[test]
    fn template_path_embedded_separator_in_repository() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        // OCI repos legitimately contain slashes (e.g. org/image/name).
        // After hashing the whole component, the path stays flat.
        let path = cache.template_path("ghcr.io", "devcontainers/templates/rust", "sha256:abc");
        assert_oci_path_shape(dir.path(), &path);
    }

    /// Regression: empty ref components must still produce the 4-segment
    /// `oci/<reg>/<repo>/<dig>` shape and must not collapse path depth.
    #[test]
    fn template_path_empty_components_preserve_depth() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let path = cache.template_path("", "", "");
        assert_oci_path_shape(dir.path(), &path);
        // Each empty string hashes to a distinct 64-char hex value, so all
        // three hashes are equal (SHA-256("") is the same each time) but still
        // present — no segment collapses.
        let depth = path.components().count();
        let root_depth = dir.path().components().count();
        assert_eq!(depth - root_depth, 4, "expected oci/<reg>/<repo>/<dig>");
    }

    #[test]
    fn template_path_distinct_repos_produce_distinct_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let a = cache.template_path("ghcr.io", "devcontainers/templates/rust", "sha256:abc");
        let b = cache.template_path("ghcr.io", "devcontainers/templates/python", "sha256:abc");
        assert_ne!(a, b);
    }

    #[test]
    fn template_path_same_inputs_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let a = cache.template_path("ghcr.io", "devcontainers/templates/rust", "sha256:abc");
        let b = cache.template_path("ghcr.io", "devcontainers/templates/rust", "sha256:abc");
        assert_eq!(a, b);
    }

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

    // -----------------------------------------------------------------------
    // Feature collection cache: miss, hit, expiry, empty
    // -----------------------------------------------------------------------

    fn sample_feature_collection() -> FeatureCollectionIndex {
        serde_json::from_str(
            r#"{
            "features": [
                { "id": "node", "version": "1.5.0", "name": "Node.js" },
                { "id": "python", "version": "2.0.0", "name": "Python" }
            ]
        }"#,
        )
        .unwrap()
    }

    #[test]
    fn feature_collection_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        assert!(cache.get_feature_collection("ghcr.io/features").is_none());
    }

    #[test]
    fn feature_collection_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let index = sample_feature_collection();

        cache
            .put_feature_collection("ghcr.io/features", &index)
            .unwrap();
        let (cached, _) = cache.get_feature_collection("ghcr.io/features").unwrap();
        assert_eq!(cached.features.len(), 2);
        assert_eq!(cached.features[0].id, "node");
        assert_eq!(cached.features[1].id, "python");
    }

    #[test]
    fn feature_collection_stale_cache_returns_expired() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let index = sample_feature_collection();

        cache
            .put_feature_collection("ghcr.io/features", &index)
            .unwrap();

        // Backdate to simulate expiry
        let path = cache.feature_collection_path("ghcr.io/features");
        let old_time = filetime::FileTime::from_unix_time(0, 0);
        filetime::set_file_mtime(&path, old_time).unwrap();

        // Fresh cache should miss
        assert!(cache.get_feature_collection("ghcr.io/features").is_none());

        // Stale cache should still hit
        let (stale, _) = cache
            .get_feature_collection_stale("ghcr.io/features")
            .unwrap();
        assert_eq!(stale.features.len(), 2);
        assert_eq!(stale.features[0].id, "node");
    }

    #[test]
    fn feature_collection_empty_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());
        let index: FeatureCollectionIndex = serde_json::from_str(r#"{ "features": [] }"#).unwrap();

        cache
            .put_feature_collection("ghcr.io/empty", &index)
            .unwrap();
        let (cached, _) = cache.get_feature_collection("ghcr.io/empty").unwrap();
        assert!(cached.features.is_empty());
    }

    #[test]
    fn template_cache_unaffected_by_feature_methods() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TemplateCache::with_root(dir.path());

        // Write both types
        cache
            .put_collection("ghcr.io/templates", &sample_collection())
            .unwrap();
        cache
            .put_feature_collection("ghcr.io/features", &sample_feature_collection())
            .unwrap();

        // Template cache still works
        let (tc, _) = cache.get_collection("ghcr.io/templates").unwrap();
        assert_eq!(tc.templates.len(), 1);
        assert_eq!(tc.templates[0].id, "rust");

        // Feature cache still works
        let (fc, _) = cache.get_feature_collection("ghcr.io/features").unwrap();
        assert_eq!(fc.features.len(), 2);
        assert_eq!(fc.features[0].id, "node");
    }
}
