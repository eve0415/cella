//! On-disk cache for a repository's published tag list.
//!
//! Tag lists are large (MCR's `devcontainers/base` publishes ~1900) and
//! change slowly, so they are cached for an hour. The stale accessor exists
//! for the offline path, where an expired list beats no answer at all.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use tracing::debug;

/// How long a cached tag list is considered fresh.
const TAG_TTL: Duration = Duration::from_hours(1);

/// On-disk cache of published tag lists, keyed by image reference.
#[derive(Debug, Clone)]
pub struct TagCache {
    root: PathBuf,
}

impl TagCache {
    /// Create a cache rooted at the platform default location.
    ///
    /// Uses `dirs::cache_dir()/cella/oci/tags/` when available, falling back
    /// to `/tmp/cella-oci-tags`.
    pub fn new() -> Self {
        let root = dirs::cache_dir().map_or_else(
            || PathBuf::from("/tmp/cella-oci-tags"),
            |d| d.join("cella").join("oci").join("tags"),
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

    /// Read cached tags if they exist and are fresh (< 1h old).
    pub fn get(&self, reference: &str) -> Option<Vec<String>> {
        let path = self.path_for(reference);
        let modified = std::fs::metadata(&path).ok()?.modified().ok()?;

        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if age > TAG_TTL {
            debug!("tag cache expired for {reference} (age: {age:?})");
            return None;
        }

        let tags = read_entry(&path)?;
        debug!("tag cache hit for {reference}");
        Some(tags)
    }

    /// Read cached tags regardless of age.
    ///
    /// Used as a fallback when the registry is unreachable, where stale data
    /// beats no data.
    pub fn get_stale(&self, reference: &str) -> Option<Vec<String>> {
        let tags = read_entry(&self.path_for(reference))?;
        debug!("stale tag cache hit for {reference}");
        Some(tags)
    }

    /// Write a tag list to the cache.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the cache directory or file cannot be written.
    pub fn put(&self, reference: &str, tags: &[String]) -> io::Result<()> {
        let path = self.path_for(reference);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(tags).map_err(io::Error::other)?;
        std::fs::write(&path, json)?;
        debug!("cached tags for {reference} at {}", path.display());
        Ok(())
    }

    /// Compute the cache file path for an image reference.
    ///
    /// The reference is hashed so that its slashes and colons cannot become
    /// path components.
    fn path_for(&self, reference: &str) -> PathBuf {
        let prefixed = format!("tags::{reference}");
        let hash = hex::encode(&Sha256::digest(prefixed.as_bytes())[..8]);
        self.root.join(format!("{hash}.json"))
    }
}

/// Deserialize a cache entry, treating any unreadable or corrupt file as a miss.
fn read_entry(path: &Path) -> Option<Vec<String>> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

impl Default for TagCache {
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

    const REF: &str = "mcr.microsoft.com/devcontainers/rust";

    fn sample() -> Vec<String> {
        vec!["2.0.14-trixie".to_owned(), "2.0.9-trixie".to_owned()]
    }

    #[test]
    fn round_trips_tags_and_misses_on_unknown_reference() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());

        cache.put(REF, &sample()).unwrap();
        assert_eq!(cache.get(REF), Some(sample()));
        assert_eq!(cache.get("mcr.microsoft.com/devcontainers/other"), None);
        assert_eq!(
            cache.get_stale("mcr.microsoft.com/devcontainers/other"),
            None
        );
    }

    #[test]
    fn expired_entries_miss_fresh_reads_but_still_serve_stale_reads() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());
        cache.put(REF, &sample()).unwrap();

        // Backdate past the TTL — without this the test cannot tell `get`
        // and `get_stale` apart.
        let old = filetime::FileTime::from_unix_time(0, 0);
        filetime::set_file_mtime(cache.path_for(REF), old).unwrap();

        assert_eq!(cache.get(REF), None, "expired entry must miss");
        assert_eq!(
            cache.get_stale(REF),
            Some(sample()),
            "expired entry must still be readable as stale"
        );
    }

    #[test]
    fn distinct_references_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());
        assert_ne!(cache.path_for("ghcr.io/a/b"), cache.path_for("ghcr.io/a/c"));
    }

    #[test]
    fn corrupt_entries_are_treated_as_misses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TagCache::with_root(dir.path());
        cache.put(REF, &sample()).unwrap();
        std::fs::write(cache.path_for(REF), b"{not json").unwrap();

        assert_eq!(cache.get(REF), None);
        assert_eq!(cache.get_stale(REF), None);
    }
}
