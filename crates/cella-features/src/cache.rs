//! Persistent on-disk cache for downloaded devcontainer features.
//!
//! OCI features are stored by registry/repository/digest.  HTTP tarball
//! features are stored under a truncated SHA-256 of the URL.  Build
//! contexts live under a separate `builds/` subtree keyed by config hash.
//!
//! All writes go through a staging directory (`{name}.partial-{pid}`) and
//! are committed with an atomic `rename()`.  This makes concurrent fetches
//! of the same feature safe: the first process to finish wins, and later
//! renames simply overwrite the (identical) directory.

use std::io;
use std::path::{Path, PathBuf};
use std::process;

use sha2::{Digest, Sha256};

/// On-disk feature cache rooted at a platform-appropriate location.
#[derive(Debug, Clone)]
pub struct FeatureCache {
    root: PathBuf,
}

impl FeatureCache {
    /// Create a cache rooted at the platform default location.
    ///
    /// Uses `dirs::cache_dir()/cella/features/` when available, falling
    /// back to `/tmp/cella-features-cache` when the platform cache
    /// directory cannot be determined.
    pub fn new() -> Self {
        let root = dirs::cache_dir().map_or_else(
            || PathBuf::from("/tmp/cella-features-cache"),
            |d| d.join("cella").join("features"),
        );
        Self { root }
    }

    /// Create a cache rooted at an explicit path (useful for testing).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Check whether an OCI feature layer is already cached.
    ///
    /// Returns `Some(path)` when the directory exists, `None` otherwise.
    pub fn get_oci(&self, registry: &str, repository: &str, digest: &str) -> Option<PathBuf> {
        let path = self.oci_path(registry, repository, digest);
        path.is_dir().then_some(path)
    }

    /// Compute the cache path for an OCI feature layer.
    pub fn oci_path(&self, registry: &str, repository: &str, digest: &str) -> PathBuf {
        self.root
            .join("oci")
            .join(registry)
            .join(repository)
            .join(digest)
    }

    /// Check whether a URL-fetched feature tarball is already cached.
    ///
    /// Returns `Some(path)` when the directory exists, `None` otherwise.
    pub fn get_url(&self, url: &str) -> Option<PathBuf> {
        let path = self.url_path(url);
        path.is_dir().then_some(path)
    }

    /// Compute the cache path for a URL-fetched feature.
    ///
    /// The path is derived from the first 16 hex characters of the
    /// SHA-256 digest of the URL.
    pub fn url_path(&self, url: &str) -> PathBuf {
        let hash = hex::encode(Sha256::digest(url.as_bytes()));
        let short = &hash[..16];
        self.root.join("urls").join(short)
    }

    /// Compute the path for a build context identified by `config_hash`.
    pub fn build_context_path(&self, config_hash: &str) -> PathBuf {
        // Build contexts live one level up from the features root,
        // alongside it under the cella cache directory.
        self.root
            .parent()
            .unwrap_or(&self.root)
            .join("builds")
            .join(config_hash)
    }

    /// Return a staging path for the given final path.
    ///
    /// The staging name is `{final_name}.partial-{pid}` placed alongside
    /// the final path.  The caller should write into this directory, then
    /// call [`commit`](Self::commit) to atomically swap it into place.
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
    /// Uses `fs::rename` which is atomic on POSIX.  If the final path
    /// already exists (another process won the race), the staging
    /// directory is removed and the existing entry is kept.
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
                // Another process won the race.  Clean up our staging dir.
                let _ = std::fs::remove_dir_all(staging);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Default for FeatureCache {
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

    // -----------------------------------------------------------------------
    // get_oci: miss and hit
    // -----------------------------------------------------------------------

    #[test]
    fn cache_miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        assert!(
            cache
                .get_oci("ghcr.io", "devcontainers/features/node", "sha256-abc")
                .is_none()
        );
    }

    #[test]
    fn cache_hit_returns_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.oci_path("ghcr.io", "devcontainers/features/node", "sha256-abc");
        std::fs::create_dir_all(&path).unwrap();
        let hit = cache.get_oci("ghcr.io", "devcontainers/features/node", "sha256-abc");
        assert_eq!(hit, Some(path));
    }

    // -----------------------------------------------------------------------
    // url_path: determinism and collision resistance
    // -----------------------------------------------------------------------

    #[test]
    fn url_path_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let a = cache.url_path("https://example.com/feature.tgz");
        let b = cache.url_path("https://example.com/feature.tgz");
        assert_eq!(a, b);
    }

    #[test]
    fn different_urls_produce_different_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let a = cache.url_path("https://example.com/feature-a.tgz");
        let b = cache.url_path("https://example.com/feature-b.tgz");
        assert_ne!(a, b);
    }

    // -----------------------------------------------------------------------
    // get_url: miss and hit
    // -----------------------------------------------------------------------

    #[test]
    fn url_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        assert!(cache.get_url("https://example.com/feat.tgz").is_none());
    }

    #[test]
    fn url_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path());
        let path = cache.url_path("https://example.com/feat.tgz");
        std::fs::create_dir_all(&path).unwrap();
        assert_eq!(cache.get_url("https://example.com/feat.tgz"), Some(path));
    }

    // -----------------------------------------------------------------------
    // staging_path
    // -----------------------------------------------------------------------

    #[test]
    fn staging_path_includes_pid() {
        let final_path = PathBuf::from("/cache/oci/ghcr.io/features/node/sha256-abc");
        let staging = FeatureCache::staging_path(&final_path);
        let name = staging.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains(&format!(".partial-{}", process::id())),
            "staging name should include PID: {name}",
        );
    }

    #[test]
    fn staging_path_is_sibling_of_final() {
        let final_path = PathBuf::from("/cache/oci/ghcr.io/features/node/sha256-abc");
        let staging = FeatureCache::staging_path(&final_path);
        assert_eq!(staging.parent(), final_path.parent());
    }

    // -----------------------------------------------------------------------
    // commit: success and race
    // -----------------------------------------------------------------------

    #[test]
    fn atomic_commit_success() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir
            .path()
            .join("oci")
            .join("ghcr.io")
            .join("features")
            .join("sha256-abc");
        let staging = FeatureCache::staging_path(&final_path);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("marker"), b"ok").unwrap();

        FeatureCache::commit(&staging, &final_path).unwrap();

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
        // Simulate the winner: final_path already exists.
        std::fs::create_dir_all(&final_path).unwrap();
        std::fs::write(final_path.join("winner"), b"first").unwrap();

        // Our staging dir has different content.
        let staging = FeatureCache::staging_path(&final_path);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("loser"), b"second").unwrap();

        // commit should succeed (loser cleans up).
        FeatureCache::commit(&staging, &final_path).unwrap();

        // Winner's content is preserved.
        assert_eq!(
            std::fs::read_to_string(final_path.join("winner")).unwrap(),
            "first"
        );
        // Staging dir cleaned up.
        assert!(!staging.exists());
    }

    // -----------------------------------------------------------------------
    // build_context_path
    // -----------------------------------------------------------------------

    #[test]
    fn build_context_path_under_builds() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FeatureCache::with_root(dir.path().join("features"));
        let path = cache.build_context_path("abc123");
        assert!(path.ends_with("builds/abc123"));
        // Should be a sibling of the features root, not nested inside it.
        assert!(!path.starts_with(dir.path().join("features")));
    }
}
